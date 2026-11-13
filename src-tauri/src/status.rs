//! 상태 표시 — 트레이 아이콘 · 트레이 툴팁 · 작업 표시줄 단추(배지 · 진행 막대)
//!
//! | 상태 | 트레이 아이콘 점 | 작업 표시줄 |
//! |---|---|---|
//! | 대기 | 없음 | 없음 |
//! | 새 버전 준비됨 (대기 중) | 초록 점 | 없음 |
//! | 시작 중 · 정리 중 | 파란 고리 | 파란 배지 (정리 중엔 흐르는 막대) |
//! | 녹음 중 | 빨간 점 | 빨간 배지 |
//! | 일시정지 | 노란 점 + 멈춤 표시 | 노란 배지 |
//! | 파일 받아쓰기 | 파란 고리 | 진행 막대 (일시정지면 노란 막대) |
//!
//! 툴팁에는 녹음 경과 시간 · 파일 진행률을 1초마다 보여 준다.

use std::sync::atomic::Ordering;
use std::time::Duration;

use parking_lot::Mutex;
use tauri::image::Image;
use tauri::window::{ProgressBarState, ProgressBarStatus};
use tauri::{AppHandle, Manager};

use crate::update::Updates;
use crate::{AppState, RecState};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Idle,
    Update,
    Busy,
    Rec,
    Paused,
    File,
}

#[derive(Clone, Copy)]
enum Glyph {
    Solid,
    Ring,
    Pause,
}

const RED: [u8; 3] = [239, 68, 68];
const AMBER: [u8; 3] = [245, 158, 11];
const BLUE: [u8; 3] = [59, 130, 246];
const GREEN: [u8; 3] = [34, 197, 94];

/// 동그란 배지를 (cx, cy, r) 에 그린다 (흰 테두리 + 안쪽 모양)
fn draw_badge(px: &mut [u8], w: i32, h: i32, (cx, cy, r): (f32, f32, f32), color: [u8; 3], glyph: Glyph) {
    let bar_w = r * 0.30;
    let bar_h = r * 0.95;
    let bar_gap = r * 0.30;
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
            let i = ((y * w + x) * 4) as usize;
            if d <= r - 1.0 {
                let white = match glyph {
                    Glyph::Solid => false,
                    Glyph::Ring => d <= r * 0.42,
                    Glyph::Pause => {
                        let dx = (fx - cx).abs();
                        (fy - cy).abs() <= bar_h / 2.0 && dx >= bar_gap / 2.0 && dx <= bar_gap / 2.0 + bar_w
                    }
                };
                let c = if white { [255, 255, 255] } else { color };
                px[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            } else if d <= r + 0.6 {
                px[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
}

/// 트레이 아이콘 오른쪽 아래에 배지를 얹은 아이콘
fn tray_icon(base: &Image<'_>, color: [u8; 3], glyph: Glyph) -> Image<'static> {
    let (w, h) = (base.width() as i32, base.height() as i32);
    let mut px = base.rgba().to_vec();
    let r = (w as f32 * 0.26).max(4.0);
    draw_badge(&mut px, w, h, (w as f32 - r - 0.5, h as f32 - r - 0.5, r), color, glyph);
    Image::new_owned(px, w as u32, h as u32)
}

/// 작업 표시줄 단추에 얹을 배지만 (투명 바탕)
fn overlay(color: [u8; 3], glyph: Glyph) -> Image<'static> {
    let s = 32;
    let mut px = vec![0u8; (s * s * 4) as usize];
    draw_badge(&mut px, s, s, (16.0, 16.0, 14.5), color, glyph);
    Image::new_owned(px, s as u32, s as u32)
}

struct Last {
    kind: Option<Kind>,
    tip: String,
    progress: Option<u64>,
}

pub struct Status {
    idle: Image<'static>,
    update: Image<'static>,
    busy: Image<'static>,
    rec: Image<'static>,
    paused: Image<'static>,
    badge_busy: Image<'static>,
    badge_rec: Image<'static>,
    badge_paused: Image<'static>,
    last: Mutex<Last>,
}

impl Status {
    pub fn new(base: Image<'static>) -> Self {
        Self {
            update: tray_icon(&base, GREEN, Glyph::Solid),
            busy: tray_icon(&base, BLUE, Glyph::Ring),
            rec: tray_icon(&base, RED, Glyph::Solid),
            paused: tray_icon(&base, AMBER, Glyph::Pause),
            idle: base,
            badge_busy: overlay(BLUE, Glyph::Ring),
            badge_rec: overlay(RED, Glyph::Solid),
            badge_paused: overlay(AMBER, Glyph::Pause),
            last: Mutex::new(Last {
                kind: None,
                tip: String::new(),
                progress: None,
            }),
        }
    }

    pub fn idle_icon(&self) -> Image<'static> {
        self.idle.clone()
    }
}

fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// 지금 상태에 맞게 트레이 · 작업 표시줄을 고친다 (바뀐 것만)
pub fn refresh(app: &AppHandle) {
    let (Some(st), Some(status)) = (app.try_state::<AppState>(), app.try_state::<Status>()) else {
        return;
    };
    let rec = *st.rec.lock();
    let paused = st.pipeline.lock().as_ref().is_some_and(|p| p.is_paused());
    let file = st.file_mode.load(Ordering::SeqCst);
    // 0 = 모름, n = (n-1)%
    let pct = st.file_pct.load(Ordering::SeqCst).checked_sub(1).map(|p| p as u64);
    let update_ready = app.try_state::<Updates>().is_some_and(|u| u.is_ready());
    let elapsed = st.rec_started.lock().map(|t| clock(t.elapsed()));

    let kind = match rec {
        RecState::Idle if update_ready => Kind::Update,
        RecState::Idle => Kind::Idle,
        RecState::Starting | RecState::Stopping => Kind::Busy,
        RecState::Recording if paused => Kind::Paused,
        RecState::Recording if file => Kind::File,
        RecState::Recording => Kind::Rec,
    };
    let tip = match (kind, rec) {
        (Kind::Update, _) => "받아쓰기 — 대기 중 · 새 버전 준비됨 (쓰지 않을 때 적용)".to_string(),
        (Kind::Idle, _) => "받아쓰기 — 대기 중".to_string(),
        (_, RecState::Starting) => "받아쓰기 — 시작하는 중…".to_string(),
        (_, RecState::Stopping) => "받아쓰기 — 남은 발화를 정리하는 중…".to_string(),
        (Kind::Paused, _) => format!("받아쓰기 — 일시정지{}", elapsed.map(|e| format!(" · {e}")).unwrap_or_default()),
        (Kind::File, _) => match pct {
            Some(p) => format!("받아쓰기 — 파일 받아 적는 중 {p}%"),
            None => "받아쓰기 — 파일 받아 적는 중".to_string(),
        },
        _ => format!("받아쓰기 — 녹음 중{}", elapsed.map(|e| format!(" · {e}")).unwrap_or_default()),
    };
    let progress = match kind {
        Kind::File | Kind::Paused if file => Some(pct.unwrap_or(0)),
        _ => None,
    };

    let mut last = status.last.lock();
    let kind_changed = last.kind != Some(kind);
    if let Some(tray) = app.tray_by_id("main") {
        if kind_changed {
            let icon = match kind {
                Kind::Idle => &status.idle,
                Kind::Update => &status.update,
                Kind::Busy | Kind::File => &status.busy,
                Kind::Rec => &status.rec,
                Kind::Paused => &status.paused,
            };
            let _ = tray.set_icon(Some(icon.clone()));
        }
        if last.tip != tip {
            let _ = tray.set_tooltip(Some(tip.as_str()));
        }
    }
    if let Some(w) = app.get_webview_window("main") {
        if kind_changed {
            let badge = match kind {
                Kind::Rec => Some(status.badge_rec.clone()),
                Kind::Paused => Some(status.badge_paused.clone()),
                Kind::Busy => Some(status.badge_busy.clone()),
                // 파일은 진행 막대로 보여 준다
                Kind::File | Kind::Idle | Kind::Update => None,
            };
            let _ = w.set_overlay_icon(badge);
        }
        if kind_changed || last.progress != progress {
            let state = match (kind, rec) {
                (_, RecState::Stopping) => ProgressBarState {
                    status: Some(ProgressBarStatus::Indeterminate),
                    progress: None,
                },
                (Kind::Paused, _) if file => ProgressBarState {
                    status: Some(ProgressBarStatus::Paused),
                    progress,
                },
                (Kind::File, _) => ProgressBarState {
                    status: Some(ProgressBarStatus::Normal),
                    progress,
                },
                _ => ProgressBarState {
                    status: Some(ProgressBarStatus::None),
                    progress: None,
                },
            };
            let _ = w.set_progress_bar(state);
        }
    }
    last.kind = Some(kind);
    last.tip = tip;
    last.progress = progress;
}

/// 녹음 중에는 1초마다 툴팁의 경과 시간을 고친다
pub fn spawn_ticker(app: AppHandle) {
    let _ = std::thread::Builder::new().name("bd-status".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let recording = app
            .try_state::<AppState>()
            .is_some_and(|st| *st.rec.lock() == RecState::Recording);
        if recording {
            refresh(&app);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BADASUGI_DUMP_ICONS=<폴더> 로 실행하면 아이콘을 RGBA 원본으로 내보낸다 (눈으로 확인용)
    #[test]
    fn dump_icons_for_review() {
        let Ok(dir) = std::env::var("BADASUGI_DUMP_ICONS") else {
            return;
        };
        let base = Image::from_bytes(include_bytes!("../icons/tray.png")).unwrap().to_owned();
        let s = Status::new(base);
        for (name, img) in [
            ("1idle", &s.idle),
            ("2update", &s.update),
            ("3busy", &s.busy),
            ("4rec", &s.rec),
            ("5paused", &s.paused),
            ("6badge_busy", &s.badge_busy),
            ("7badge_rec", &s.badge_rec),
            ("8badge_paused", &s.badge_paused),
        ] {
            std::fs::write(format!("{dir}/{name}_{}.rgba", img.width()), img.rgba()).unwrap();
        }
    }
}
