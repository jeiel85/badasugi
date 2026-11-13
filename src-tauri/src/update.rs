//! 자동 업데이트 — GitHub Releases 의 latest.json 을 확인하고 minisign 서명을 검증한 뒤 설치한다.
//!
//! - 앱 시작 20초 뒤, 그리고 6시간마다 확인
//! - "자동 업데이트" 가 켜져 있으면 새 버전을 백그라운드에서 미리 받아 두고,
//!   녹음·요약·모델 다운로드 중이 아니고 창이 숨겨져 있을 때(트레이) 조용히 설치 → 앱이 저절로 다시 켜진다
//! - 창이 보이는 동안에는 상단 배너로 알리고, 누르면 바로 설치

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::{AppState, RecState};

/// 업데이트 설치 뒤 다시 켜질 때 창을 띄우지 않도록 남기는 표시
pub const START_HIDDEN_MARKER: &str = "start-hidden";

#[derive(Default)]
pub struct Updates {
    ready: Mutex<Option<(Update, Vec<u8>)>>,
    busy: AtomicBool,
    pub status: Mutex<Option<UpdateStatus>>,
    /// 마지막으로 서버에 물어본 때
    last_check: Mutex<Option<Instant>>,
}

/// 창을 열 때 이만큼 지났으면 한 번 더 확인한다 (6시간 주기를 기다리지 않게)
const STALE: Duration = Duration::from_secs(30 * 60);

/// 창을 열었을 때: 확인한 지 30분이 넘었으면 백그라운드에서 새 버전을 확인
pub fn check_if_stale(app: &AppHandle) {
    let Some(updates) = app.try_state::<Updates>() else {
        return;
    };
    let stale = updates.last_check.lock().is_none_or(|t| t.elapsed() > STALE);
    let auto = app.try_state::<AppState>().is_some_and(|st| st.settings.lock().auto_update);
    if !stale || !auto || updates.ready.lock().is_some() {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new().name("bd-update-now".into()).spawn(move || {
        if let Err(e) = tauri::async_runtime::block_on(check(&app, false)) {
            log::info!("업데이트 확인 실패: {e}");
        }
    });
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    /// "available" | "downloading" | "ready" | "installing" | "error"
    pub state: &'static str,
    pub version: String,
    pub notes: String,
    pub done: u64,
    pub total: u64,
    pub message: Option<String>,
}

fn set_status(app: &AppHandle, status: Option<UpdateStatus>) {
    *app.state::<Updates>().status.lock() = status.clone();
    let _ = app.emit("bd://update", status);
    // 대기 중이면 트레이 아이콘에 초록 점
    crate::status::refresh(app);
}

impl Updates {
    /// 내려받아 두어 바로 설치할 수 있는 새 버전이 있는가
    pub fn is_ready(&self) -> bool {
        self.ready.lock().is_some()
    }
}

/// 백그라운드 확인 루프
pub fn spawn_background(app: AppHandle) {
    let _ = std::thread::Builder::new().name("bd-update".into()).spawn(move || {
        std::thread::sleep(Duration::from_secs(20));
        loop {
            if app.state::<AppState>().settings.lock().auto_update {
                if let Err(e) = tauri::async_runtime::block_on(check(&app, false)) {
                    log::info!("업데이트 확인 실패: {e}");
                }
            }
            std::thread::sleep(Duration::from_secs(6 * 3600));
        }
    });
}

/// 새 버전을 확인하고 내려받아 둔다. 반환: 새 버전 번호 (없으면 None)
///
/// `manual` 이면 자동 업데이트 설정과 상관없이 내려받는다.
pub async fn check(app: &AppHandle, manual: bool) -> Result<Option<String>, String> {
    let updates = app.state::<Updates>();
    if let Some((u, _)) = updates.ready.lock().as_ref() {
        return Ok(Some(u.version.clone()));
    }
    if updates.busy.swap(true, Ordering::SeqCst) {
        return Err("업데이트를 확인하고 있어요".into());
    }
    let r = check_inner(app, manual).await;
    updates.busy.store(false, Ordering::SeqCst);
    if let Err(e) = &r {
        if manual {
            set_status(
                app,
                Some(UpdateStatus {
                    state: "error",
                    version: String::new(),
                    notes: String::new(),
                    done: 0,
                    total: 0,
                    message: Some(e.clone()),
                }),
            );
        }
    }
    r
}

async fn check_inner(app: &AppHandle, manual: bool) -> Result<Option<String>, String> {
    *app.state::<Updates>().last_check.lock() = Some(Instant::now());
    let app2 = app.clone();
    let updater = app
        .updater_builder()
        // 설치 직전(프로세스 종료 전) 트레이 아이콘을 치워 유령 아이콘이 남지 않게
        .on_before_exit(move || {
            let _ = app2.state::<AppState>().store.lock().flush();
            if let Some(t) = app2.remove_tray_by_id("main") {
                let _ = t.set_visible(false);
            }
        })
        .build()
        .map_err(|e| e.to_string())?;
    let Some(update) = updater
        .check()
        .await
        .map_err(|e| format!("업데이트 서버에 연결하지 못했어요: {e}"))?
    else {
        return Ok(None);
    };
    let version = update.version.clone();
    let notes = update.body.clone().unwrap_or_default();
    let status = |state, done, total| UpdateStatus {
        state,
        version: version.clone(),
        notes: notes.clone(),
        done,
        total,
        message: None,
    };
    set_status(app, Some(status("downloading", 0, 0)));

    let mut done = 0u64;
    let mut last = Instant::now();
    let bytes = update
        .download(
            |chunk, total| {
                done += chunk as u64;
                if manual && last.elapsed() > Duration::from_millis(150) {
                    last = Instant::now();
                    set_status(app, Some(status("downloading", done, total.unwrap_or(0))));
                }
            },
            || {},
        )
        .await
        .map_err(|e| format!("업데이트를 내려받지 못했어요: {e}"))?;
    log::info!("업데이트 {version} 준비됨 ({} bytes)", bytes.len());
    *app.state::<Updates>().ready.lock() = Some((update, bytes));
    set_status(app, Some(status("ready", 0, 0)));
    maybe_install(app);
    Ok(Some(version))
}

/// 지금 설치해도 되는 상태인가 (녹음·요약·모델 다운로드 중이 아님)
fn idle(app: &AppHandle) -> bool {
    let st = app.state::<AppState>();
    let rec = *st.rec.lock();
    rec == RecState::Idle && st.busy_ops.load(Ordering::SeqCst) == 0 && st.downloading.lock().is_none()
}

/// 미니 창은 늘 떠 있으므로, 쓰고 있지 않으면(대기 중) 숨겨진 창처럼 업데이트해도 된다
fn mini_mode(app: &AppHandle) -> bool {
    app.state::<AppState>().settings.lock().mini_mode
}

fn window_hidden(app: &AppHandle) -> bool {
    app.get_webview_window("main")
        .map(|w| !w.is_visible().unwrap_or(true) || w.is_minimized().unwrap_or(false))
        .unwrap_or(true)
}

/// 준비된 업데이트가 있고, 쓰고 있지 않을 때(트레이로 숨겨진 상태) 조용히 설치
pub fn maybe_install(app: &AppHandle) {
    if !app.state::<AppState>().settings.lock().auto_update {
        return;
    }
    if app.state::<Updates>().ready.lock().is_none() || !idle(app) || !(window_hidden(app) || mini_mode(app)) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        // 숨기자마자 다시 여는 경우를 위해 잠깐 기다렸다가 한 번 더 확인
        std::thread::sleep(Duration::from_secs(8));
        let hidden = window_hidden(&app);
        if idle(&app) && (hidden || mini_mode(&app)) {
            if let Err(e) = install(&app, hidden) {
                log::warn!("자동 업데이트 설치 실패: {e}");
            }
        }
    });
}

/// 내려받아 둔 업데이트를 설치한다. 성공하면 앱이 종료되고 새 버전이 다시 켜진다.
pub fn install(app: &AppHandle, start_hidden: bool) -> Result<(), String> {
    if !idle(app) {
        return Err("녹음이나 요약이 끝난 뒤에 업데이트할 수 있어요".into());
    }
    let Some((update, bytes)) = app.state::<Updates>().ready.lock().take() else {
        return Err("내려받은 업데이트가 없어요".into());
    };
    let st = app.state::<AppState>();
    let marker = st.paths.config.join(START_HIDDEN_MARKER);
    if start_hidden {
        let _ = std::fs::create_dir_all(&st.paths.config);
        let _ = std::fs::write(&marker, b"1");
    }
    set_status(
        app,
        Some(UpdateStatus {
            state: "installing",
            version: update.version.clone(),
            notes: String::new(),
            done: 0,
            total: 0,
            message: None,
        }),
    );
    // Windows: 설치 프로그램을 띄우고 이 프로세스는 종료된다 (설치가 끝나면 새 버전이 저절로 켜짐)
    let r = update.install(&bytes).map_err(|e| format!("업데이트 설치 실패: {e}"));
    // 여기까지 왔다면 실패한 것
    let _ = std::fs::remove_file(&marker);
    *app.state::<Updates>().ready.lock() = Some((update, bytes));
    set_status(app, None);
    r
}
