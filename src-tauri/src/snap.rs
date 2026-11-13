//! 창 자석 효과 · 미니 창 자리 잡기 (Windows)
//!
//! 창을 끌어서 옮길 때(Windows 이동 루프의 WM_MOVING) 화면 작업 영역 가장자리에 가까워지면
//! 착 달라붙게 한다. 작업 영역은 작업 표시줄을 뺀 영역이라, 아래쪽에 붙이면 작업 표시줄 바로 위에 붙는다.
//! 모니터가 여러 개면 창이 가장 많이 걸친 모니터 기준.

/// 이동이 끝났을 때(마우스를 놓았을 때) 부를 함수
pub type MoveEnd = Box<dyn Fn() + Send + Sync>;

/// (왼쪽, 위, 오른쪽, 아래)
type Rect4 = (i32, i32, i32, i32);

/// 보이는 창 테두리가 작업 영역 가장자리에서 `d` 안쪽이면 그 가장자리로 옮길 거리 (dx, dy)
fn snap_offset(visible: Rect4, work: Rect4, d: i32) -> (i32, i32) {
    let (vl, vt, vr, vb) = visible;
    let (wl, wt, wr, wb) = work;
    let dx = if (vl - wl).abs() <= d {
        wl - vl
    } else if (vr - wr).abs() <= d {
        wr - vr
    } else {
        0
    };
    let dy = if (vt - wt).abs() <= d {
        wt - vt
    } else if (vb - wb).abs() <= d {
        wb - vb
    } else {
        0
    };
    (dx, dy)
}

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromRect, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, WM_EXITSIZEMOVE, WM_MOVING,
    };

    use super::MoveEnd;

    /// 이만큼(96 DPI 기준 픽셀) 가까워지면 달라붙는다
    const SNAP_PX: i32 = 18;

    static ON_MOVE_END: OnceLock<MoveEnd> = OnceLock::new();

    /// 창 사각형과 눈에 보이는 테두리 사이 여백 (왼쪽, 위, 오른쪽, 아래)
    unsafe fn insets(hwnd: HWND) -> (i32, i32, i32, i32) {
        let mut wr: RECT = std::mem::zeroed();
        let mut fr: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut wr) == 0
            || DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut fr as *mut RECT as *mut core::ffi::c_void,
                std::mem::size_of::<RECT>() as u32,
            ) != 0
        {
            return (0, 0, 0, 0);
        }
        (fr.left - wr.left, fr.top - wr.top, wr.right - fr.right, wr.bottom - fr.bottom)
    }

    unsafe fn work_area(mon: HMONITOR) -> Option<RECT> {
        if mon.is_null() {
            return None;
        }
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        (GetMonitorInfoW(mon, &mut mi) != 0).then_some(mi.rcWork)
    }

    unsafe fn snap(hwnd: HWND, r: &mut RECT) {
        let Some(work) = work_area(MonitorFromRect(r, MONITOR_DEFAULTTONEAREST)) else {
            return;
        };
        let (il, it, ir, ib) = insets(hwnd);
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let d = SNAP_PX * dpi / 96;
        let (dx, dy) = super::snap_offset(
            (r.left + il, r.top + it, r.right - ir, r.bottom - ib),
            (work.left, work.top, work.right, work.bottom),
            d,
        );
        r.left += dx;
        r.right += dx;
        r.top += dy;
        r.bottom += dy;
    }

    unsafe extern "system" fn subclass(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        match msg {
            WM_MOVING if lparam != 0 => {
                snap(hwnd, &mut *(lparam as *mut RECT));
                return 1;
            }
            WM_EXITSIZEMOVE => {
                if let Some(f) = ON_MOVE_END.get() {
                    f();
                }
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    pub fn install(hwnd: *mut core::ffi::c_void, on_end: MoveEnd) {
        let _ = ON_MOVE_END.set(on_end);
        unsafe {
            SetWindowSubclass(hwnd as HWND, Some(subclass), 0x5341_5050, 0);
        }
    }

    /// 지금 모니터 작업 영역의 오른쪽 아래(작업 표시줄 시계 바로 위)에 붙인다
    pub fn dock_bottom_right(hwnd: *mut core::ffi::c_void) {
        let hwnd = hwnd as HWND;
        unsafe {
            let mut wr: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut wr) == 0 {
                return;
            }
            let Some(work) = work_area(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)) else {
                return;
            };
            let (_, _, ir, ib) = insets(hwnd);
            let (w, h) = (wr.right - wr.left, wr.bottom - wr.top);
            let x = work.right - w + ir;
            let y = work.bottom - h + ib;
            SetWindowPos(hwnd, std::ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

/// 창에 자석 효과를 단다 (메인 스레드에서 한 번)
pub fn install(window: &tauri::WebviewWindow, on_end: MoveEnd) {
    #[cfg(windows)]
    if let Ok(h) = window.hwnd() {
        imp::install(h.0, on_end);
    }
    #[cfg(not(windows))]
    let _ = (window, on_end);
}

/// 작업 표시줄 시계 위(작업 영역 오른쪽 아래)로
pub fn dock_bottom_right(window: &tauri::WebviewWindow) {
    #[cfg(windows)]
    if let Ok(h) = window.hwnd() {
        imp::dock_bottom_right(h.0);
    }
    #[cfg(not(windows))]
    let _ = window;
}

#[cfg(test)]
mod tests {
    use super::snap_offset;

    const WORK: (i32, i32, i32, i32) = (0, 0, 1920, 1032); // 작업 표시줄 48px 를 뺀 1080p

    #[test]
    fn snaps_to_nearby_edges() {
        // 오른쪽 아래 모서리 근처 → 작업 표시줄 바로 위 오른쪽 끝에 붙음
        assert_eq!(snap_offset((1540, 940, 1910, 1024), WORK, 18), (10, 8));
        // 왼쪽 위
        assert_eq!(snap_offset((12, -5, 432, 675), WORK, 18), (-12, 5));
    }

    #[test]
    fn leaves_window_alone_away_from_edges() {
        assert_eq!(snap_offset((500, 300, 920, 980), WORK, 18), (0, 0));
        // 딱 경계를 넘은 거리
        assert_eq!(snap_offset((19, 300, 439, 980), WORK, 18), (0, 0));
    }
}
