//! 자동 업데이트 — GitHub Releases 의 latest.json 을 확인하고 minisign 서명을 검증한 뒤 설치한다.
//!
//! - 앱 시작 20초 뒤, 그리고 6시간마다 확인
//! - "자동 업데이트" 가 켜져 있으면 새 버전을 백그라운드에서 미리 받아 두고,
//!   녹음·요약·모델 다운로드 중이 아니고 창이 숨겨져 있을 때(트레이 · 최소화 · 미니 창) 조용히 설치 → 앱이 저절로 다시 켜진다
//! - 설치 조건은 녹음을 멈추거나 창을 숨길 때 바로 보고, 그 밖의 경우(요약이 끝남 · 미니 창으로 바꿈)도
//!   놓치지 않게 준비된 업데이트가 있는 동안 1분마다 다시 본다 (이때 최소화만 한 창은 쓰는 중으로 본다)
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
    /// 자동 설치를 앞두고 잠깐 기다리는 중 — 여러 곳에서 겹쳐 불려도 한 번만 설치하게
    arming: AtomicBool,
    /// 자동 설치가 마지막으로 실패한 때 — 그 뒤 잠깐은 1분 재시도를 쉰다 (창을 숨기는 등 다른 계기로는 다시 시도)
    auto_failed_at: Mutex<Option<Instant>>,
}

/// 창을 열 때 이만큼 지났으면 한 번 더 확인한다 (6시간 주기를 기다리지 않게)
const STALE: Duration = Duration::from_secs(30 * 60);

/// 준비된 업데이트를 지금 설치해도 되는지 다시 보는 간격
const RETRY: Duration = Duration::from_secs(60);

/// 자동 설치가 실패하면 1분 재시도를 이만큼 쉰다 (백신 검사 · 파일 잠금 같은 일시적 실패를 다시 시도하되, 1분마다 되풀이하지 않게)
const RETRY_AFTER_FAIL: Duration = Duration::from_secs(30 * 60);

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
    spawn_retry(app.clone());
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

/// 준비된 업데이트가 있으면 1분마다 '지금 설치해도 되나' 다시 본다.
/// 녹음 중지 · 창 숨기기는 그때 바로 보지만, 요약 · 모델 다운로드가 끝났을 때나 미니 창으로 바꿨을 때는
/// 따로 알려 주는 곳이 없어 준비된 업데이트가 계속 남아 있었다.
///
/// 최소화만 해 둔 창은 여기서 설치하지 않는다 — 다시 듣기 재생 · 문장 고치기 도중에 잠깐 내려 둔 것일 수 있다
/// (최소화한 채 녹음을 멈추는 등 다른 계기로는 전처럼 설치).
fn spawn_retry(app: AppHandle) {
    let _ = std::thread::Builder::new().name("bd-update-apply".into()).spawn(move || loop {
        std::thread::sleep(RETRY);
        let updates = app.state::<Updates>();
        let resting = updates.auto_failed_at.lock().is_some_and(|t| t.elapsed() < RETRY_AFTER_FAIL);
        if updates.is_ready() && !resting && (window_closed(&app) || mini_mode(&app)) {
            maybe_install(&app);
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

/// 창을 닫아 트레이로 숨겼는가 (최소화는 아님)
fn window_closed(app: &AppHandle) -> bool {
    app.get_webview_window("main").map(|w| !w.is_visible().unwrap_or(true)).unwrap_or(true)
}

/// 준비된 업데이트가 있고, 쓰고 있지 않을 때(트레이로 숨겨진 상태) 조용히 설치
pub fn maybe_install(app: &AppHandle) {
    if !app.state::<AppState>().settings.lock().auto_update {
        return;
    }
    if app.state::<Updates>().ready.lock().is_none() || !idle(app) || !(window_hidden(app) || mini_mode(app)) {
        return;
    }
    // 이미 기다리는 중이면 그쪽에 맡긴다 (창 숨기기와 1분 확인이 겹쳐도 설치는 한 번)
    if app.state::<Updates>().arming.swap(true, Ordering::SeqCst) {
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
                *app.state::<Updates>().auto_failed_at.lock() = Some(Instant::now());
            }
        }
        app.state::<Updates>().arming.store(false, Ordering::SeqCst);
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
    // 여기까지 왔다면 실패한 것 — 받아 둔 업데이트는 그대로이니 배너도 '준비됨'으로 되돌려 '지금 적용'으로 다시 시도할 수 있게
    let _ = std::fs::remove_file(&marker);
    let ready = UpdateStatus {
        state: "ready",
        version: update.version.clone(),
        notes: update.body.clone().unwrap_or_default(),
        done: 0,
        total: 0,
        message: None,
    };
    *app.state::<Updates>().ready.lock() = Some((update, bytes));
    set_status(app, Some(ready));
    r
}
