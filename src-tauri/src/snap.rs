//! 창 자석 효과 · 미니 창 자리 잡기 (Windows)
//!
//! - 창을 끌어서 옮길 때(Windows 이동 루프의 WM_MOVING) 화면 작업 영역 가장자리에 가까워지면
//!   착 달라붙게 한다. 작업 영역은 작업 표시줄을 뺀 영역이라, 아래쪽에 붙이면 작업 표시줄 바로 위에 붙는다.
//! - 배율(DPI) · 해상도 · 작업 표시줄이 바뀌면(원격 접속 ↔ 노트북 화면 등) 알려 주어 창을 다시 자리 잡게 한다.
//! - 모니터가 여러 개면 창이 가장 많이 걸친 모니터 기준.

/// 창 이벤트를 받을 함수
pub type Callback = Box<dyn Fn() + Send + Sync>;

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

/// 보이는 창이 작업 영역 밖으로 삐져나갔으면 안으로 들여올 거리 (dx, dy).
/// 창이 작업 영역보다 크면 왼쪽 위를 맞춘다 (제목 줄이 보이게).
fn clamp_offset(visible: Rect4, work: Rect4) -> (i32, i32) {
    let (vl, vt, vr, vb) = visible;
    let (wl, wt, wr, wb) = work;
    let axis = |lo: i32, hi: i32, wlo: i32, whi: i32| {
        if hi - lo >= whi - wlo || lo < wlo {
            wlo - lo
        } else if hi > whi {
            whi - hi
        } else {
            0
        }
    };
    (axis(vl, vr, wl, wr), axis(vt, vb, wt, wb))
}

/// 알림 창 자리(보이는 테두리의 왼쪽 위): 작업 영역 오른쪽 아래에서 `gap` 안쪽.
/// 그 자리를 `avoid`(시계 위에 붙은 미니 창)가 차지하면 그 위로 — 올려서 화면을 벗어나면 그냥 둔다
fn popup_spot(size: (i32, i32), work: Rect4, avoid: Option<Rect4>, gap: i32) -> (i32, i32) {
    let (w, h) = size;
    let right = work.2 - gap;
    let mut bottom = work.3 - gap;
    if let Some((al, at, ar, ab)) = avoid {
        if al < right && ar > right - w && at < bottom && ab > bottom - h && at - gap - h >= work.1 {
            bottom = at - gap;
        }
    }
    (right - w, bottom - h)
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
        GetWindowRect, IsIconic, IsWindowVisible, SetWindowPos, ShowWindow, HWND_TOPMOST, SPI_SETWORKAREA,
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_EXITSIZEMOVE,
        WM_MOVING, WM_SETTINGCHANGE,
    };

    use super::{Callback, Rect4};

    /// 이만큼(96 DPI 기준 픽셀) 가까워지면 달라붙는다
    const SNAP_PX: i32 = 18;

    static ON_MOVE_END: OnceLock<Callback> = OnceLock::new();
    static ON_DISPLAY: OnceLock<Callback> = OnceLock::new();

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

    /// (창 사각형, 보이는 테두리, 지금 모니터 작업 영역)
    unsafe fn geometry(hwnd: HWND) -> Option<(RECT, Rect4, Rect4)> {
        let mut wr: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut wr) == 0 {
            return None;
        }
        let work = work_area(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST))?;
        let (il, it, ir, ib) = insets(hwnd);
        Some((
            wr,
            (wr.left + il, wr.top + it, wr.right - ir, wr.bottom - ib),
            (work.left, work.top, work.right, work.bottom),
        ))
    }

    unsafe fn move_by(hwnd: HWND, wr: &RECT, dx: i32, dy: i32) {
        if dx != 0 || dy != 0 {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                wr.left + dx,
                wr.top + dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
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
            // 배율 · 해상도 · 작업 표시줄 크기가 바뀜: 기본 처리(창 크기 조정)를 먼저 한 뒤 알린다
            WM_DPICHANGED | WM_DISPLAYCHANGE => {
                let r = DefSubclassProc(hwnd, msg, wparam, lparam);
                if let Some(f) = ON_DISPLAY.get() {
                    f();
                }
                return r;
            }
            WM_SETTINGCHANGE if wparam as u32 == SPI_SETWORKAREA => {
                let r = DefSubclassProc(hwnd, msg, wparam, lparam);
                if let Some(f) = ON_DISPLAY.get() {
                    f();
                }
                return r;
            }
            _ => {}
        }
        DefSubclassProc(hwnd, msg, wparam, lparam)
    }

    pub fn install(hwnd: *mut core::ffi::c_void, on_move_end: Callback, on_display: Callback) {
        let _ = ON_MOVE_END.set(on_move_end);
        let _ = ON_DISPLAY.set(on_display);
        unsafe {
            SetWindowSubclass(hwnd as HWND, Some(subclass), 0x5341_5050, 0);
        }
    }

    /// 지금 모니터 작업 영역의 오른쪽 아래(작업 표시줄 시계 바로 위)에 붙인다
    pub fn dock_bottom_right(hwnd: *mut core::ffi::c_void) {
        let hwnd = hwnd as HWND;
        unsafe {
            let Some((wr, (_, _, vr, vb), (_, _, wkr, wkb))) = geometry(hwnd) else {
                return;
            };
            move_by(hwnd, &wr, wkr - vr, wkb - vb);
        }
    }

    /// 작업 영역 밖으로 삐져나간 만큼 안으로 들여온다
    pub fn keep_inside(hwnd: *mut core::ffi::c_void) {
        let hwnd = hwnd as HWND;
        unsafe {
            if let Some((wr, vis, work)) = geometry(hwnd) {
                let (dx, dy) = super::clamp_offset(vis, work);
                move_by(hwnd, &wr, dx, dy);
            }
        }
    }

    /// 알림 창을 `near` 창이 있는 모니터의 오른쪽 아래(시계 위)에, 활성화하지 않고(포커스를 빼앗지 않게) 맨 위로 보인다.
    /// `avoid` 면 그 자리에 보이는 `near`(미니 창)를 피해 그 위에.
    pub fn show_popup(hwnd: *mut core::ffi::c_void, near: Option<*mut core::ffi::c_void>, avoid: bool) {
        let hwnd = hwnd as HWND;
        let near = near.map(|n| n as HWND);
        unsafe {
            // 배율이 다른 모니터로 옮기면 창 크기가 바뀌므로 옮긴 뒤 한 번 더 잰다
            for _ in 0..2 {
                let Some((wr, (vl, vt, vr, vb), own)) = geometry(hwnd) else {
                    return;
                };
                let work = near
                    .and_then(|n| work_area(MonitorFromWindow(n, MONITOR_DEFAULTTONEAREST)))
                    .map(|r| (r.left, r.top, r.right, r.bottom))
                    .unwrap_or(own);
                let shown = near.filter(|&n| avoid && IsWindowVisible(n) != 0 && IsIconic(n) == 0);
                let avoid_rect = shown.and_then(|n| geometry(n)).map(|g| g.1);
                let gap = 12 * GetDpiForWindow(hwnd).max(96) as i32 / 96;
                let (x, y) = super::popup_spot((vr - vl, vb - vt), work, avoid_rect, gap);
                SetWindowPos(hwnd, HWND_TOPMOST, x - (vl - wr.left), y - (vt - wr.top), 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
            }
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }

    /// 보이는 창이 작업 영역 오른쪽 아래 모서리에 붙어 있나
    pub fn docked_bottom_right(hwnd: *mut core::ffi::c_void) -> bool {
        unsafe {
            geometry(hwnd as HWND).is_some_and(|(_, (_, _, vr, vb), (_, _, wkr, wkb))| {
                (vr - wkr).abs() <= 3 && (vb - wkb).abs() <= 3
            })
        }
    }
}

/// 창에 자석 효과를 달고, 이동 끝 · 화면 구성 변경을 알려 받는다 (메인 스레드에서 한 번)
pub fn install(window: &tauri::WebviewWindow, on_move_end: Callback, on_display: Callback) {
    #[cfg(windows)]
    if let Ok(h) = window.hwnd() {
        imp::install(h.0, on_move_end, on_display);
    }
    #[cfg(not(windows))]
    let _ = (window, on_move_end, on_display);
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

/// 화면 밖으로 나간 부분이 있으면 안으로
pub fn keep_inside(window: &tauri::WebviewWindow) {
    #[cfg(windows)]
    if let Ok(h) = window.hwnd() {
        imp::keep_inside(h.0);
    }
    #[cfg(not(windows))]
    let _ = window;
}

/// 알림 창을 `near` 창의 모니터 오른쪽 아래(시계 위)에 포커스를 빼앗지 않고 보인다 (`avoid` = 미니 창을 피해 그 위에)
pub fn show_popup(window: &tauri::WebviewWindow, near: Option<&tauri::WebviewWindow>, avoid: bool) {
    #[cfg(windows)]
    if let Ok(h) = window.hwnd() {
        imp::show_popup(h.0, near.and_then(|n| n.hwnd().ok()).map(|n| n.0), avoid);
        return;
    }
    let _ = (near, avoid);
    let _ = window.show();
}

/// 오른쪽 아래 모서리(시계 위)에 붙어 있나
pub fn docked_bottom_right(window: &tauri::WebviewWindow) -> bool {
    #[cfg(windows)]
    if let Ok(h) = window.hwnd() {
        return imp::docked_bottom_right(h.0);
    }
    #[cfg(not(windows))]
    let _ = window;
    false
}

#[cfg(test)]
mod tests {
    use super::{clamp_offset, popup_spot, snap_offset};

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

    #[test]
    fn clamps_window_back_inside() {
        // 원격(100%)에서 저장한 미니 창 자리가 노트북 배율(150%)에서 오른쪽·아래로 삐져나감
        let work_150 = (0, 0, 1920, 1008);
        assert_eq!(clamp_offset((1546, 946, 2104, 1075), work_150), (-184, -67));
        // 안에 있으면 그대로
        assert_eq!(clamp_offset((100, 100, 500, 600), WORK), (0, 0));
        // 왼쪽·위로 나가면 안으로
        assert_eq!(clamp_offset((-30, -10, 390, 670), WORK), (30, 10));
        // 작업 영역보다 크면 왼쪽 위에 맞춤 (제목 줄이 보이게)
        assert_eq!(clamp_offset((50, 40, 2050, 1200), WORK), (-50, -40));
    }

    #[test]
    fn popup_sits_above_the_clock_or_the_mini_window() {
        // 시계 위에서 12px 안쪽
        assert_eq!(popup_spot((344, 118), WORK, None, 12), (1564, 902));
        // 시계 위에 붙은 미니 창이 있으면 그 위로
        assert_eq!(popup_spot((344, 118), WORK, Some((1548, 948, 1920, 1032)), 12), (1564, 818));
        // 미니 창이 다른 곳에 있으면 그대로
        assert_eq!(popup_spot((344, 118), WORK, Some((100, 100, 472, 184)), 12), (1564, 902));
        // 올리면 화면 위로 벗어나는 큰 창이면 그대로
        assert_eq!(popup_spot((344, 118), WORK, Some((1000, 60, 1920, 1032)), 12), (1564, 902));
    }
}
