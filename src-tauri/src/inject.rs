//! 지금 커서가 있는 창에 글자 넣기 (받아쓰기 입력 모드)
//!
//! - **붙여넣기**: 클립보드에 글을 넣고 `Ctrl+V` 를 보낸 뒤, 원래 클립보드를 되돌려 놓는다. 넣는 글 · 되돌린 글에는
//!   "클립보드 기록 · 클라우드 클립보드에 넣지 않음" 표시를 붙여 `Win+V` 기록이 어지러워지지 않게 한다.
//!   그림 · 파일 목록 같은 메모리 형식도 그대로 되돌린다 (GDI 핸들 · 앱 전용 형식은 되돌리지 못한다 — 붙여 넣기 전에 그런 것만 있었으면 비워 둔다).
//! - **한 글자씩**: 키 입력 흉내(`SendInput` 유니코드). 클립보드를 건드리지 않는다. 줄 바꿈은 `Shift+Enter` (메신저에서 보내지지 않게).
//!
//! 둘 다 단축키의 Ctrl · Alt · Shift · Win 을 사람이 다 뗀 뒤에 넣는다 (누른 채면 Ctrl+V 가 Ctrl+Alt+V 가 된다).
//! 관리자 권한으로 돈 창에는 Windows 가 넣지 못하게 막는다(UIPI).

use std::time::{Duration, Instant};

/// 넣는 방법
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Paste,
    Type,
}

impl Mode {
    pub fn from_setting(s: &str) -> Mode {
        if s == "type" {
            Mode::Type
        } else {
            Mode::Paste
        }
    }
}

/// 줄 바꿈을 `\r\n` 으로 (붙여넣기 — 메모장 등 Windows 앱이 바라는 꼴)
pub fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

#[cfg(windows)]
pub fn insert(text: &str, mode: Mode) -> Result<(), String> {
    // 누른 채면 Ctrl+V 가 Ctrl+Alt+V 가 되고, 한 글자씩도 단축키처럼 들어간다
    if !imp::wait_keys_up(Duration::from_secs(2)) {
        return Err("단축키(Ctrl · Alt 등)를 아직 누르고 있어 넣지 않았어요".into());
    }
    match mode {
        Mode::Paste => imp::paste(&crlf(text)),
        Mode::Type => imp::type_text(text),
    }
}

#[cfg(not(windows))]
pub fn insert(_text: &str, _mode: Mode) -> Result<(), String> {
    Err("Windows 에서만 돼요".into())
}

/// 글을 클립보드에 넣어 둔다 (넣지 못한 글을 잃지 않게)
#[cfg(windows)]
pub fn copy(text: &str) -> Result<(), String> {
    imp::copy(&crlf(text))
}

#[cfg(not(windows))]
pub fn copy(_text: &str) -> Result<(), String> {
    Err("Windows 에서만 돼요".into())
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
        RegisterClipboardFormatW, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
        VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RETURN, VK_RWIN, VK_SHIFT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};

    /// 단축키의 보조 키를 다 뗄 때까지 (최대 `max`) → 다 뗐으면 true
    pub fn wait_keys_up(max: Duration) -> bool {
        let t = Instant::now();
        loop {
            let held = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
                .iter()
                .any(|&k| unsafe { GetAsyncKeyState(k as i32) } as u16 & 0x8000 != 0);
            if !held {
                return true;
            }
            if t.elapsed() >= max {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 클립보드 주인 창 (보이지 않는 메시지 전용 창) — 주인 없이 열고 비우면 SetClipboardData 가 실패할 수 있다
    /// (OpenClipboard 문서). 이 스레드에서 만들고 다 쓰면 없앤다
    struct Owner(windows_sys::Win32::Foundation::HWND);

    impl Owner {
        fn new() -> Owner {
            let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
            let h = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    std::ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            Owner(h)
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { DestroyWindow(self.0) };
            }
        }
    }

    fn key(vk: VIRTUAL_KEY, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        }
    }

    fn send(inputs: &[INPUT]) -> Result<(), String> {
        if inputs.is_empty() {
            return Ok(());
        }
        let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        if n as usize == inputs.len() {
            Ok(())
        } else {
            Err("글자를 넣지 못했어요 — 관리자 권한으로 실행한 창에는 넣을 수 없어요".into())
        }
    }

    /// 한 글자씩 (유니코드 키 입력) — 줄 바꿈은 Shift+Enter
    pub fn type_text(text: &str) -> Result<(), String> {
        let mut inputs = Vec::new();
        for line_i in text.split('\n').enumerate() {
            let (i, line) = line_i;
            if i > 0 {
                inputs.push(key(VK_SHIFT, 0, 0));
                inputs.push(key(VK_RETURN, 0, 0));
                inputs.push(key(VK_RETURN, 0, KEYEVENTF_KEYUP));
                inputs.push(key(VK_SHIFT, 0, KEYEVENTF_KEYUP));
            }
            for u in line.trim_end_matches('\r').encode_utf16() {
                inputs.push(key(0, u, KEYEVENTF_UNICODE));
                inputs.push(key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
        }
        // 한 번에 너무 많이 보내면 받는 앱이 놓칠 수 있어 나눠 보낸다
        for chunk in inputs.chunks(200) {
            send(chunk)?;
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// 클립보드를 연다 (다른 앱이 잠깐 쥐고 있으면 조금 기다린다)
    fn open(owner: &Owner) -> bool {
        for _ in 0..25 {
            if unsafe { OpenClipboard(owner.0) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// 되돌릴 수 있는 형식인가 — 메모리(HGLOBAL) 형식만. GDI 핸들 형식 · 앱 전용 · 그려 주는 형식은 뺀다
    fn restorable(fmt: u32) -> bool {
        const CF_BITMAP: u32 = 2;
        const CF_METAFILEPICT: u32 = 3;
        const CF_PALETTE: u32 = 9;
        const CF_ENHMETAFILE: u32 = 14;
        !matches!(fmt, CF_BITMAP | CF_METAFILEPICT | CF_PALETTE | CF_ENHMETAFILE) && !(0x80..=0x8E).contains(&fmt) && !(0x200..=0x3FF).contains(&fmt)
    }

    /// 지금 클립보드 (형식, 내용). 너무 크면(64MB) 되돌리지 않는다
    fn save() -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut total = 0usize;
        let mut fmt = 0;
        loop {
            fmt = unsafe { EnumClipboardFormats(fmt) };
            if fmt == 0 {
                break;
            }
            if !restorable(fmt) {
                continue;
            }
            let h = unsafe { GetClipboardData(fmt) } as HGLOBAL;
            if h.is_null() {
                continue;
            }
            let size = unsafe { GlobalSize(h) };
            let p = unsafe { GlobalLock(h) } as *const u8;
            if p.is_null() {
                continue;
            }
            let bytes = unsafe { std::slice::from_raw_parts(p, size) }.to_vec();
            unsafe { GlobalUnlock(h) };
            total += size;
            if total > 64 << 20 {
                return Vec::new();
            }
            out.push((fmt, bytes));
        }
        out
    }

    /// 클립보드에 한 형식을 넣는다 → 넣었으면 true
    fn set(fmt: u32, bytes: &[u8]) -> bool {
        unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
            if h.is_null() {
                return false;
            }
            let p = GlobalLock(h) as *mut u8;
            if p.is_null() {
                GlobalFree(h);
                return false;
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            GlobalUnlock(h);
            // 성공하면 클립보드가 메모리를 가져간다
            if SetClipboardData(fmt, h as HANDLE).is_null() {
                GlobalFree(h);
                return false;
            }
            true
        }
    }

    /// Win+V 클립보드 기록 · 클라우드 클립보드에 넣지 않는다는 표시
    fn keep_out_of_history() {
        let reg = |name: &str| {
            let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            unsafe { RegisterClipboardFormatW(w.as_ptr()) }
        };
        let zero = 0u32.to_le_bytes();
        for (name, data) in [
            ("ExcludeClipboardContentFromMonitorProcessing", &zero[..]),
            ("CanIncludeInClipboardHistory", &zero[..]),
            ("CanUploadToCloudClipboard", &zero[..]),
        ] {
            let f = reg(name);
            if f != 0 {
                set(f, data);
            }
        }
    }

    fn set_text(text: &str) -> bool {
        let mut w: Vec<u16> = text.encode_utf16().collect();
        w.push(0);
        let bytes: Vec<u8> = w.iter().flat_map(|u| u.to_le_bytes()).collect();
        set(CF_UNICODETEXT as u32, &bytes)
    }

    pub fn copy(text: &str) -> Result<(), String> {
        let owner = Owner::new();
        if !open(&owner) {
            return Err("클립보드를 쓸 수 없어요".into());
        }
        unsafe { EmptyClipboard() };
        let ok = set_text(text);
        unsafe { CloseClipboard() };
        if ok {
            Ok(())
        } else {
            Err("클립보드에 글을 넣지 못했어요".into())
        }
    }

    pub fn paste(text: &str) -> Result<(), String> {
        let v = 'V' as VIRTUAL_KEY;
        paste_with(text, || {
            send(&[key(VK_CONTROL, 0, 0), key(v, 0, 0), key(v, 0, KEYEVENTF_KEYUP), key(VK_CONTROL, 0, KEYEVENTF_KEYUP)])
        })
    }

    /// 클립보드에 글을 넣고 `press`(Ctrl+V)를 부른 뒤 원래 클립보드를 되돌린다
    pub fn paste_with(text: &str, press: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let owner = Owner::new();
        if !open(&owner) {
            return Err("클립보드를 쓸 수 없어요 — 다른 프로그램이 쥐고 있어요".into());
        }
        let saved = save();
        unsafe { EmptyClipboard() };
        let put = set_text(text);
        if put {
            keep_out_of_history();
        }
        unsafe { CloseClipboard() };
        // 우리가 넣은 뒤의 클립보드 번호 — 기다리는 사이 누가 복사하면 바뀐다
        let ours = unsafe { GetClipboardSequenceNumber() };

        // 글을 넣지 못했으면 Ctrl+V 를 보내지 않는다 (원래 클립보드를 붙여 넣게 된다)
        let r = if put { press() } else { Err("클립보드에 글을 넣지 못했어요".into()) };
        // 받는 앱이 클립보드를 읽을 틈을 주고 되돌린다
        std::thread::sleep(Duration::from_millis(if r.is_ok() { 350 } else { 0 }));
        if open(&owner) {
            // 그 사이 사람이나 다른 앱이 새로 복사했으면 그것을 지우지 않게 그대로 둔다 — 번호가 바뀌었어도 아직 우리 글이면
            // (원격 데스크톱 rdpclip 처럼 같은 내용을 다시 알린 것) 되돌린다
            let changed = unsafe { GetClipboardSequenceNumber() } != ours;
            if put && changed && current_text().as_deref() != Some(text) {
                unsafe { CloseClipboard() };
                return r;
            }
            unsafe { EmptyClipboard() };
            for (fmt, bytes) in &saved {
                set(*fmt, bytes);
            }
            if !saved.is_empty() {
                keep_out_of_history();
            }
            unsafe { CloseClipboard() };
        }
        r
    }

    /// 지금 클립보드의 글 (클립보드를 연 채로 부른다)
    fn current_text() -> Option<String> {
        let h = unsafe { GetClipboardData(CF_UNICODETEXT as u32) } as HGLOBAL;
        if h.is_null() {
            return None;
        }
        unsafe {
            let p = GlobalLock(h) as *const u16;
            if p.is_null() {
                return None;
            }
            let n = GlobalSize(h) / 2;
            let w = std::slice::from_raw_parts(p, n);
            let end = w.iter().position(|&c| c == 0).unwrap_or(n);
            let s = String::from_utf16_lossy(&w[..end]);
            GlobalUnlock(h);
            Some(s)
        }
    }

    /// 지금 클립보드의 글 (시험용)
    #[cfg(test)]
    pub fn read_text() -> Option<String> {
        let owner = Owner::new();
        if !open(&owner) {
            return None;
        }
        let out = current_text();
        unsafe { CloseClipboard() };
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 실제 클립보드로: 붙여넣는 동안은 받아 적은 글, 끝나면 원래 글로 돌아온다 (키는 보내지 않는다).
    /// `cargo test -p badasugi inject::tests::paste_restores_the_clipboard -- --ignored` (쓰던 클립보드는 되돌려 놓는다)
    /// 클립보드 시험은 하나씩 (같은 클립보드를 쓰므로 함께 돌면 서로 덮는다)
    #[cfg(windows)]
    static CLIPBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn paste_restores_the_clipboard() {
        let _one = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
        let mut seen = None;
        // 바깥 붙여넣기가 시험 전 클립보드(그림 등 무엇이든)를 지켜 두었다가 시험 뒤에 되돌린다
        imp::paste_with("시험 준비", || {
            imp::copy("원래 클립보드 글 🙂")?;
            let mut during = None;
            imp::paste_with("받아 적은 글\r\n둘째 줄", || {
                during = imp::read_text();
                Ok(())
            })?;
            seen = Some((during, imp::read_text()));
            Ok(())
        })
        .unwrap();
        let (during, after) = seen.unwrap();
        assert_eq!(during.as_deref(), Some("받아 적은 글\r\n둘째 줄"));
        assert_eq!(after.as_deref(), Some("원래 클립보드 글 🙂"));
    }

    /// 붙여 넣은 뒤 기다리는 사이 다른 앱이 복사하면 그것을 지우지 않는다 (원래 클립보드로 되돌리지 않음)
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn paste_keeps_a_copy_made_meanwhile() {
        let _one = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
        let mut after = None;
        imp::paste_with("시험 준비", || {
            imp::copy("원래 클립보드 글")?;
            imp::paste_with("받아 적은 글", || imp::copy("그 사이 복사한 글"))?;
            after = imp::read_text();
            Ok(())
        })
        .unwrap();
        assert_eq!(after.as_deref(), Some("그 사이 복사한 글"));
    }

    #[test]
    fn modes_and_line_breaks() {
        assert_eq!(Mode::from_setting("type"), Mode::Type);
        assert_eq!(Mode::from_setting("paste"), Mode::Paste);
        assert_eq!(Mode::from_setting(""), Mode::Paste);
        assert_eq!(crlf("가\n나\r\n다"), "가\r\n나\r\n다");
    }
}
