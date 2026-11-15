//! 절전(잠자기) 감지 — 녹음 중에 PC 가 잠들면 녹음을 일시정지한다
//!
//! 노트북 덮개를 닫은 채 가방에 넣었다가 다시 열면, 그동안 멈춰 있던 녹음이 저절로 이어져
//! 엉뚱한 소리까지 받아 적게 된다. 그래서 잠들었다 깨어나면 녹음을 일시정지 상태로 두고 알린다.
//!
//! 요즘 노트북(모던 스탠바이, S0 저전력 유휴)은 데스크톱 앱에 절전 알림이 오지 않을 수 있어 두 길을 함께 쓴다.
//! 1. 창으로 받는 절전 · 깨어남 알림 (`RegisterSuspendResumeNotification` → `WM_POWERBROADCAST`)
//! 2. 2초마다 도는 감시 스레드: 벽시계가 한 번에 [`GAP`] 넘게 건너뛰면 "잠들었다 깨어남"으로 본다
//!    (잠든 동안은 프로세스가 멈춰 소리도 안 들어오므로, 깨어난 직후에 멈춰도 잃는 녹음은 없다)

use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

/// 감시 스레드가 도는 간격
const TICK: Duration = Duration::from_secs(2);
/// 이보다 오래 멈췄다 깨어나면 잠들었던 것으로 본다
pub const GAP: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    /// 곧 잠든다 (알림을 받은 경우만)
    Suspend,
    /// 깨어났다. 감시 스레드가 알아챈 경우 멈춰 있던 시간(초)
    Resume { slept_secs: Option<u64> },
}

pub type Handler = Box<dyn Fn(PowerEvent) + Send + Sync>;

static HANDLER: OnceLock<Handler> = OnceLock::new();

fn fire(ev: PowerEvent) {
    if let Some(h) = HANDLER.get() {
        h(ev);
    }
}

/// 벽시계가 `prev` → `now` 사이에 잠들었다 깰 만큼 건너뛰었나 (뒤로 간 시계는 무시)
fn slept(prev: SystemTime, now: SystemTime) -> Option<u64> {
    let gap = now.duration_since(prev).ok()?;
    (gap >= GAP).then(|| gap.as_secs())
}

/// 절전 알림을 받기 시작한다 (메인 창, 앱 시작 때 한 번)
pub fn install(window: &tauri::WebviewWindow, handler: Handler) {
    if HANDLER.set(handler).is_err() {
        return;
    }
    #[cfg(windows)]
    if let Ok(h) = window.hwnd() {
        imp::install(h.0);
    }
    #[cfg(not(windows))]
    let _ = window;
    let _ = std::thread::Builder::new().name("power-watch".into()).spawn(|| {
        let mut prev = SystemTime::now();
        loop {
            std::thread::sleep(TICK);
            let now = SystemTime::now();
            if let Some(secs) = slept(prev, now) {
                log::info!("{secs}초 동안 멈춰 있다 깨어남 (절전으로 보임)");
                fire(PowerEvent::Resume { slept_secs: Some(secs) });
            }
            prev = now;
        }
    });
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Power::RegisterSuspendResumeNotification;
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DEVICE_NOTIFY_WINDOW_HANDLE, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND, WM_POWERBROADCAST,
    };

    use super::{fire, PowerEvent};

    unsafe extern "system" fn subclass(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if msg == WM_POWERBROADCAST {
            match wparam as u32 {
                PBT_APMSUSPEND => {
                    log::info!("절전 알림 받음");
                    fire(PowerEvent::Suspend);
                }
                // 자동으로 깨어남(AUTOMATIC) 뒤에 사람이 깨운 경우(RESUMESUSPEND)가 또 올 수 있다 — 받는 쪽에서 한 번만 처리
                PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
                    log::info!("깨어남 알림 받음");
                    fire(PowerEvent::Resume { slept_secs: None });
                }
                _ => {}
            }
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    pub fn install(hwnd: *mut core::ffi::c_void) {
        unsafe {
            SetWindowSubclass(hwnd as HWND, Some(subclass), 0x504F_5752, 0);
            // 모던 스탠바이 PC 는 등록한 창에만 절전 알림을 보낸다
            if RegisterSuspendResumeNotification(hwnd, DEVICE_NOTIFY_WINDOW_HANDLE) == 0 {
                log::warn!("절전 알림 등록 실패 — 시계 감시로만 알아챔");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_clock_jump_means_sleep() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(slept(t, t + Duration::from_secs(2)), None);
        assert_eq!(slept(t, t + Duration::from_secs(30)), None);
        assert_eq!(slept(t, t + Duration::from_secs(600)), Some(600));
        // 시계가 뒤로 감 (시간 맞춤) — 잠든 게 아님
        assert_eq!(slept(t, t - Duration::from_secs(600)), None);
    }
}
