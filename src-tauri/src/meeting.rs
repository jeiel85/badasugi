//! 회의 시작 알림 — 회의 앱이 마이크를 쓰기 시작하면 "받아쓸까요?", 놓으면 "멈출까요?" 를 묻는다
//!
//! 설정에서 켰을 때만 몇 초마다 마이크를 쓰는 앱을 본다. 두 곳을 함께 본다:
//! - Windows 오디오 세션 — 입력 장치마다 지금 소리를 받고 있는 프로세스 (원격 데스크톱에서도 보인다)
//! - Windows 가 앱마다 남기는 마이크 사용 기록 (`HKCU\Software\Microsoft\Windows\CurrentVersion\
//!   CapabilityAccessManager\ConsentStore\microphone` 아래 앱 키의 `LastUsedTimeStop` 이 0 이면 지금 쓰는 중) —
//!   스토어 앱(새 Teams)이 다른 프로세스로 소리를 받아도 앱 이름으로 남는다
//!
//! 녹음은 저절로 시작하지 않는다 — 묻기만 한다(동의 없는 녹음 방지).
//!
//! 알림은 작은 창(`meet.html`)으로 작업 표시줄 시계 위에 띄운다. 회의 앱을 보고 있는 사람의
//! 포커스를 빼앗지 않게 활성화하지 않고 띄우고, 닫을 때는 창을 없앤다(쉴 때 가볍게).

use std::time::{Duration, Instant};

use badasugi_core::audio::SourceKind;
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::{AppState, CmdResult, RecState};

/// (설정의 앱 id, 알림에 보일 이름, 실행 파일 이름 — `_` 로 끝나면 스토어 앱 패키지 이름 앞부분)
const MATCHERS: &[(&str, &str, &str)] = &[
    ("zoom", "Zoom", "zoom.exe"),
    ("teams", "Teams", "ms-teams.exe"),
    ("teams", "Teams", "teams.exe"),
    ("teams", "Teams", "MSTeams_"),
    ("teams", "Teams", "MicrosoftTeams_"),
    ("browser", "Chrome", "chrome.exe"),
    ("browser", "Edge", "msedge.exe"),
    ("browser", "웨일", "whale.exe"),
    ("browser", "Firefox", "firefox.exe"),
    ("browser", "Brave", "brave.exe"),
    ("webex", "Webex", "ciscocollabhost.exe"),
    ("webex", "Webex", "webex.exe"),
    ("webex", "Webex", "atmgr.exe"),
    ("webex", "Webex", "webexmta.exe"),
    ("slack", "Slack", "slack.exe"),
    ("slack", "Slack", "91750D7E.Slack_"),
    ("discord", "Discord", "discord.exe"),
    ("kakaotalk", "카카오톡", "kakaotalk.exe"),
    ("kakaotalk", "카카오톡", "kakaotalkui.exe"),
];

/// 처음 켰을 때 지켜볼 앱
pub fn default_apps() -> Vec<String> {
    ["zoom", "teams", "browser", "webex"].map(String::from).to_vec()
}

/// 마이크를 쓰는 중인 앱: 실행 파일 경로(사용 기록은 `\` 대신 `#`) 또는 스토어 앱 패키지 이름(packaged)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicUser {
    pub key: String,
    pub packaged: bool,
}

/// 마이크를 쓰는 앱 가운데 지켜보는 회의 앱의 이름 (없으면 None)
pub fn meeting_app(users: &[MicUser], watched: &[String]) -> Option<&'static str> {
    users.iter().find_map(|u| {
        // 스토어 앱이 아니면 키가 실행 파일 경로(`\` 대신 `#`)
        let exe = u.key.rsplit(['#', '\\']).next().unwrap_or("").to_lowercase();
        MATCHERS.iter().find_map(|&(id, name, pat)| {
            let hit = match pat.strip_suffix('_') {
                Some(_) => u.packaged && u.key.to_lowercase().starts_with(&pat.to_lowercase()),
                None => !u.packaged && exe == pat,
            };
            (hit && watched.iter().any(|w| w == id)).then_some(name)
        })
    })
}

// ───────────────────────── 언제 묻나 ─────────────────────────

/// 지금 녹음 상태 (알림 판단용)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rec {
    Idle,
    Recording,
    /// 시작 · 정리 중이거나 파일 받아쓰기
    Busy,
}

/// 알림 창에 보일 것
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Prompt {
    /// 회의 앱이 마이크를 쓰기 시작 — 받아쓸까요?
    Start { app: String },
    /// 알림에서 녹음을 시작함 — 잠깐 보여 주고 닫는다
    Started { app: String },
    /// 회의 앱이 마이크를 놓음 — 멈출까요?
    End { app: String },
}

/// 마이크를 이만큼 계속 써야 회의로 본다 (장치 점검 · 음성 검색처럼 잠깐 여는 것은 거른다)
const START_AFTER: Duration = Duration::from_secs(3);
/// 이만큼 계속 놓아야 회의가 끝난 것으로 본다 (장치를 바꾸며 잠깐 놓는 것은 거른다)
const END_AFTER: Duration = Duration::from_secs(8);
/// 답이 없으면 알림을 닫는다 (시작 알림이면 이번 회의는 묻지 않음)
const ASK_FOR: Duration = Duration::from_secs(60);
/// "받아 적고 있어요" 를 보여 주는 시간
const STARTED_FOR: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Detector {
    on_since: Option<Instant>,
    off_since: Option<Instant>,
    /// 지금 회의 중인 앱 (마이크를 START_AFTER 넘게 쓰는 중)
    meeting: Option<String>,
    /// 이 회의 동안 녹음했다 → 회의가 끝나면 멈출지 묻는다
    follow: bool,
    prompt: Option<(Prompt, Instant)>,
}

impl Detector {
    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref().map(|p| &p.0)
    }

    /// 몇 초마다: 마이크를 쓰는 회의 앱(없으면 None)과 녹음 상태를 받아,
    /// 알림이 바뀌었으면 `Some(새 알림)` (`Some(None)` = 닫기)
    pub fn tick(&mut self, now: Instant, app: Option<&str>, rec: Rec) -> Option<Option<Prompt>> {
        let before = self.prompt().cloned();
        if app.is_some() {
            self.off_since = None;
            self.on_since.get_or_insert(now);
        } else {
            self.on_since = None;
            self.off_since.get_or_insert(now);
        }
        let held = |since: Option<Instant>, d: Duration| since.is_some_and(|t| now.duration_since(t) >= d);

        if let Some(name) = app.filter(|_| self.meeting.is_none()) {
            if held(self.on_since, START_AFTER) {
                log::info!("회의 시작 감지: {name} 이(가) 마이크를 씀");
                self.meeting = Some(name.to_string());
                self.follow = false;
                match rec {
                    Rec::Idle => self.prompt = Some((Prompt::Start { app: name.to_string() }, now)),
                    // 회의가 잠깐 끊겼다 다시 이어졌으면 "멈출까요?" 는 거둔다
                    _ if matches!(self.prompt(), Some(Prompt::End { .. })) => self.prompt = None,
                    _ => {}
                }
            }
        }
        if self.meeting.is_some() && rec == Rec::Recording {
            self.follow = true;
        }
        if self.meeting.is_some() && held(self.off_since, END_AFTER) {
            let name = self.meeting.take().unwrap_or_default();
            log::info!("회의 끝 감지: {name} 이(가) 마이크를 놓음");
            if matches!(self.prompt(), Some(Prompt::Start { .. })) {
                self.prompt = None;
            }
            if std::mem::take(&mut self.follow) && rec == Rec::Recording {
                self.prompt = Some((Prompt::End { app: name }, now));
            }
        }
        // 상황이 바뀌어 필요 없어진 알림은 닫는다
        if let Some((p, at)) = &self.prompt {
            let age = now.duration_since(*at);
            let stale = match p {
                // 사람이 직접 녹음을 시작했거나 오래 답이 없음
                Prompt::Start { .. } => rec != Rec::Idle || age >= ASK_FOR,
                // 모델을 불러오는 등 시작하는 동안은 두고, 녹음이 시작된 뒤 잠깐 보인다
                Prompt::Started { .. } => rec != Rec::Busy && age >= STARTED_FOR,
                // 이미 멈췄거나 오래 답이 없음 (계속 녹음)
                Prompt::End { .. } => rec != Rec::Recording || age >= ASK_FOR,
            };
            if stale {
                self.prompt = None;
            }
        }
        let after = self.prompt().cloned();
        (after != before).then_some(after)
    }

    /// 알림에서 녹음을 시작한다(시작하는 중) · 시작됐다(다시 부르면 그때부터 STARTED_FOR)
    fn started(&mut self, now: Instant) {
        let app = match self.prompt() {
            Some(Prompt::Start { app }) => app.clone(),
            _ => self.meeting.clone().unwrap_or_default(),
        };
        self.follow = true;
        self.prompt = Some((Prompt::Started { app }, now));
    }

    fn close(&mut self) {
        self.prompt = None;
    }

    /// 기능을 껐다 — 처음부터
    fn reset(&mut self) {
        *self = Self::default();
    }
}

// ───────────────────────── 마이크를 쓰는 앱 찾기 ─────────────────────────

/// 지금 마이크를 쓰는 앱 (오디오 세션 + 사용 기록)
pub fn mic_users() -> Vec<MicUser> {
    let mut v = session_users();
    for u in registry_users() {
        if !v.contains(&u) {
            v.push(u);
        }
    }
    v
}

/// 입력 장치마다 지금 소리를 받고 있는(활성) 오디오 세션의 프로세스 — 실행 파일 경로와, 스토어 앱이면 패키지 이름
#[cfg(windows)]
fn session_users() -> Vec<MicUser> {
    use windows::core::{Interface, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
    use windows::Win32::Media::Audio::{
        eCapture, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
        MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    thread_local! {
        // 이 스레드에서 COM 을 한 번만 연다
        static COM: () = unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        };
    }
    COM.with(|_| ());
    let me = std::process::id();
    let mut pids = Vec::new();
    unsafe {
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return Vec::new();
        };
        let Ok(devs) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        for i in 0..devs.GetCount().unwrap_or(0) {
            let Ok(mgr) = devs.Item(i).and_then(|d| d.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)) else {
                continue;
            };
            let Ok(sessions) = mgr.GetSessionEnumerator() else {
                continue;
            };
            for j in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(s) = sessions.GetSession(j) else {
                    continue;
                };
                if s.GetState().ok() != Some(AudioSessionStateActive) {
                    continue;
                }
                if let Ok(pid) = s.cast::<IAudioSessionControl2>().and_then(|s2| s2.GetProcessId()) {
                    if pid != 0 && pid != me && !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }
        let mut out = Vec::new();
        for pid in pids {
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                continue;
            };
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
                out.push(MicUser { key: String::from_utf16_lossy(&buf[..len as usize]), packaged: false });
            }
            let mut len = buf.len() as u32;
            if GetPackageFamilyName(h, &mut len, Some(PWSTR(buf.as_mut_ptr()))) == ERROR_SUCCESS {
                // 길이에 끝 NUL 이 들어 있다
                let n = (len as usize).saturating_sub(1).min(buf.len());
                out.push(MicUser { key: String::from_utf16_lossy(&buf[..n]), packaged: true });
            }
            let _ = CloseHandle(h);
        }
        out
    }
}

#[cfg(not(windows))]
fn session_users() -> Vec<MicUser> {
    Vec::new()
}

/// Windows 가 남긴 앱별 마이크 사용 기록에서 아직 놓지 않은 앱
#[cfg(windows)]
fn registry_users() -> Vec<MicUser> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_QWORD,
    };

    const BASE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let qword = |key: HKEY, sub: &[u16], name: &str| -> Option<u64> {
        let name = wide(name);
        let mut v = 0u64;
        let mut size = 8u32;
        let rc = unsafe {
            RegGetValueW(key, sub.as_ptr(), name.as_ptr(), RRF_RT_REG_QWORD, std::ptr::null_mut(), (&mut v as *mut u64).cast(), &mut size)
        };
        (rc == ERROR_SUCCESS).then_some(v)
    };
    let mut out = Vec::new();
    for (path, packaged) in [(BASE.to_string(), true), (format!(r"{BASE}\NonPackaged"), false)] {
        let mut key: HKEY = std::ptr::null_mut();
        if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(&path).as_ptr(), 0, KEY_READ, &mut key) } != ERROR_SUCCESS {
            continue;
        }
        for i in 0.. {
            let mut name = [0u16; 512];
            let mut len = name.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    key,
                    i,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc != ERROR_SUCCESS {
                break; // ERROR_NO_MORE_ITEMS
            }
            let sub = &name[..=len as usize]; // NUL 까지
            let key_name = String::from_utf16_lossy(&name[..len as usize]);
            if packaged && key_name == "NonPackaged" {
                continue;
            }
            // 쓰기 시작한 적이 있고 아직 놓지 않음
            let started = qword(key, sub, "LastUsedTimeStart").is_some_and(|t| t > 0);
            if started && qword(key, sub, "LastUsedTimeStop") == Some(0) {
                out.push(MicUser { key: key_name, packaged });
            }
        }
        unsafe { RegCloseKey(key) };
    }
    out
}

#[cfg(not(windows))]
fn registry_users() -> Vec<MicUser> {
    Vec::new()
}

// ───────────────────────── 알림 창 ─────────────────────────

pub struct Meet {
    det: Mutex<Detector>,
    /// 지금 회의에 맞는 Outlook 일정 제목 (일정에서 가져오기를 켰을 때 · 알림에 보인다)
    event: Mutex<Option<String>>,
}

impl Meet {
    pub fn new() -> Self {
        Self { det: Mutex::new(Detector::default()), event: Mutex::new(None) }
    }
}

const POLL: Duration = Duration::from_secs(2);
const LABEL: &str = "meet";

fn rec_now(app: &AppHandle) -> Rec {
    let st = app.state::<AppState>();
    let rec = *st.rec.lock();
    match rec {
        RecState::Idle => Rec::Idle,
        // 녹음 폴더 파일을 받아 적는 중이면 쉬는 것으로 본다 — 받아쓰기를 시작하면 그 파일은 비켜 준다
        _ if crate::watch::job_running(app) => Rec::Idle,
        RecState::Recording if !st.file_mode.load(std::sync::atomic::Ordering::SeqCst) => Rec::Recording,
        _ => Rec::Busy,
    }
}

/// 설정에서 켜 두면 몇 초마다 마이크 사용 기록을 본다
pub fn spawn(app: AppHandle) {
    let _ = std::thread::Builder::new().name("bd-meet".into()).spawn(move || loop {
        std::thread::sleep(POLL);
        let (on, watched) = {
            let st = app.state::<AppState>();
            let s = st.settings.lock();
            (s.meeting_alert, s.meeting_apps.clone())
        };
        let meet = app.state::<Meet>();
        let change = if on {
            let hit = meeting_app(&mic_users(), &watched);
            let rec = rec_now(&app);
            meet.det.lock().tick(Instant::now(), hit, rec)
        } else {
            let mut det = meet.det.lock();
            let had = det.prompt().is_some();
            det.reset();
            had.then_some(None)
        };
        if let Some(p) = change {
            // 받아쓸지 묻는 알림에는 지금 일정의 제목을 함께 (Outlook 을 기다리는 동안은 알림을 미룬다 — 몇 초 안)
            if matches!(p, Some(Prompt::Start { .. })) {
                *meet.event.lock() = crate::calendar::current(&app).map(|e| e.subject).filter(|s| !s.is_empty());
            }
            show(&app, p.as_ref());
        }
    });
}

/// 알림 창에 보일 내용
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetView {
    prompt: Option<Prompt>,
    /// 받아쓰기를 시작하면 받을 소리
    source: SourceKind,
    /// 지금 녹음한 시간 (ms)
    elapsed_ms: u64,
    theme: String,
    /// 지금 회의에 맞는 Outlook 일정 제목
    event: Option<String>,
}

/// 알림에서 녹음을 시작할 때 받을 소리 — 마이크만 고른 상태면 상대방 목소리(PC 소리)도 함께
fn meeting_source(s: SourceKind) -> SourceKind {
    match s {
        SourceKind::Mic => SourceKind::Both,
        s => s,
    }
}

fn view(app: &AppHandle) -> MeetView {
    let st = app.state::<AppState>();
    let s = st.settings.lock().clone();
    let elapsed_ms = st.rec_started.lock().map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
    let meet = app.state::<Meet>();
    let prompt = meet.det.lock().prompt().cloned();
    let event = meet.event.lock().clone();
    MeetView {
        prompt,
        source: meeting_source(s.source),
        elapsed_ms,
        theme: s.theme,
        event,
    }
}

/// 알림을 띄우거나(창이 없으면 만들고) 닫는다. 창은 화면이 내용을 그린 뒤 `meet_ready` 에서 보인다.
pub fn show(app: &AppHandle, prompt: Option<&Prompt>) {
    let existing = app.get_webview_window(LABEL);
    match (prompt, existing) {
        (None, Some(w)) => {
            let _ = w.destroy();
        }
        (None, None) => {}
        (Some(_), Some(_)) => {
            let _ = app.emit_to(LABEL, "bd://meet", view(app));
        }
        (Some(_), None) => {
            let built = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("meet.html".into()))
                .title("받아쓰기 — 회의 알림")
                .inner_size(344.0, 126.0)
                .resizable(false)
                .maximizable(false)
                .minimizable(false)
                .decorations(false)
                .shadow(true)
                .always_on_top(true)
                .skip_taskbar(true)
                .focused(false)
                .visible(false)
                .build();
            if let Err(e) = built {
                log::warn!("회의 알림 창을 만들지 못함: {e}");
            }
        }
    }
}

#[tauri::command]
pub fn meet_state(app: AppHandle) -> MeetView {
    view(&app)
}

/// 화면이 내용을 그렸다 → 시계 위(미니 창이 있으면 그 위)에 포커스를 빼앗지 않고 보인다
#[tauri::command]
pub async fn meet_ready(app: AppHandle) {
    let (Some(w), main) = (app.get_webview_window(LABEL), app.get_webview_window("main")) else {
        return;
    };
    if app.state::<Meet>().det.lock().prompt().is_none() {
        let _ = w.destroy();
        return;
    }
    let mini = app.state::<AppState>().settings.lock().mini_mode;
    crate::snap::show_popup(&w, main.as_ref(), mini);
}

/// 알림의 단추: start(받아쓰기 시작) · stop(멈추기) · open(창 열기) · close(닫기 · 이번엔 안 함 · 계속)
#[tauri::command]
pub async fn meet_answer(app: AppHandle, action: String) -> CmdResult<()> {
    let meet = app.state::<Meet>();
    match action.as_str() {
        "start" => {
            meet.det.lock().started(Instant::now());
            let source = meeting_source(app.state::<AppState>().settings.lock().source);
            if let Err(e) = crate::start_session(&app, None, Some(source)) {
                meet.det.lock().close();
                show(&app, None);
                crate::show_main(&app);
                let _ = app.emit("bd://toast", e.clone());
                return Err(e);
            }
            let p = {
                let mut det = meet.det.lock();
                det.started(Instant::now());
                det.prompt().cloned()
            };
            show(&app, p.as_ref());
        }
        "stop" => {
            meet.det.lock().close();
            show(&app, None);
            crate::stop_recording_inner(&app);
        }
        "open" => {
            meet.det.lock().close();
            show(&app, None);
            crate::show_main(&app);
        }
        _ => {
            meet.det.lock().close();
            show(&app, None);
        }
    }
    Ok(())
}

/// 자동 점검: 알림 창이 포커스를 빼앗지 않고 화면 안에 뜨고, 닫으면 없어지는지
pub fn smoke_check(app: &AppHandle) -> Result<String, String> {
    let meet = app.state::<Meet>();
    let prompt = Prompt::Start { app: "점검".into() };
    meet.det.lock().prompt = Some((prompt.clone(), Instant::now()));
    show(app, Some(&prompt));
    let t = Instant::now();
    let shown = loop {
        if let Some(w) = app.get_webview_window(LABEL).filter(|w| w.is_visible().unwrap_or(false)) {
            break Some(w);
        }
        if t.elapsed() > Duration::from_secs(15) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let secs = t.elapsed().as_secs_f64();
    let result = match shown {
        None => Err("15초 안에 뜨지 않았어요 (meet.html 스크립트 오류 가능성)".to_string()),
        Some(w) => {
            let (Ok(p), Ok(sz)) = (w.outer_position(), w.outer_size()) else {
                return Err("창 자리를 알 수 없어요".into());
            };
            let inside = app.available_monitors().unwrap_or_default().iter().any(|m| {
                let r = m.work_area();
                p.x >= r.position.x
                    && p.y >= r.position.y
                    && p.x + sz.width as i32 <= r.position.x + r.size.width as i32
                    && p.y + sz.height as i32 <= r.position.y + r.size.height as i32
            });
            if w.is_focused().unwrap_or(false) {
                Err("알림 창이 포커스를 가져갔어요".into())
            } else if !inside {
                Err(format!("화면 밖에 떴어요 ({}, {}) {}×{}", p.x, p.y, sz.width, sz.height))
            } else {
                Ok(format!("{secs:.1}초 만에 뜸 · 포커스 그대로 · ({}, {}) {}×{}", p.x, p.y, sz.width, sz.height))
            }
        }
    };
    meet.det.lock().close();
    show(app, None);
    let t = Instant::now();
    while app.get_webview_window(LABEL).is_some() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(100));
    }
    match (result, app.get_webview_window(LABEL).is_some()) {
        (Ok(_), true) => Err("닫은 뒤에도 창이 남아 있어요".into()),
        (r, _) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(key: &str, packaged: bool) -> MicUser {
        MicUser { key: key.into(), packaged }
    }

    fn all() -> Vec<String> {
        MATCHERS.iter().map(|m| m.0.to_string()).collect()
    }

    #[test]
    fn finds_watched_meeting_apps() {
        let zoom = user(r"C:#Users#me#AppData#Roaming#Zoom#bin#Zoom.exe", false);
        let teams = user("MSTeams_8wekyb3d8bbwe", true);
        let chrome = user(r"C:#Program Files#Google#Chrome#Application#chrome.exe", false);
        let other = user(r"C:#Tools#recorder.exe", false);
        // 오디오 세션에서 찾은 앱은 보통 경로
        let zoom_session = user(r"C:\Users\me\AppData\Roaming\Zoom\bin\Zoom.exe", false);
        assert_eq!(meeting_app(&[zoom_session], &all()), Some("Zoom"));
        assert_eq!(meeting_app(&[other.clone(), zoom.clone()], &all()), Some("Zoom"));
        assert_eq!(meeting_app(&[teams.clone()], &all()), Some("Teams"));
        assert_eq!(meeting_app(&[chrome.clone()], &all()), Some("Chrome"));
        // 지켜보지 않는 앱 · 목록에 없는 앱은 거른다
        assert_eq!(meeting_app(&[chrome, other.clone()], &["zoom".into()]), None);
        assert_eq!(meeting_app(&[other], &all()), None);
        // 이름이 같아도 스토어 앱 · 일반 앱을 섞어 맞추지 않는다
        assert_eq!(meeting_app(&[user("zoom.exe", true)], &all()), None);
        assert_eq!(meeting_app(&[user(r"C:#x#MSTeams_8wekyb3d8bbwe", false)], &all()), None);
        // 실행 파일 이름은 대소문자를 가리지 않고, 앞부분만 같은 이름은 아니다
        assert_eq!(meeting_app(&[user(r"D:#Apps#ZOOM.EXE", false)], &all()), Some("Zoom"));
        assert_eq!(meeting_app(&[user(r"D:#Apps#myzoom.exe", false)], &all()), None);
    }

    fn secs(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    #[test]
    fn asks_once_after_the_app_keeps_the_mic() {
        let t0 = Instant::now();
        let mut d = Detector::default();
        // 잠깐 열었다 놓은 것은 회의가 아니다
        assert_eq!(d.tick(t0, Some("Zoom"), Rec::Idle), None);
        assert_eq!(d.tick(secs(t0, 2), None, Rec::Idle), None);
        assert_eq!(d.tick(secs(t0, 4), Some("Zoom"), Rec::Idle), None);
        // 3초 넘게 쓰면 묻는다
        assert_eq!(d.tick(secs(t0, 7), Some("Zoom"), Rec::Idle), Some(Some(Prompt::Start { app: "Zoom".into() })));
        assert_eq!(d.tick(secs(t0, 9), Some("Zoom"), Rec::Idle), None);
        // 닫으면 이번 회의 동안은 다시 묻지 않는다
        d.close();
        assert_eq!(d.tick(secs(t0, 30), Some("Zoom"), Rec::Idle), None);
        assert_eq!(d.prompt(), None);
        // 회의가 끝나고 새로 시작하면 다시 묻는다
        assert_eq!(d.tick(secs(t0, 40), None, Rec::Idle), None);
        assert_eq!(d.tick(secs(t0, 50), None, Rec::Idle), None);
        assert_eq!(d.tick(secs(t0, 60), Some("Teams"), Rec::Idle), None);
        assert_eq!(d.tick(secs(t0, 64), Some("Teams"), Rec::Idle), Some(Some(Prompt::Start { app: "Teams".into() })));
    }

    #[test]
    fn start_prompt_goes_away_when_no_longer_needed() {
        let t0 = Instant::now();
        let mut d = Detector::default();
        d.tick(t0, Some("Zoom"), Rec::Idle);
        assert!(d.tick(secs(t0, 3), Some("Zoom"), Rec::Idle).is_some());
        // 사람이 직접 녹음을 시작함 → 닫는다
        assert_eq!(d.tick(secs(t0, 5), Some("Zoom"), Rec::Recording), Some(None));
        // 답 없이 오래 두면 닫는다
        let mut d = Detector::default();
        d.tick(t0, Some("Zoom"), Rec::Idle);
        d.tick(secs(t0, 3), Some("Zoom"), Rec::Idle);
        assert_eq!(d.tick(secs(t0, 62), Some("Zoom"), Rec::Idle), None);
        assert_eq!(d.tick(secs(t0, 63), Some("Zoom"), Rec::Idle), Some(None));
        // 답하기 전에 회의가 끝남 → 닫는다
        let mut d = Detector::default();
        d.tick(t0, Some("Zoom"), Rec::Idle);
        d.tick(secs(t0, 3), Some("Zoom"), Rec::Idle);
        d.tick(secs(t0, 10), None, Rec::Idle);
        assert_eq!(d.tick(secs(t0, 18), None, Rec::Idle), Some(None));
    }

    #[test]
    fn asks_to_stop_when_the_meeting_ends() {
        let t0 = Instant::now();
        let mut d = Detector::default();
        d.tick(t0, Some("Zoom"), Rec::Idle);
        d.tick(secs(t0, 3), Some("Zoom"), Rec::Idle);
        d.started(secs(t0, 4));
        assert_eq!(d.prompt(), Some(&Prompt::Started { app: "Zoom".into() }));
        // 모델을 불러오느라 시작이 오래 걸려도 닫지 않는다
        assert_eq!(d.tick(secs(t0, 12), Some("Zoom"), Rec::Busy), None);
        d.started(secs(t0, 13));
        assert_eq!(d.tick(secs(t0, 15), Some("Zoom"), Rec::Recording), None);
        assert_eq!(d.tick(secs(t0, 18), Some("Zoom"), Rec::Recording), Some(None));
        // 잠깐 놓았다 다시 잡는 것(장치 바꾸기)은 끝이 아니다
        d.tick(secs(t0, 100), None, Rec::Recording);
        d.tick(secs(t0, 104), Some("Zoom"), Rec::Recording);
        assert_eq!(d.tick(secs(t0, 112), None, Rec::Recording), None);
        assert_eq!(d.tick(secs(t0, 120), None, Rec::Recording), Some(Some(Prompt::End { app: "Zoom".into() })));
        // 사람이 멈추면 닫는다
        assert_eq!(d.tick(secs(t0, 125), None, Rec::Busy), Some(None));
    }

    #[test]
    fn follows_recordings_started_by_hand() {
        let t0 = Instant::now();
        // 녹음 중에 회의가 시작됨 → 묻지 않고, 끝나면 멈출지 묻는다
        let mut d = Detector::default();
        d.tick(t0, Some("Teams"), Rec::Recording);
        assert_eq!(d.tick(secs(t0, 3), Some("Teams"), Rec::Recording), None);
        d.tick(secs(t0, 10), None, Rec::Recording);
        assert_eq!(d.tick(secs(t0, 18), None, Rec::Recording), Some(Some(Prompt::End { app: "Teams".into() })));
        // 회의가 다시 이어지면 "멈출까요?" 는 거둔다
        d.tick(secs(t0, 20), Some("Teams"), Rec::Recording);
        assert_eq!(d.tick(secs(t0, 23), Some("Teams"), Rec::Recording), Some(None));
        // 녹음하지 않은 회의는 끝나도 묻지 않는다
        let mut d = Detector::default();
        d.tick(t0, Some("Zoom"), Rec::Idle);
        d.tick(secs(t0, 3), Some("Zoom"), Rec::Idle);
        d.close();
        d.tick(secs(t0, 10), None, Rec::Idle);
        assert_eq!(d.tick(secs(t0, 18), None, Rec::Idle), None);
        // 파일을 받아 적는 중이면 묻지 않는다
        let mut d = Detector::default();
        d.tick(t0, Some("Zoom"), Rec::Busy);
        assert_eq!(d.tick(secs(t0, 3), Some("Zoom"), Rec::Busy), None);
    }

    /// 실제 레지스트리를 읽어 본다: `cargo test meeting::tests::live_mic_users -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_mic_users() {
        println!("오디오 세션: {:?}", session_users());
        println!("사용 기록: {:?}", registry_users());
        let users = mic_users();
        println!("회의 앱: {:?}", meeting_app(&users, &all()));
    }
}
