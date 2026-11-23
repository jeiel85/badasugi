//! 이미 떠 있는 받아쓰기로 실행 명령을 넘기고 곧바로 끝내기 — 바로 가기 · 시작 메뉴 · `badasugi --toggle` 등
//!
//! 두 번째 실행은 원래 tauri 를 다 준비한 뒤(창 관리 · 메뉴 · 트레이 · 플러그인) 단일 실행 플러그인이 앞선 앱에 넘기고 끝났다.
//! 그러느라 넘기기까지 실행 파일(GPU 판 100MB 남짓)에서 31MB 를 읽고 DLL 45개를 올렸고(v0.4.39), 한동안 쉬었다가
//! 바로 가기를 누르면 0.1~0.4초까지 걸렸다. 그래서 `main` 맨 앞에서, 플러그인과 같은 창 · 같은 형식(WM_COPYDATA)으로
//! 바로 넘긴다 (1.4MB · DLL 22개 — 나머지 DLL 은 build.rs 에서 처음 쓸 때 불러오게 했다).
//! 넘길 곳이 없으면(앞선 앱이 없거나, 막 켜지는 중이라 아직 받을 창이 없음) 예전처럼 tauri 를 띄우고 플러그인이 처리한다.
//!
//! 창 이름과 형식은 tauri-plugin-single-instance 2.5 의 `platform_impl/windows.rs` 를 그대로 따른다
//! (플러그인을 올리면 아래 `mirrors_the_locked_plugin_version` 시험이 깨진다 — 형식이 같은지 보고 판 번호를 고친다).

use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, IsWindow, SendMessageTimeoutW, SMTO_NORMAL,
    WM_COPYDATA,
};

/// 플러그인이 받는 형식: 작업 폴더와 인자들을 NUL 로 잇는다 (끝 NUL 없음)
const COPYDATA_ARGS_NUL: usize = 1543;
/// 앞선 앱이 받아 줄 때까지 기다리는 한도 (플러그인과 같다)
const TIMEOUT_MS: u32 = 10_000;

/// 앞선 앱에 넘겼으면(또는 넘기다 늦어졌지만 앞선 앱이 살아 있으면) true — 이 실행은 여기서 끝낸다
pub fn to_running_app() -> bool {
    let data = payload(
        &std::env::current_dir().unwrap_or_default().to_string_lossy(),
        std::env::args_os().map(|a| a.to_string_lossy().into_owned()),
    );
    send(env!("BADASUGI_IDENTIFIER"), &data)
}

fn payload(cwd: &str, args: impl Iterator<Item = String>) -> String {
    let mut data = cwd.to_string();
    for arg in args {
        data.push('\0');
        data.push_str(&arg);
    }
    data
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn send(identifier: &str, data: &str) -> bool {
    let class = wide(&format!("{identifier}-sic"));
    let title = wide(&format!("{identifier}-siw"));
    let hwnd = unsafe { FindWindowW(class.as_ptr(), title.as_ptr()) };
    if hwnd.is_null() {
        return false;
    }
    // 창을 앞으로 가져올 권한은 사용자가 누른 이 실행에만 있다 — 앞선 앱에 넘겨 준다
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != 0 {
        unsafe { AllowSetForegroundWindow(pid) };
    }
    let cds = COPYDATASTRUCT {
        dwData: COPYDATA_ARGS_NUL,
        cbData: data.len() as u32,
        lpData: data.as_ptr() as *mut _,
    };
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            0,
            &cds as *const COPYDATASTRUCT as isize,
            SMTO_NORMAL,
            TIMEOUT_MS,
            std::ptr::null_mut(),
        )
    };
    if sent != 0 {
        return true;
    }
    // 못 보냈다: 앞선 앱이 마침 꺼져 창이 없어졌으면 이 실행이 새로 뜬다.
    // 창이 그대로면 앞선 앱이 바빠 늦는 것 — 늦게라도 받을 수 있으니 같은 명령을 두 번 넘기지 않게 여기서 끝낸다(플러그인도 그렇게 끝낸다)
    unsafe { IsWindow(hwnd) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, UnregisterClassW, WNDCLASSW, WS_POPUP,
    };

    static RECEIVED: Mutex<Option<(usize, Vec<u8>)>> = Mutex::new(None);

    unsafe extern "system" fn test_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_COPYDATA {
            let cds = unsafe { &*(lparam as *const COPYDATASTRUCT) };
            let bytes = unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize) };
            *RECEIVED.lock() = Some((cds.dwData, bytes.to_vec()));
            return 1;
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// 플러그인 받는 쪽(WM_COPYDATA)처럼 숨은 창을 만들어 두고 보내 본다
    #[test]
    fn sends_cwd_and_args_to_the_plugin_window() {
        let id = format!("com.badasugi.test-forward-{}", std::process::id());
        assert!(!send(&id, "x"), "받을 창이 없으면 넘기지 않고 tauri 를 띄워야 한다");

        let class = wide(&format!("{id}-sic"));
        let title = wide(&format!("{id}-siw"));
        let wc = WNDCLASSW {
            lpfnWndProc: Some(test_proc),
            lpszClassName: class.as_ptr(),
            ..unsafe { std::mem::zeroed() }
        };
        unsafe { RegisterClassW(&wc) };
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!hwnd.is_null());

        let data = payload(r"C:\작업", ["badasugi.exe", "--toggle", r"D:\회의 녹음.m4a"].map(String::from).into_iter());
        assert!(send(&id, &data));
        let (kind, bytes) = RECEIVED.lock().take().expect("받은 것이 없음");
        assert_eq!(kind, COPYDATA_ARGS_NUL);
        // 플러그인이 푸는 방식: NUL 로 나눠 첫째가 작업 폴더, 나머지가 인자
        let text = String::from_utf8(bytes).unwrap();
        let parts: Vec<&str> = text.split('\0').collect();
        assert_eq!(parts, [r"C:\작업", "badasugi.exe", "--toggle", r"D:\회의 녹음.m4a"]);

        unsafe {
            DestroyWindow(hwnd);
            UnregisterClassW(class.as_ptr(), std::ptr::null_mut());
        }
    }

    /// 창 이름 · 형식은 잠근 플러그인 판을 따라 썼다 — 플러그인을 올리면 형식이 같은지 보고 여기를 고친다
    #[test]
    fn mirrors_the_locked_plugin_version() {
        let lock = include_str!("../../Cargo.lock");
        let mut lines = lock.lines();
        let found = lines.by_ref().any(|l| l == r#"name = "tauri-plugin-single-instance""#);
        assert!(found);
        let version = lines.next().unwrap_or_default();
        assert!(
            version.starts_with(r#"version = "2.5."#),
            "tauri-plugin-single-instance 판이 바뀜({version}) — platform_impl/windows.rs 의 창 이름 · WM_COPYDATA 형식이 forward.rs 와 같은지 확인"
        );
    }

    #[test]
    fn identifier_comes_from_tauri_config() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        if option_env!("TAURI_CONFIG").is_none() {
            assert_eq!(env!("BADASUGI_IDENTIFIER"), conf["identifier"].as_str().unwrap());
        }
    }
}
