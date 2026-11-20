//! 일정에서 회의 제목 · 참석자 가져오기 — PC 의 클래식 Outlook 일정을 COM 으로 읽는다 (로그인 · 앱 등록 없음).
//! 설정에서 켰을 때만.
//!
//! - 녹음을 시작할 때 지금 시각 앞뒤의 일정(시작 15분 전 ~ 끝날 때)이 있으면 그 제목을 회의 제목으로, 참석자를
//!   화자 이름 후보로 쓴다. 회의 시작 알림에도 일정 제목을 보인다.
//! - **Outlook 을 띄우지 않는다**: 이미 켜져 있는 Outlook 에만 붙는다(`GetActiveObject`). `CoCreateInstance` 는 꺼져 있는
//!   Outlook 을 화면 없이 띄우고, 계정을 만든 적 없는 PC 에서는 계정 추가 창까지 띄운다. 꺼져 있으면 조용히 건너뛰고
//!   설정에 그렇다고 보인다.
//! - **보안 경고를 내지 않는다**: 참석자 · 주최자(주소록 정보)는 Outlook 개체 모델 보호 대상이라, 백신이 꺼져 있거나
//!   오래됐거나 회사 정책 · "늘 경고" 설정이면 주소 정보 접근 경고 창이 뜬다. Outlook 이 바깥 프로그램에 쓰는 것과 같은
//!   기준 — 프로그래밍 방식 액세스 설정 · 정책과 Windows 보안 센터의 백신 상태 — 으로 경고가 뜨지 않을 때만 참석자를
//!   읽고(`people_allowed`), 아니면 제목 · 시각만 쓴다. (`Application.IsTrusted` 는 바깥 프로그램에는 늘 거짓이라 못 쓴다)
//! - 새 Outlook(olk.exe)은 COM 을 내놓지 않아 읽을 수 없다 — 클래식 Outlook 이 없으면 그렇다고 보인다.
//! - COM 은 따로 스레드 하나(STA)에서 — Outlook 이 바쁘거나 답하지 않아도 녹음 시작 · 회의 알림을 붙잡지 않게 짧게만 기다린다.

use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use chrono::{Local, NaiveDateTime};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Manager};

/// 녹음을 시작하는 때가 일정 시작보다 이만큼 이르거나, 끝난 뒤 이만큼까지면 그 일정으로 본다
const EARLY: i64 = 15;
const LATE: i64 = 5;
/// Outlook 답을 기다리는 시간 (녹음 시작을 붙잡지 않게)
const WAIT: Duration = Duration::from_secs(4);

/// Outlook 일정 하나
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub subject: String,
    /// "2026-11-20 14:00"
    pub start: String,
    pub end: String,
    /// 참석자 이름 (주최자 먼저) — Outlook 이 바깥 프로그램을 믿지 않으면 비어 있다
    pub attendees: Vec<String>,
}

/// 설정 화면에 보일 상태
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Status {
    /// 클래식 Outlook 이 설치돼 있지 않다 (새 Outlook 만 있음 · Outlook 없음)
    NoOutlook,
    /// 설치돼 있지만 켜져 있지 않다 — 띄우지 않고 건너뛴다
    NotRunning,
    /// 읽을 수 있다. `trusted` 가 아니면 참석자는 읽지 않는다 (Outlook 보안 경고가 뜨지 않게)
    #[serde(rename_all = "camelCase")]
    Ready { trusted: bool, now: Option<Event>, next: Option<Event> },
    Error { message: String },
}

/// 일정 시각 (분 단위) — 비교 · 표시용
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Item {
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
}

/// 지금 녹음하는 회의에 맞는 일정 — 시작 15분 전부터 끝나고 5분까지, 하루 종일 · 취소된 일정은 빼고 시작이 지금과 가장 가까운 것
pub fn pick(now: NaiveDateTime, items: &[(Item, bool)]) -> Option<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, (it, skip))| {
            !*skip && it.start - chrono::Duration::minutes(EARLY) <= now && now <= it.end + chrono::Duration::minutes(LATE)
        })
        .min_by_key(|(_, (it, _))| (it.start - now).num_seconds().abs())
        .map(|(i, _)| i)
}

/// Outlook 에 물을 범위 — 오늘 하루에 고르는 여유(시작 15분 전 · 끝나고 5분)를 더한다
/// (자정 근처에 시작한 녹음도 내일 0시 회의 · 방금 0시에 끝난 회의를 찾게)
pub fn query_range(now: NaiveDateTime) -> (NaiveDateTime, NaiveDateTime) {
    let day = now.date().and_hms_opt(0, 0, 0).unwrap_or(now);
    let from = day.min(now - chrono::Duration::minutes(LATE + 1));
    let to = (day + chrono::Duration::days(1)).max(now + chrono::Duration::minutes(EARLY + 1));
    (from, to)
}

/// 다음 일정 (오늘 안, 지금 뒤에 시작)
pub fn next_after(now: NaiveDateTime, items: &[(Item, bool)]) -> Option<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, (it, skip))| !*skip && it.start > now && it.start.date() == now.date())
        .min_by_key(|(_, (it, _))| it.start)
        .map(|(i, _)| i)
}

/// 백신이 켜져 있고 최신인가 — Windows 보안 센터 `AntiVirusProduct.productState` (가운데 바이트 0x10 = 켜짐,
/// 낮은 바이트 0x00 = 최신. 예: Windows Defender 0x061100). 하나라도 그러면 참
pub fn antivirus_good(states: &[u32]) -> bool {
    states.iter().any(|s| (s >> 8) & 0x10 != 0 && s & 0xFF == 0)
}

/// 참석자(주소록 정보)를 읽어도 Outlook 이 경고 창을 띄우지 않나 — Outlook 이 바깥 프로그램을 보는 기준 그대로:
/// - `AdminSecurityMode` 1 · 2(Exchange 공용 폴더의 보안 양식)면 백신 상태를 보지 않으므로 읽지 않는다
/// - 회사 정책(`AdminSecurityMode` = 3)이 주소 정보 접근(`PromptOOMAddressInformationAccess`)을 정했으면 그대로
///   (Microsoft 문서: 0 = 자동 거부 · 1 = 사용자에게 묻기(기본) · 2 = 자동 허용. 3 = 컴퓨터 보안 상태(백신)에 따라 —
///   그룹 정책 서식의 넷째 선택. 그 밖은 읽지 않음)
/// - 아니면 프로그래밍 방식 액세스 설정(`ObjectModelGuard`): 2 = 경고하지 않음 · 1 = 늘 경고 ·
///   0 이나 없음(기본) = 백신이 꺼졌거나 오래됐을 때만 경고
/// 모르는 값은 읽지 않는 쪽으로
pub fn people_allowed(policy_mode: Option<u32>, policy_prompt: Option<u32>, guard: Option<u32>, av_good: bool) -> bool {
    match policy_mode {
        Some(3) => match policy_prompt {
            Some(2) => return true,
            Some(3) => return av_good,
            Some(_) => return false,
            None => {}
        },
        // 1 · 2 = Exchange 공용 폴더의 보안 양식 — Outlook 이 백신 상태를 보지 않아 경고가 뜰 수 있으니 읽지 않는다
        Some(1) | Some(2) => return false,
        _ => {}
    }
    match guard {
        Some(2) => true,
        None | Some(0) => av_good,
        Some(_) => false,
    }
}

/// "김 팀장; 이 개발 (개발팀); park@example.com" → 이름들 (주소만 있으면 @ 앞, 괄호 속 부서는 뗀다, 같은 이름은 한 번)
pub fn attendee_names(lists: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for list in lists {
        for raw in list.split(';') {
            let mut n = raw.trim().trim_matches(['\'', '"']).trim().to_string();
            if let Some(i) = n.find(" (") {
                n.truncate(i);
            }
            if let Some(i) = n.find('<') {
                n = n[..i].trim().to_string();
            }
            if !n.contains(' ') && n.contains('@') {
                n = n.split('@').next().unwrap_or("").to_string();
            }
            let n = n.trim().to_string();
            if !n.is_empty() && !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

// ───────────────────────── Outlook (COM) ─────────────────────────

#[cfg(windows)]
mod com {
    use std::mem::ManuallyDrop;

    use chrono::{NaiveDate, NaiveDateTime};
    use windows::core::{Interface, BSTR, GUID, IUnknown, PCWSTR};
    use windows::Win32::Foundation::{SYSTEMTIME, VARIANT_BOOL};
    use windows::Win32::System::Com::{
        CLSIDFromProgID, IDispatch, DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET, DISPATCH_PROPERTYPUT, DISPPARAMS,
    };
    use windows::Win32::System::Ole::{GetActiveObject, DISPID_PROPERTYPUT};
    use windows::Win32::System::Variant::{
        SystemTimeToVariantTime, VariantClear, VariantTimeToSystemTime, VARIANT, VT_BOOL, VT_BSTR, VT_DATE, VT_DISPATCH, VT_I4,
    };

    use super::{Event, Item};

    const LCID: u32 = 0x0400; // LOCALE_USER_DEFAULT

    pub enum Outlook {
        /// 클래식 Outlook 이 설치돼 있지 않다
        Missing,
        /// 켜져 있지 않다 (띄우지 않는다)
        NotRunning,
        App(IDispatch),
    }

    /// 이미 켜져 있는 클래식 Outlook 에 붙는다 — 꺼져 있으면 띄우지 않는다
    pub fn attach() -> Outlook {
        let prog: Vec<u16> = "Outlook.Application".encode_utf16().chain(Some(0)).collect();
        let Ok(clsid) = (unsafe { CLSIDFromProgID(PCWSTR(prog.as_ptr())) }) else {
            return Outlook::Missing;
        };
        let mut unk: Option<IUnknown> = None;
        if unsafe { GetActiveObject(&clsid, None, &mut unk) }.is_err() {
            return Outlook::NotRunning;
        }
        match unk.and_then(|u| u.cast::<IDispatch>().ok()) {
            Some(d) => Outlook::App(d),
            None => Outlook::NotRunning,
        }
    }

    /// 다 쓴 VARIANT 를 풀어 준다 (인자 배열로 그대로 넘기므로 VARIANT 와 같은 모양)
    #[repr(transparent)]
    struct Var(VARIANT);

    impl Drop for Var {
        fn drop(&mut self) {
            let _ = unsafe { VariantClear(&mut self.0) };
        }
    }

    impl Var {
        fn vt(&self) -> u16 {
            unsafe { self.0.Anonymous.Anonymous.vt.0 }
        }
        fn string(&self) -> Option<String> {
            (self.vt() == VT_BSTR.0).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.bstrVal.to_string() })
        }
        fn date(&self) -> Option<f64> {
            (self.vt() == VT_DATE.0).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.date })
        }
        fn bool(&self) -> Option<bool> {
            (self.vt() == VT_BOOL.0).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.boolVal.0 != 0 })
        }
        fn int(&self) -> Option<i32> {
            (self.vt() == VT_I4.0).then(|| unsafe { self.0.Anonymous.Anonymous.Anonymous.lVal })
        }
        fn object(&self) -> Option<IDispatch> {
            if self.vt() != VT_DISPATCH.0 {
                return None;
            }
            unsafe { (*self.0.Anonymous.Anonymous.Anonymous.pdispVal).clone() }
        }
    }

    fn arg_str(s: &str) -> VARIANT {
        let mut v = VARIANT::default();
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = VT_BSTR;
            inner.Anonymous.bstrVal = ManuallyDrop::new(BSTR::from(s));
        }
        v
    }

    fn arg_int(n: i32) -> VARIANT {
        let mut v = VARIANT::default();
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = VT_I4;
            inner.Anonymous.lVal = n;
        }
        v
    }

    fn arg_bool(b: bool) -> VARIANT {
        let mut v = VARIANT::default();
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = VT_BOOL;
            inner.Anonymous.boolVal = VARIANT_BOOL(if b { -1 } else { 0 });
        }
        v
    }

    fn invoke(d: &IDispatch, name: &str, flags: DISPATCH_FLAGS, args: Vec<VARIANT>) -> windows::core::Result<Var> {
        let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let names = [PCWSTR(w.as_ptr())];
        let mut id = 0i32;
        unsafe { d.GetIDsOfNames(&GUID::zeroed(), names.as_ptr(), 1, LCID, &mut id)? };
        // 인자는 거꾸로 넣는다 (COM 규칙)
        let mut args: Vec<Var> = args.into_iter().rev().map(Var).collect();
        let mut put = DISPID_PROPERTYPUT;
        let is_put = flags == DISPATCH_PROPERTYPUT;
        let params = DISPPARAMS {
            rgvarg: args.as_mut_ptr().cast(),
            rgdispidNamedArgs: if is_put { &mut put } else { std::ptr::null_mut() },
            cArgs: args.len() as u32,
            cNamedArgs: u32::from(is_put),
        };
        let mut out = Var(VARIANT::default());
        unsafe { d.Invoke(id, &GUID::zeroed(), LCID, flags, &params, Some(&mut out.0), None, None)? };
        drop(args);
        Ok(out)
    }

    fn get(d: &IDispatch, name: &str) -> windows::core::Result<Var> {
        invoke(d, name, DISPATCH_PROPERTYGET, Vec::new())
    }

    fn call(d: &IDispatch, name: &str, args: Vec<VARIANT>) -> windows::core::Result<Var> {
        invoke(d, name, DISPATCH_METHOD, args)
    }

    fn put(d: &IDispatch, name: &str, v: VARIANT) -> windows::core::Result<()> {
        invoke(d, name, DISPATCH_PROPERTYPUT, vec![v]).map(|_| ())
    }

    fn no_object() -> windows::core::Error {
        windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER)
    }

    /// OLE 날짜(로컬 시각) → 분 단위 시각
    fn to_time(d: f64) -> Option<NaiveDateTime> {
        let mut st = SYSTEMTIME::default();
        if unsafe { VariantTimeToSystemTime(d, &mut st) } == 0 {
            return None;
        }
        NaiveDate::from_ymd_opt(st.wYear as i32, st.wMonth as u32, st.wDay as u32)?.and_hms_opt(st.wHour as u32, st.wMinute as u32, 0)
    }

    /// 분 단위 시각 → Outlook 걸러내기(Restrict)에 쓰는 날짜 글 — 이 PC 의 날짜 · 시간 형식 ("2026-11-20 오후 3:30")
    fn restrict_date(t: NaiveDateTime) -> String {
        use chrono::{Datelike, Timelike};
        use windows::Win32::Globalization::{GetDateFormatEx, GetTimeFormatEx, DATE_SHORTDATE, TIME_NOSECONDS};
        let st = SYSTEMTIME {
            wYear: t.year() as u16,
            wMonth: t.month() as u16,
            wDay: t.day() as u16,
            wHour: t.hour() as u16,
            wMinute: t.minute() as u16,
            ..Default::default()
        };
        let mut d = [0u16; 64];
        let mut h = [0u16; 64];
        let nd = unsafe { GetDateFormatEx(PCWSTR::null(), DATE_SHORTDATE, Some(&st as *const SYSTEMTIME), PCWSTR::null(), Some(&mut d), PCWSTR::null()) };
        let nh = unsafe { GetTimeFormatEx(PCWSTR::null(), TIME_NOSECONDS, Some(&st as *const SYSTEMTIME), PCWSTR::null(), Some(&mut h)) };
        let s = |b: &[u16], n: i32| String::from_utf16_lossy(&b[..(n.max(1) as usize - 1)]);
        format!("{} {}", s(&d, nd), s(&h, nh))
    }

    #[allow(dead_code)]
    fn to_ole(t: NaiveDateTime) -> f64 {
        use chrono::{Datelike, Timelike};
        let st = SYSTEMTIME {
            wYear: t.year() as u16,
            wMonth: t.month() as u16,
            wDay: t.day() as u16,
            wHour: t.hour() as u16,
            wMinute: t.minute() as u16,
            ..Default::default()
        };
        let mut v = 0.0;
        unsafe { SystemTimeToVariantTime(&st, &mut v) };
        v
    }

    /// Windows 보안 센터에 등록된 백신들의 상태 (`root\SecurityCenter2` 의 `AntiVirusProduct.productState`)
    pub fn antivirus_states() -> windows::core::Result<Vec<u32>> {
        use windows::Win32::System::Com::{
            CoCreateInstance, CoSetProxyBlanket, CLSCTX_INPROC_SERVER, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
        };
        use windows::Win32::System::Wmi::{
            IWbemClassObject, IWbemContext, IWbemLocator, WbemLocator, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
        };
        unsafe {
            let loc: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
            let empty = BSTR::new();
            let svc = loc.ConnectServer(&BSTR::from("ROOT\\SecurityCenter2"), &empty, &empty, &empty, 0, &empty, None::<&IWbemContext>)?;
            // RPC_C_AUTHN_WINNT = 10, RPC_C_AUTHZ_NONE = 0
            CoSetProxyBlanket(&svc, 10, 0, PCWSTR::null(), RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE)?;
            let found = svc.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from("SELECT productState FROM AntiVirusProduct"),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None::<&IWbemContext>,
            )?;
            let mut out = Vec::new();
            for _ in 0..16 {
                let mut objs: [Option<IWbemClassObject>; 1] = [None];
                let mut n = 0u32;
                // 5초까지 (보안 센터가 답하지 않으면 그만)
                if found.Next(5000, &mut objs, &mut n).is_err() || n == 0 {
                    break;
                }
                let Some(o) = objs[0].take() else {
                    break;
                };
                let name: Vec<u16> = "productState".encode_utf16().chain(Some(0)).collect();
                let mut v = Var(VARIANT::default());
                if o.Get(PCWSTR(name.as_ptr()), 0, &mut v.0, None, None).is_ok() {
                    if let Some(x) = v.int() {
                        out.push(x as u32);
                    }
                }
            }
            Ok(out)
        }
    }

    /// 레지스트리 DWORD 하나 (없으면 None)
    fn dword(root: windows_sys::Win32::System::Registry::HKEY, path: &str, name: &str) -> Option<u32> {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_DWORD};
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (p, n) = (wide(path), wide(name));
        let mut v = 0u32;
        let mut size = 4u32;
        let rc = unsafe { RegGetValueW(root, p.as_ptr(), n.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut v as *mut u32).cast(), &mut size) };
        (rc == ERROR_SUCCESS).then_some(v)
    }

    /// 참석자를 읽어도 경고 창이 뜨지 않나 — 정책 · 프로그래밍 방식 액세스 설정 · 백신 상태 ([`super::people_allowed`])
    pub fn people_allowed() -> bool {
        use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
        const POLICY: &str = r"Software\Policies\Microsoft\Office\16.0\Outlook\Security";
        let policy = |name: &str| dword(HKEY_CURRENT_USER, POLICY, name).or_else(|| dword(HKEY_LOCAL_MACHINE, POLICY, name));
        // 설정은 설치 방식에 따라 여기저기 (일반 · 32비트 Office · 클릭 투 런 가상 레지스트리)
        let guard = [
            r"SOFTWARE\Microsoft\Office\16.0\Outlook\Security",
            r"SOFTWARE\WOW6432Node\Microsoft\Office\16.0\Outlook\Security",
            r"SOFTWARE\Microsoft\Office\ClickToRun\REGISTRY\MACHINE\Software\Microsoft\Office\16.0\Outlook\Security",
        ]
        .iter()
        .find_map(|p| dword(HKEY_LOCAL_MACHINE, p, "ObjectModelGuard"));
        let (mode, prompt) = (policy("AdminSecurityMode"), policy("PromptOOMAddressInformationAccess"));
        // 백신 상태가 필요할 때만 보안 센터에 묻는다
        let decided = matches!(mode, Some(1) | Some(2)) || (mode == Some(3) && prompt.is_some_and(|p| p != 3));
        let needs_av = !decided && !matches!(guard, Some(1) | Some(2));
        let av = needs_av && antivirus_states().map(|s| super::antivirus_good(&s)).unwrap_or(false);
        let ok = super::people_allowed(mode, prompt, guard, av);
        log::info!("Outlook 참석자 읽기 {} (정책 {mode:?}/{prompt:?}, 설정 {guard:?}, 백신 {av})", if ok { "됨" } else { "안 함 — 경고 창이 뜰 수 있음" });
        ok
    }

    /// `from` ~ `to` 사이에 걸친 기본 일정 폴더의 일정 (되풀이 일정은 그날 것으로 펼친다) — 최대 60개
    pub fn events(app: &IDispatch, from: NaiveDateTime, to: NaiveDateTime, with_people: bool) -> windows::core::Result<Vec<(Item, bool, Event)>> {
        let ns = call(app, "GetNamespace", vec![arg_str("MAPI")])?.object().ok_or_else(no_object)?;
        // 9 = olFolderCalendar
        let cal = call(&ns, "GetDefaultFolder", vec![arg_int(9)])?.object().ok_or_else(no_object)?;
        let items = get(&cal, "Items")?.object().ok_or_else(no_object)?;
        // 되풀이 일정을 펼치려면 Sort → IncludeRecurrences → Restrict 순서로
        call(&items, "Sort", vec![arg_str("[Start]")])?;
        put(&items, "IncludeRecurrences", arg_bool(true))?;
        let filter = format!("[Start] < '{}' AND [End] > '{}'", restrict_date(to), restrict_date(from));
        let found = call(&items, "Restrict", vec![arg_str(&filter)])?.object().ok_or_else(no_object)?;
        let mut out = Vec::new();
        let mut it = call(&found, "GetFirst", Vec::new())?.object();
        while let Some(item) = it {
            if out.len() >= 60 {
                break;
            }
            let start = get(&item, "Start").ok().and_then(|v| v.date()).and_then(to_time);
            let end = get(&item, "End").ok().and_then(|v| v.date()).and_then(to_time);
            if let (Some(start), Some(end)) = (start, end) {
                // 이 PC 날짜 형식을 Outlook 이 잘못 읽어도 엉뚱한 일정을 쓰지 않게 다시 본다
                if start < to && end > from {
                    let all_day = get(&item, "AllDayEvent").ok().and_then(|v| v.bool()).unwrap_or(false);
                    // 5 = olMeetingCanceled, 7 = olMeetingReceivedAndCanceled
                    let canceled = get(&item, "MeetingStatus").ok().and_then(|v| v.int()).is_some_and(|s| s == 5 || s == 7);
                    let subject = get(&item, "Subject").ok().and_then(|v| v.string()).unwrap_or_default();
                    let attendees = if with_people {
                        let org = get(&item, "Organizer").ok().and_then(|v| v.string()).unwrap_or_default();
                        let req = get(&item, "RequiredAttendees").ok().and_then(|v| v.string()).unwrap_or_default();
                        let opt = get(&item, "OptionalAttendees").ok().and_then(|v| v.string()).unwrap_or_default();
                        super::attendee_names(&[&org, &req, &opt])
                    } else {
                        Vec::new()
                    };
                    let fmt = |t: NaiveDateTime| t.format("%Y-%m-%d %H:%M").to_string();
                    out.push((
                        Item { start, end },
                        all_day || canceled || subject.trim().is_empty(),
                        Event { subject: subject.trim().to_string(), start: fmt(start), end: fmt(end), attendees },
                    ));
                }
            }
            it = call(&found, "GetNext", Vec::new())?.object();
        }
        Ok(out)
    }
}

// ───────────────────────── Outlook 스레드 ─────────────────────────

type Job = Box<dyn FnOnce() + Send>;

/// COM 을 여는 스레드 하나 (STA). 일을 넘기고 답을 짧게만 기다린다
pub struct Calendar {
    tx: Mutex<Option<Sender<Job>>>,
}

impl Calendar {
    pub fn new() -> Self {
        Self { tx: Mutex::new(None) }
    }

    fn sender(&self) -> Option<Sender<Job>> {
        let mut tx = self.tx.lock();
        if tx.is_none() {
            let (s, r) = channel::<Job>();
            let spawned = std::thread::Builder::new().name("bd-outlook".into()).spawn(move || {
                #[cfg(windows)]
                unsafe {
                    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
                }
                for job in r {
                    job();
                }
            });
            if spawned.is_ok() {
                *tx = Some(s);
            }
        }
        tx.clone()
    }

    /// Outlook 스레드에서 돌리고 `WAIT` 안에 답을 받는다 (못 받으면 None — Outlook 이 바쁘거나 멈춤)
    pub(crate) fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let tx = self.sender()?;
        let (rs, rr) = channel();
        tx.send(Box::new(move || {
            let _ = rs.send(f());
        }))
        .ok()?;
        rr.recv_timeout(WAIT).ok()
    }
}

/// 참석자를 읽어도 경고가 뜨지 않나 — 읽을 때마다 다시 본다 (그 사이 백신이 꺼지거나 정책이 바뀌면 경고가 돌아오므로,
/// 기억해 두지 않는다. 레지스트리 몇 개와 보안 센터 한 번이라 가볍다)
#[cfg(windows)]
fn people_ok() -> bool {
    com::people_allowed()
}

/// 오늘 일정을 읽는다 → 상태 (지금 일정 · 다음 일정)
pub(crate) fn read_now() -> Status {
    #[cfg(windows)]
    {
        let now = Local::now().naive_local();
        let app = match com::attach() {
            com::Outlook::Missing => return Status::NoOutlook,
            com::Outlook::NotRunning => return Status::NotRunning,
            com::Outlook::App(a) => a,
        };
        let trusted = people_ok();
        let (from, to) = query_range(now);
        match com::events(&app, from, to, trusted) {
            Ok(list) => {
                let times: Vec<(Item, bool)> = list.iter().map(|(i, skip, _)| (*i, *skip)).collect();
                let now_ev = pick(now, &times).map(|i| list[i].2.clone());
                let next = next_after(now, &times).map(|i| list[i].2.clone());
                Status::Ready { trusted, now: now_ev, next }
            }
            Err(e) => Status::Error { message: format!("일정을 읽지 못했어요 ({})", e.message()) },
        }
    }
    #[cfg(not(windows))]
    Status::NoOutlook
}

/// 설정 화면: 지금 Outlook 상태 (켜 두지 않았어도 — 켤지 고를 수 있게)
#[tauri::command]
pub async fn calendar_status(app: AppHandle) -> Status {
    let r = tauri::async_runtime::spawn_blocking(move || app.state::<Calendar>().run(read_now)).await.ok().flatten();
    r.unwrap_or(Status::Error { message: "Outlook 이 답하지 않아요 — 바쁘거나 창이 열려 있을 수 있어요".into() })
}

/// 지금 녹음하는 회의에 맞는 일정 (설정에서 켰을 때만 · 짧게 기다린다)
pub fn current(app: &AppHandle) -> Option<Event> {
    if !app.state::<crate::AppState>().settings.lock().calendar {
        return None;
    }
    match app.state::<Calendar>().run(read_now)? {
        Status::Ready { now, .. } => now,
        Status::Error { message } => {
            log::warn!("{message}");
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    fn it(sh: u32, sm: u32, eh: u32, em: u32) -> Item {
        Item { start: t(sh, sm), end: t(eh, em) }
    }

    #[test]
    fn picks_the_meeting_around_now() {
        let items = [
            (it(9, 0, 10, 0), false),  // 지난 회의
            (it(14, 0, 15, 0), false), // 지금 회의
            (it(14, 30, 15, 30), false),
            (it(0, 0, 23, 59), true),  // 하루 종일 · 취소됨은 뺀다
            (it(16, 0, 17, 0), false), // 다음 회의
        ];
        // 시작 5분 뒤 → 14시 회의
        assert_eq!(pick(t(14, 5), &items), Some(1));
        // 겹치면 시작이 가까운 쪽
        assert_eq!(pick(t(14, 28), &items), Some(2));
        // 시작 15분 전부터는 그 회의
        assert_eq!(pick(t(15, 46), &items), Some(4));
        assert_eq!(pick(t(15, 44), &items), None);
        assert_eq!(pick(t(15, 33), &items), Some(2));
        // 끝나고 5분까지
        assert_eq!(pick(t(10, 5), &items), Some(0));
        assert_eq!(pick(t(10, 6), &items), None);
        // 하루 종일 일정만 있으면 없음
        assert_eq!(pick(t(12, 0), &items), None);
        // 다음 일정
        assert_eq!(next_after(t(14, 5), &items), Some(2));
        assert_eq!(next_after(t(16, 30), &items), None);
    }

    /// 이 PC 의 Outlook 을 실제로 본다 — 꺼져 있으면 띄우지 않아야 한다:
    /// `cargo test -p badasugi calendar::tests::live_status -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_status_does_not_start_outlook() {
        let running = || {
            std::process::Command::new("tasklist")
                .args(["/FI", "IMAGENAME eq OUTLOOK.EXE", "/NH"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("outlook.exe"))
                .unwrap_or(false)
        };
        let before = running();
        let cal = Calendar::new();
        let st = cal.run(read_now);
        std::thread::sleep(std::time::Duration::from_secs(5));
        let after = running();
        println!("상태: {st:?} · Outlook 실행 {before} → {after}");
        assert!(st.is_some());
        assert!(before || !after, "꺼져 있던 Outlook 이 떴어요");
    }

    #[test]
    fn query_covers_the_margins_around_midnight() {
        let at = |d: u32, h: u32, m: u32| chrono::NaiveDate::from_ymd_opt(2026, 11, d).unwrap().and_hms_opt(h, m, 0).unwrap();
        // 낮에는 오늘 하루
        assert_eq!(query_range(at(20, 14, 0)), (at(20, 0, 0), at(21, 0, 0)));
        // 23:50 → 내일 0시 회의도 (시작 15분 전)
        let (_, to) = query_range(at(20, 23, 50));
        assert!(to > at(21, 0, 0));
        assert_eq!(pick(at(20, 23, 50), &[(Item { start: at(21, 0, 0), end: at(21, 1, 0) }, false)]), Some(0));
        // 0:03 → 방금 0시에 끝난 회의도 (끝나고 5분)
        let (from, _) = query_range(at(21, 0, 3));
        assert!(from < at(21, 0, 0));
    }

    #[test]
    fn reads_attendees_only_when_outlook_would_not_warn() {
        // Windows Defender 켜짐 · 최신 / 꺼짐 / 오래됨
        assert!(antivirus_good(&[0x061100]));
        assert!(!antivirus_good(&[0x060100]));
        assert!(!antivirus_good(&[0x061110]));
        assert!(antivirus_good(&[0x060100, 0x041000]));
        assert!(!antivirus_good(&[]));
        // 기본 설정: 백신 상태에 따라
        assert!(people_allowed(None, None, None, true));
        assert!(!people_allowed(None, None, None, false));
        assert!(people_allowed(None, None, Some(0), true));
        // "경고하지 않음" · "늘 경고"
        assert!(people_allowed(None, None, Some(2), false));
        assert!(!people_allowed(None, None, Some(1), true));
        // 회사 정책이 정해 두었으면 그대로 (정책을 쓰지 않으면 설정으로) — 0 자동 거부 · 1 묻기 · 2 자동 허용
        assert!(people_allowed(Some(3), Some(2), Some(1), false));
        assert!(!people_allowed(Some(3), Some(0), None, true));
        assert!(!people_allowed(Some(3), Some(1), Some(2), true));
        assert!(people_allowed(Some(3), Some(3), None, true));
        assert!(!people_allowed(Some(3), Some(3), None, false));
        assert!(people_allowed(Some(0), Some(0), None, true));
        // 공용 폴더 보안 양식(1 · 2)은 백신이 좋아도 읽지 않는다
        assert!(!people_allowed(Some(1), None, None, true));
        assert!(!people_allowed(Some(2), Some(2), Some(2), true));
        // 모르는 값은 읽지 않는다
        assert!(!people_allowed(None, None, Some(7), true));
    }

    /// 이 PC 의 Outlook 보안 설정 · 백신 상태로 참석자를 읽을지:
    /// `cargo test -p badasugi calendar::tests::live_people -- --ignored --nocapture`
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn live_people_allowed() {
        let cal = Calendar::new();
        let r = cal.run(|| (com::antivirus_states().map_err(|e| e.message()), com::people_allowed()));
        println!("{r:?}");
        assert!(r.is_some_and(|(s, _)| s.is_ok()));
    }

    #[test]
    fn attendee_names_from_outlook_lists() {
        assert_eq!(
            attendee_names(&["김 팀장", "김 팀장; 이 개발 (개발팀); park.design@example.com", "'최 기획'"]),
            vec!["김 팀장", "이 개발", "park.design", "최 기획"]
        );
        assert_eq!(attendee_names(&["Jane Doe <jane@example.com>; ", ""]), vec!["Jane Doe"]);
        assert!(attendee_names(&["", " ; "]).is_empty());
    }
}
