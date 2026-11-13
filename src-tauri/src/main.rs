// 릴리스 빌드에서는 콘솔 창을 띄우지 않는다
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod session;
mod settings;
mod snap;
mod status;
mod summary;
mod update;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use badasugi_core::asr::EngineConfig;
use badasugi_core::audio::{CaptureConfig, DeviceInfo};
use badasugi_core::models::{self, ModelSpec};
use badasugi_core::pipeline::{Event, EventSink, Pipeline, PipelineConfig};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, WindowEvent, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_opener::OpenerExt;

use session::{SearchHit, SessionDoc, SessionMeta, SessionStore, Utterance};
use settings::Settings;
use update::{UpdateStatus, Updates};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RecState {
    Idle,
    Starting,
    Recording,
    Stopping,
}

pub(crate) struct Paths {
    pub config: PathBuf,
    settings: PathBuf,
    models: PathBuf,
    sessions: PathBuf,
}

pub(crate) struct AppState {
    pub settings: Mutex<Settings>,
    pub paths: Paths,
    pub rec: Mutex<RecState>,
    pub pipeline: Mutex<Option<Pipeline>>,
    pub store: Mutex<SessionStore>,
    dl_cancel: Arc<AtomicBool>,
    pub downloading: Mutex<Option<String>>,
    /// 진행 중인 요약 등 (업데이트 설치를 미룬다)
    pub busy_ops: AtomicUsize,
    /// 지금 세션이 파일 받아쓰기인지
    pub file_mode: AtomicBool,
    /// 파일 받아쓰기 진행률 + 1 (0 = 모름)
    pub file_pct: AtomicUsize,
    /// 업데이트 직후 첫 실행이면 이전 버전
    updated_from: Mutex<Option<String>>,
    /// 시작할 때 단축키 등록에 실패했다면 그 이유 (다른 앱이 쓰는 중 등)
    hotkey_error: Mutex<Option<String>>,
    tray_toggle: Mutex<Option<MenuItem<Wry>>>,
    tray_pause: Mutex<Option<MenuItem<Wry>>>,
    tray_mini: Mutex<Option<MenuItem<Wry>>>,
    pub rec_started: Mutex<Option<Instant>>,
    quit_after_stop: AtomicBool,
}

type CmdResult<T> = Result<T, String>;

fn err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ───────────────────────── 상태 조회 ─────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelDto {
    #[serde(flatten)]
    spec: ModelSpec,
    ready: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeysDto {
    openai: bool,
    groq: bool,
    custom: bool,
    anthropic: bool,
    gemini: bool,
    llm_custom: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateDto {
    settings: Settings,
    rec: RecState,
    models: Vec<ModelDto>,
    custom_models: Vec<String>,
    speaker_model_ready: bool,
    keys: KeysDto,
    autostart: bool,
    version: &'static str,
    sessions_dir: String,
    models_dir: String,
    downloading: Option<String>,
    current: Option<SessionDoc>,
    elapsed_ms: u64,
    paused: bool,
    file_mode: bool,
    update: Option<UpdateStatus>,
    updated_from: Option<String>,
    hotkey_error: Option<String>,
    audio_exts: &'static [&'static str],
    demo: bool,
}

#[tauri::command]
fn get_state(app: AppHandle) -> StateDto {
    let st = app.state::<AppState>();
    let models_dir = &st.paths.models;
    let dto = StateDto {
        settings: st.settings.lock().clone(),
        rec: *st.rec.lock(),
        models: models::WHISPER_MODELS
            .iter()
            .map(|m| ModelDto {
                spec: m.clone(),
                ready: m.is_ready(models_dir),
            })
            .collect(),
        custom_models: models::custom_whisper_files(models_dir)
            .into_iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .collect(),
        speaker_model_ready: models::SPEAKER_MODEL.is_ready(models_dir),
        keys: KeysDto {
            openai: settings::get_key("openai").is_some(),
            groq: settings::get_key("groq").is_some(),
            custom: settings::get_key("custom").is_some(),
            anthropic: settings::get_key("anthropic").is_some(),
            gemini: settings::get_key("gemini").is_some(),
            llm_custom: settings::get_key("llm-custom").is_some(),
        },
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        version: env!("CARGO_PKG_VERSION"),
        sessions_dir: st.paths.sessions.display().to_string(),
        models_dir: models_dir.display().to_string(),
        downloading: st.downloading.lock().clone(),
        current: st.store.lock().current.clone(),
        elapsed_ms: st
            .rec_started
            .lock()
            .map(|t| t.elapsed().as_millis() as u64)
            .unwrap_or(0),
        paused: st.pipeline.lock().as_ref().is_some_and(|p| p.is_paused()),
        file_mode: st.file_mode.load(Ordering::SeqCst),
        update: app.state::<Updates>().status.lock().clone(),
        updated_from: st.updated_from.lock().take(),
        hotkey_error: st.hotkey_error.lock().take(),
        audio_exts: badasugi_core::decode::AUDIO_EXTENSIONS,
        demo: std::env::var("BADASUGI_DEMO").is_ok(),
    };
    dto
}

// ───────────────────────── 설정 ─────────────────────────

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> CmdResult<()> {
    let st = app.state::<AppState>();
    let mut settings = settings;
    let old_hotkey = {
        // 창 모드·위치·버전은 앱(백엔드)이 관리한다 — 화면이 들고 있던 옛 값으로 덮어쓰지 않게
        let cur = st.settings.lock();
        settings.mini_mode = cur.mini_mode;
        settings.mini_pos = cur.mini_pos;
        settings.full_bounds = cur.full_bounds;
        settings.last_version = cur.last_version.clone();
        cur.hotkey.clone()
    };
    settings.save(&st.paths.settings).map_err(err)?;
    if let Some(w) = app.get_webview_window("main") {
        // 미니 창은 늘 위에
        let _ = w.set_always_on_top(settings.always_on_top || settings.mini_mode);
    }
    let new_hotkey = settings.hotkey.clone();
    *st.settings.lock() = settings;
    if old_hotkey != new_hotkey {
        register_hotkey(&app, &new_hotkey)?;
    }
    Ok(())
}

#[tauri::command]
fn set_api_key(name: String, value: String) -> CmdResult<()> {
    if !settings::KEY_NAMES.contains(&name.as_str()) {
        return Err("알 수 없는 키 이름".into());
    }
    settings::set_key(&name, &value).map_err(err)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> CmdResult<()> {
    let al = app.autolaunch();
    if enabled {
        al.enable().map_err(err)
    } else {
        al.disable().map_err(err)
    }
}

#[tauri::command]
fn list_devices() -> Vec<DeviceInfo> {
    badasugi_core::audio::list_input_devices()
}

fn register_hotkey(app: &AppHandle, hotkey: &str) -> CmdResult<()> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    if hotkey.trim().is_empty() {
        return Ok(());
    }
    gs.register(hotkey.trim())
        .map_err(|e| format!("단축키 '{hotkey}' 를 등록할 수 없습니다: {e}"))
}

// ───────────────────────── 엔진 ─────────────────────────

fn build_engine(s: &Settings, paths: &Paths) -> Result<EngineConfig, String> {
    let need_key = |name: &str, label: &str| {
        settings::get_key(name).ok_or_else(|| format!("{label} API 키를 설정에서 입력해 주세요."))
    };
    Ok(match s.engine.as_str() {
        "openai" => EngineConfig::Api {
            base_url: "https://api.openai.com/v1".into(),
            api_key: need_key("openai", "OpenAI")?,
            model: s.openai_model.clone(),
        },
        "groq" => EngineConfig::Api {
            base_url: "https://api.groq.com/openai/v1".into(),
            api_key: need_key("groq", "Groq")?,
            model: s.groq_model.clone(),
        },
        "custom" => EngineConfig::Api {
            base_url: s.custom_url.clone(),
            api_key: settings::get_key("custom").unwrap_or_default(),
            model: s.custom_model.clone(),
        },
        _ => EngineConfig::Local {
            model_path: s.local_model_path(&paths.models),
            threads: s.threads,
            beam_size: s.beam_size,
        },
    })
}

/// 용어집을 whisper 프롬프트로 (자연스러운 문장 형태가 효과가 좋다)
fn glossary_prompt(glossary: &str) -> String {
    let terms: Vec<&str> = glossary
        .split([',', '\n', ';'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        String::new()
    } else {
        format!("회의 녹취록입니다. 주요 용어: {}.", terms.join(", "))
    }
}

#[tauri::command]
async fn test_engine(app: AppHandle) -> CmdResult<String> {
    let (s, engine) = {
        let st = app.state::<AppState>();
        let s = st.settings.lock().clone();
        let e = build_engine(&s, &st.paths)?;
        (s, e)
    };
    tauri::async_runtime::spawn_blocking(move || match engine {
        EngineConfig::Local { model_path, .. } => {
            if model_path.exists() {
                Ok(format!("로컬 모델 준비됨: {}", model_path.display()))
            } else {
                Err("모델 파일이 없습니다. 먼저 내려받아 주세요.".to_string())
            }
        }
        EngineConfig::Api {
            base_url,
            api_key,
            model,
        } => badasugi_core::asr::openai::test_connection(&base_url, &api_key, &model)
            .map(|_| format!("연결 성공 · {} ({})", model, s.engine))
            .map_err(err),
    })
    .await
    .map_err(err)?
}

// ───────────────────────── 모델 내려받기 ─────────────────────────

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadEvent {
    id: String,
    done: u64,
    total: u64,
    state: &'static str,
    message: Option<String>,
}

#[tauri::command]
fn download_model(app: AppHandle, id: String) -> CmdResult<()> {
    let spec: ModelSpec = if id == models::SPEAKER_MODEL.id {
        models::SPEAKER_MODEL.clone()
    } else {
        models::find_whisper(&id).cloned().ok_or("알 수 없는 모델")?
    };
    let st = app.state::<AppState>();
    {
        let mut d = st.downloading.lock();
        if d.is_some() {
            return Err("다른 모델을 내려받는 중입니다".into());
        }
        *d = Some(id.clone());
    }
    st.dl_cancel.store(false, Ordering::SeqCst);
    let dir = st.paths.models.clone();
    let cancel = st.dl_cancel.clone();
    let app2 = app.clone();
    std::thread::spawn(move || {
        let emit = |done, total, state, message: Option<String>| {
            let _ = app2.emit(
                "bd://download",
                DownloadEvent {
                    id: id.clone(),
                    done,
                    total,
                    state,
                    message,
                },
            );
        };
        let r = models::download(&spec, &dir, &cancel, |d, t| emit(d, t, "progress", None));
        *app2.state::<AppState>().downloading.lock() = None;
        match r {
            Ok(_) => emit(spec.size, spec.size, "done", None),
            Err(e) => emit(0, spec.size, "error", Some(e.to_string())),
        }
    });
    Ok(())
}

#[tauri::command]
fn cancel_download(app: AppHandle) {
    app.state::<AppState>().dl_cancel.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn delete_model(app: AppHandle, id: String) -> CmdResult<()> {
    let st = app.state::<AppState>();
    if *st.rec.lock() != RecState::Idle {
        return Err("녹음 중에는 모델을 지울 수 없습니다".into());
    }
    let path = match models::find_whisper(&id) {
        Some(spec) => spec.path_in(&st.paths.models),
        None if id.starts_with("ggml-") && id.ends_with(".bin") && !id.contains(['/', '\\']) => st.paths.models.join(&id),
        None => return Err("알 수 없는 모델".into()),
    };
    badasugi_core::asr::whisper::unload();
    std::fs::remove_file(path).map_err(err)
}

// ───────────────────────── 녹음 ─────────────────────────

fn set_rec(app: &AppHandle, state: RecState) {
    let st = app.state::<AppState>();
    *st.rec.lock() = state;
    let recording = matches!(state, RecState::Recording | RecState::Starting);
    status::refresh(app);
    if let Some(item) = st.tray_toggle.lock().as_ref() {
        let _ = item.set_text(if recording { "녹음 중지" } else { "녹음 시작" });
    }
    if let Some(item) = st.tray_pause.lock().as_ref() {
        let _ = item.set_enabled(state == RecState::Recording);
        let _ = item.set_text("일시정지");
    }
    let _ = app.emit("bd://rec", state);
}

fn set_paused_inner(app: &AppHandle, paused: bool) -> CmdResult<()> {
    let st = app.state::<AppState>();
    let guard = st.pipeline.lock();
    let p = guard.as_ref().ok_or("녹음 중이 아니에요")?;
    p.set_paused(paused);
    drop(guard);
    status::refresh(app);
    if let Some(item) = st.tray_pause.lock().as_ref() {
        let _ = item.set_text(if paused { "이어서 녹음" } else { "일시정지" });
    }
    let _ = app.emit("bd://paused", paused);
    Ok(())
}

fn toggle_pause(app: &AppHandle) -> CmdResult<()> {
    let paused = app
        .state::<AppState>()
        .pipeline
        .lock()
        .as_ref()
        .map(|p| p.is_paused())
        .ok_or("녹음 중이 아니에요")?;
    set_paused_inner(app, !paused)
}

#[tauri::command]
fn set_paused(app: AppHandle, paused: bool) -> CmdResult<()> {
    set_paused_inner(&app, paused)
}

fn start_recording_inner(app: &AppHandle) -> CmdResult<()> {
    start_session(app, None)
}

/// 녹음(마이크·시스템 소리) 또는 파일 받아쓰기를 시작한다
fn start_session(app: &AppHandle, file: Option<&Path>) -> CmdResult<()> {
    let st = app.state::<AppState>();
    {
        let mut rec = st.rec.lock();
        if *rec != RecState::Idle {
            return Err("이미 녹음 중이거나 정리 중입니다".into());
        }
        *rec = RecState::Starting;
    }
    set_rec(app, RecState::Starting);

    let result = (|| -> CmdResult<Pipeline> {
        let s = st.settings.lock().clone();
        let engine = build_engine(&s, &st.paths)?;
        let mut doc = SessionDoc::new(engine.label());
        if let Some(f) = file {
            let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("오디오 파일").to_string();
            let stem = f.file_stem().and_then(|n| n.to_str()).unwrap_or("오디오 파일");
            doc.title = stem.chars().take(60).collect();
            doc.source_file = Some(name);
            doc.source_path = Some(f.display().to_string());
        }
        let record_path = if s.save_audio && file.is_none() {
            let p = st.store.lock().audio_path(&doc.id);
            doc.audio_file = p.file_name().and_then(|n| n.to_str()).map(str::to_string);
            Some(p)
        } else {
            None
        };
        let speaker_model = models::SPEAKER_MODEL.path_in(&st.paths.models);
        let cfg = PipelineConfig {
            capture: CaptureConfig {
                source: s.source,
                mic_device: s.mic_device.clone(),
            },
            engine,
            diarization: s.diarization.clone(),
            speaker_model: speaker_model.exists().then_some(speaker_model),
            segmenter: Default::default(),
            language: s.language.clone(),
            prompt: glossary_prompt(&s.glossary),
            record_path,
        };
        st.store.lock().begin(doc);
        badasugi_core::perf::disable_power_throttling();
        let app2 = app.clone();
        let sink: EventSink = Arc::new(move |ev| on_pipeline_event(&app2, ev));
        st.file_mode.store(file.is_some(), Ordering::SeqCst);
        st.file_pct.store(0, Ordering::SeqCst);
        match file {
            Some(f) => Pipeline::start_file(f, cfg, sink).map_err(err),
            None => Pipeline::start(cfg, sink).map_err(err),
        }
    })();

    match result {
        Ok(p) => {
            *st.pipeline.lock() = Some(p);
            *st.rec_started.lock() = Some(Instant::now());
            set_rec(app, RecState::Recording);
            Ok(())
        }
        Err(e) => {
            st.store.lock().current = None;
            st.file_mode.store(false, Ordering::SeqCst);
            badasugi_core::perf::restore_power_throttling();
            set_rec(app, RecState::Idle);
            Err(e)
        }
    }
}

fn stop_recording_inner(app: &AppHandle) {
    let st = app.state::<AppState>();
    let p = st.pipeline.lock().take();
    if let Some(p) = p {
        set_rec(app, RecState::Stopping);
        p.stop();
    }
}

fn toggle_recording_inner(app: &AppHandle) -> CmdResult<()> {
    let state = *app.state::<AppState>().rec.lock();
    match state {
        RecState::Idle => start_recording_inner(app),
        RecState::Recording => {
            stop_recording_inner(app);
            Ok(())
        }
        _ => Ok(()),
    }
}

#[tauri::command]
async fn start_recording(app: AppHandle) -> CmdResult<()> {
    start_recording_inner(&app)
}

#[tauri::command]
async fn stop_recording(app: AppHandle) {
    stop_recording_inner(&app)
}

#[tauri::command]
async fn toggle_recording(app: AppHandle) -> CmdResult<()> {
    toggle_recording_inner(&app)
}

/// 녹음 파일(mp3·m4a·wav…)을 받아 적는다. 진행 상황은 녹음과 같은 이벤트 + progress 로 온다.
#[tauri::command]
async fn transcribe_file(app: AppHandle, path: String) -> CmdResult<()> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err("파일을 찾을 수 없어요".into());
    }
    start_session(&app, Some(&p))
}

fn on_pipeline_event(app: &AppHandle, ev: Event) {
    let st = app.state::<AppState>();
    match &ev {
        Event::Transcript {
            id,
            start_ms,
            end_ms,
            speaker,
            text,
            ..
        } => {
            let mut store = st.store.lock();
            if let Some(doc) = store.current.as_mut() {
                let speaker = doc.resolve(*speaker);
                doc.upsert(Utterance {
                    id: *id,
                    start_ms: *start_ms,
                    end_ms: *end_ms,
                    speaker,
                    text: text.clone(),
                });
            }
            store.mark_dirty();
            store.autosave();
        }
        Event::Progress { done_ms, total_ms } if *total_ms > 0 => {
            let pct = (done_ms * 100 / total_ms).min(100) as usize + 1;
            if st.file_pct.swap(pct, Ordering::SeqCst) != pct {
                status::refresh(app);
            }
        }
        Event::Level { position_ms, .. } => {
            if let Some(doc) = st.store.lock().current.as_mut() {
                doc.duration_ms = doc.duration_ms.max(*position_ms);
            }
        }
        Event::Stopped => {
            // 파일 받아쓰기는 끝까지 가면 저절로 멈춘다
            drop(st.pipeline.lock().take());
            st.file_mode.store(false, Ordering::SeqCst);
            {
                let mut store = st.store.lock();
                if let Err(e) = store.flush() {
                    log::warn!("기록 저장 실패: {e}");
                }
                // 내용이 없는 세션은 녹음 파일도 남기지 않는다
                if let Some(doc) = store.current.take() {
                    if doc.utterances.is_empty() {
                        let _ = std::fs::remove_file(store.audio_path(&doc.id));
                    }
                }
            }
            *st.rec_started.lock() = None;
            if st.settings.lock().unload_when_idle {
                badasugi_core::asr::whisper::unload();
            }
            badasugi_core::perf::restore_power_throttling();
            set_rec(app, RecState::Idle);
            if st.quit_after_stop.load(Ordering::SeqCst) {
                app.exit(0);
                return;
            }
            update::maybe_install(app);
        }
        _ => {}
    }
    let _ = app.emit("bd://event", &ev);
}

// ───────────────────────── 기록 ─────────────────────────

#[tauri::command]
fn list_sessions(app: AppHandle) -> Vec<SessionMeta> {
    app.state::<AppState>().store.lock().list()
}

#[tauri::command]
async fn search_sessions(app: AppHandle, query: String) -> Vec<SearchHit> {
    let dir = app.state::<AppState>().store.lock().dir.clone();
    tauri::async_runtime::spawn_blocking(move || SessionStore::new(dir).search(&query))
        .await
        .unwrap_or_default()
}

/// 기록의 음성 파일 경로 (다시 듣기). 화면이 읽을 수 있게 그 파일 하나만 허용한다.
///
/// - 실시간 녹음: 기록 폴더의 `<id>.wav` (녹음 중에도 1초마다 헤더를 고쳐 써서 그때까지 들을 수 있다)
/// - 파일 받아쓰기: 원본 파일
#[tauri::command]
fn session_audio(app: AppHandle, id: String) -> CmdResult<Option<String>> {
    let st = app.state::<AppState>();
    let (doc, dir) = {
        let store = st.store.lock();
        (store.load(&id).map_err(err)?, store.dir.clone())
    };
    let wav = doc
        .audio_file
        .as_deref()
        // 기록 파일을 고쳐 다른 경로를 가리키게 해도 기록 폴더 밖은 열지 않는다
        .and_then(|f| std::path::Path::new(f).file_name())
        .map(|f| dir.join(f))
        .filter(|p| p.is_file());
    let original = doc.source_path.as_deref().map(PathBuf::from).filter(|p| {
        p.is_file()
            && p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| badasugi_core::decode::AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
    });
    let Some(path) = wav.or(original) else {
        return Ok(None);
    };
    app.asset_protocol_scope().allow_file(&path).map_err(err)?;
    Ok(Some(path.display().to_string()))
}

#[tauri::command]
fn load_session(app: AppHandle, id: String) -> CmdResult<SessionDoc> {
    app.state::<AppState>().store.lock().load(&id).map_err(err)
}

#[tauri::command]
fn save_session(app: AppHandle, doc: SessionDoc) -> CmdResult<()> {
    app.state::<AppState>().store.lock().save_doc(doc).map_err(err)
}

#[tauri::command]
fn delete_session(app: AppHandle, id: String) -> CmdResult<()> {
    let st = app.state::<AppState>();
    if st.store.lock().current.as_ref().is_some_and(|c| c.id == id) {
        return Err("녹음 중인 기록은 지울 수 없습니다".into());
    }
    let r = st.store.lock().delete(&id).map_err(err);
    r
}

#[tauri::command]
fn export_session(app: AppHandle, id: String, format: String, path: String) -> CmdResult<()> {
    let doc = app.state::<AppState>().store.lock().load(&id).map_err(err)?;
    let body = match format.as_str() {
        "md" => doc.to_markdown(),
        "txt" => doc.to_text(),
        "srt" => doc.to_srt(),
        "json" => serde_json::to_string_pretty(&doc).map_err(err)?,
        _ => return Err("알 수 없는 형식".into()),
    };
    // 메모장에서 한글이 깨지지 않도록 텍스트 계열은 BOM 을 붙인다
    let mut bytes = Vec::with_capacity(body.len() + 3);
    if format == "txt" || format == "srt" {
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    bytes.extend_from_slice(body.as_bytes());
    std::fs::write(path, bytes).map_err(err)
}

#[tauri::command]
fn session_text(app: AppHandle, id: String, format: String) -> CmdResult<String> {
    let doc = app.state::<AppState>().store.lock().load(&id).map_err(err)?;
    Ok(match format.as_str() {
        "md" => doc.to_markdown(),
        _ => doc.to_text(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SummaryDto {
    text: String,
    /// 자동 제목이었다면 AI 가 붙인 새 제목
    title: Option<String>,
}

struct BusyGuard<'a>(&'a AtomicUsize);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tauri::command]
async fn summarize(app: AppHandle, id: String) -> CmdResult<SummaryDto> {
    let st = app.state::<AppState>();
    st.busy_ops.fetch_add(1, Ordering::SeqCst);
    let _busy = BusyGuard(&st.busy_ops);
    let (cfg, doc) = {
        let cfg = st.settings.lock().summary.clone();
        let doc = st.store.lock().load(&id).map_err(err)?;
        (cfg, doc)
    };
    let doc2 = doc.clone();
    let sum = tauri::async_runtime::spawn_blocking(move || summary::summarize(&cfg, &doc))
        .await
        .map_err(err)?
        .map_err(err)?;
    // 요약하는 동안 바뀐 내용(이름 바꾸기 등)을 덮어쓰지 않도록 최신 기록에 요약만 얹는다
    let loaded = st.store.lock().load(&id);
    let mut latest = loaded.unwrap_or(doc2);
    latest.summary = Some(sum.text.clone());
    let title = sum.title.filter(|_| latest.auto_title);
    if let Some(t) = &title {
        latest.title = t.clone();
        latest.auto_title = false;
    }
    let saved = st.store.lock().save_doc(latest);
    saved.map_err(err)?;
    Ok(SummaryDto { text: sum.text, title })
}

/// 요약 제공자 연결 확인 + 모델 목록
#[tauri::command]
async fn list_llm_models(app: AppHandle) -> CmdResult<Vec<String>> {
    let cfg = app.state::<AppState>().settings.lock().summary.clone();
    tauri::async_runtime::spawn_blocking(move || summary::list_models(&cfg))
        .await
        .map_err(err)?
        .map_err(err)
}

// ───────────────────────── 업데이트 ─────────────────────────

/// 지금 확인 (자동 업데이트 설정과 상관없이 새 버전이 있으면 내려받아 둔다)
#[tauri::command]
async fn check_update(app: AppHandle) -> CmdResult<Option<String>> {
    update::check(&app, true).await
}

/// 내려받아 둔 업데이트를 설치하고 다시 시작
#[tauri::command]
fn install_update(app: AppHandle) -> CmdResult<()> {
    update::install(&app, false)
}

// ───────────────────────── 미니 창 ─────────────────────────

/// 미니 창 크기 (논리 픽셀)
const MINI_SIZE: (f64, f64) = (372.0, 84.0);
/// 큰 창 최소 크기 (tauri.conf.json 과 같게)
const FULL_MIN: (f64, f64) = (360.0, 460.0);

/// 이 점이 어느 모니터 안에 있나 (모니터를 뗀 뒤 엉뚱한 곳에 뜨지 않게)
fn on_screen(app: &AppHandle, x: i32, y: i32) -> bool {
    app.available_monitors()
        .map(|ms| {
            ms.iter().any(|m| {
                let (p, s) = (m.position(), m.size());
                x + 40 >= p.x && x + 40 < p.x + s.width as i32 && y + 20 >= p.y && y + 20 < p.y + s.height as i32
            })
        })
        .unwrap_or(false)
}

/// 미니 창(녹음 도크만 · 항상 위 · 작업 표시줄 시계 위) ↔ 큰 창
fn set_mini_inner(app: &AppHandle, mini: bool, startup: bool) -> CmdResult<()> {
    let w = app.get_webview_window("main").ok_or("창을 찾을 수 없어요")?;
    let st = app.state::<AppState>();
    let mut s = st.settings.lock().clone();
    if mini {
        // 최대화된 상태였다면 먼저 원래 크기로 돌려놓고 그 크기를 기억한다 (돌아올 때 꽉 찬 창이 되지 않게)
        if w.is_maximized().unwrap_or(false) {
            let _ = w.unmaximize();
        }
        if !s.mini_mode {
            if let (Ok(p), Ok(sz)) = (w.outer_position(), w.inner_size()) {
                s.full_bounds = Some([p.x, p.y, sz.width as i32, sz.height as i32]);
            }
        }
        let _ = w.set_min_size(None::<tauri::Size>);
        let _ = w.set_resizable(false);
        let _ = w.set_size(LogicalSize::new(MINI_SIZE.0, MINI_SIZE.1));
        let _ = w.set_always_on_top(true);
        match s.mini_pos.filter(|p| on_screen(app, p[0], p[1])) {
            Some([x, y]) => {
                let _ = w.set_position(PhysicalPosition::new(x, y));
            }
            None => snap::dock_bottom_right(&w),
        }
        if !startup {
            show_main(app);
        }
    } else {
        if s.mini_mode {
            if let Ok(p) = w.outer_position() {
                s.mini_pos = Some([p.x, p.y]);
            }
        }
        let _ = w.unmaximize();
        let _ = w.set_resizable(true);
        let _ = w.set_min_size(Some(LogicalSize::new(FULL_MIN.0, FULL_MIN.1)));
        match s.full_bounds.filter(|b| on_screen(app, b[0], b[1])) {
            Some([x, y, bw, bh]) => {
                let _ = w.set_size(PhysicalSize::new(bw.max(1) as u32, bh.max(1) as u32));
                let _ = w.set_position(PhysicalPosition::new(x, y));
            }
            None => {
                let _ = w.set_size(LogicalSize::new(420.0, 680.0));
                let _ = w.center();
            }
        }
        let _ = w.set_always_on_top(s.always_on_top);
        show_main(app);
    }
    s.mini_mode = mini;
    let _ = s.save(&st.paths.settings);
    *st.settings.lock() = s;
    if let Some(item) = st.tray_mini.lock().as_ref() {
        let _ = item.set_text(if mini { "창 위치 복구" } else { "미니 창으로" });
    }
    let _ = app.emit("bd://mini", mini);
    Ok(())
}

#[tauri::command]
fn set_mini(app: AppHandle, mini: bool) -> CmdResult<()> {
    set_mini_inner(&app, mini, false)
}

/// 창을 끌어 옮기고 놓았을 때: 미니 창 위치를 기억
fn on_move_end(app: &AppHandle) {
    let st = app.state::<AppState>();
    if !st.settings.lock().mini_mode {
        return;
    }
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    if let Ok(p) = w.outer_position() {
        let mut s = st.settings.lock();
        s.mini_pos = Some([p.x, p.y]);
        let _ = s.save(&st.paths.settings);
    }
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    update::maybe_install(&app);
}

#[tauri::command]
fn open_folder(app: AppHandle, kind: String) -> CmdResult<()> {
    let st = app.state::<AppState>();
    let dir = match kind.as_str() {
        "models" => st.paths.models.clone(),
        _ => st.paths.sessions.clone(),
    };
    std::fs::create_dir_all(&dir).map_err(err)?;
    app.opener().open_path(dir.display().to_string(), None::<&str>).map_err(err)
}

#[tauri::command]
fn open_url(app: AppHandle, url: String) -> CmdResult<()> {
    if !url.starts_with("https://") {
        return Err("https 주소만 열 수 있습니다".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(err)
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    request_quit(&app);
}

fn request_quit(app: &AppHandle) {
    let st = app.state::<AppState>();
    let rec = *st.rec.lock();
    if rec == RecState::Idle {
        app.exit(0);
    } else {
        // 남은 발화를 마저 인식하고 저장한 뒤 종료
        st.quit_after_stop.store(true, Ordering::SeqCst);
        stop_recording_inner(app);
    }
}

// ───────────────────────── 창 · 트레이 ─────────────────────────

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    update::check_if_stale(app);
}

fn toggle_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false) {
            let _ = w.hide();
            update::maybe_install(app);
        } else {
            show_main(app);
        }
    }
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "녹음 시작", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "일시정지", false, None::<&str>)?;
    let mini_text = if app.state::<AppState>().settings.lock().mini_mode { "창 위치 복구" } else { "미니 창으로" };
    let mini = MenuItem::with_id(app, "mini", mini_text, true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "창 열기", true, None::<&str>)?;
    let folder = MenuItem::with_id(app, "folder", "회의 기록 폴더", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&toggle, &pause, &sep2, &show, &mini, &folder, &sep, &quit])?;
    *app.state::<AppState>().tray_toggle.lock() = Some(toggle);
    *app.state::<AppState>().tray_pause.lock() = Some(pause);
    *app.state::<AppState>().tray_mini.lock() = Some(mini);

    TrayIconBuilder::with_id("main")
        .icon(app.state::<status::Status>().idle_icon())
        .tooltip("받아쓰기 — 대기 중")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle" => {
                let app = app.clone();
                std::thread::spawn(move || {
                    if let Err(e) = toggle_recording_inner(&app) {
                        show_main(&app);
                        let _ = app.emit("bd://toast", e);
                    }
                });
            }
            "pause" => {
                let _ = toggle_pause(app);
            }
            "show" => show_main(app),
            "mini" => {
                let now = app.state::<AppState>().settings.lock().mini_mode;
                if let Err(e) = set_mini_inner(app, !now, false) {
                    let _ = app.emit("bd://toast", e);
                }
            }
            "folder" => {
                let _ = open_folder(app.clone(), "sessions".into());
            }
            "quit" => request_quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// 두 번째 실행에서 넘어온 명령 처리 (작업 스레드에서 호출)
fn handle_forwarded(app: &AppHandle, argv: &[String]) {
    // `badasugi.exe 회의.m4a` · 탐색기 "연결 프로그램" → 파일 받아쓰기
    if let Some(file) = audio_arg(argv) {
        show_main(app);
        open_file_from_cli(app.clone(), file);
        return;
    }
    let cmd = argv.iter().skip(1).find(|a| a.starts_with("--")).cloned();
    let rec = *app.state::<AppState>().rec.lock();
    let r = match cmd.as_deref() {
        Some("--pause") => toggle_pause(app),
        Some("--mini") => {
            // 창 조작은 메인 스레드에서 (크기를 바꾼 직후 위치를 계산하므로)
            let h = app.clone();
            let _ = app.run_on_main_thread(move || {
                let now = h.state::<AppState>().settings.lock().mini_mode;
                if let Err(e) = set_mini_inner(&h, !now, false) {
                    let _ = h.emit("bd://toast", e);
                }
            });
            Ok(())
        }
        Some("--start") if rec == RecState::Idle => start_recording_inner(app),
        Some("--stop") if rec == RecState::Recording => {
            stop_recording_inner(app);
            Ok(())
        }
        Some("--toggle") => toggle_recording_inner(app),
        Some("--start") | Some("--stop") | Some("--minimized") => Ok(()),
        _ => {
            show_main(app);
            Ok(())
        }
    };
    if let Err(e) = r {
        show_main(app);
        let _ = app.emit("bd://toast", e);
    }
}

/// 명령줄 인자 중 받아 적을 수 있는 오디오 파일
fn audio_arg(argv: &[String]) -> Option<PathBuf> {
    argv.iter().skip(1).filter(|a| !a.starts_with("--")).map(PathBuf::from).find(|p| {
        p.is_file()
            && p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| badasugi_core::decode::AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
    })
}

fn open_file_from_cli(app: AppHandle, file: PathBuf) {
    std::thread::spawn(move || {
        // 창(UI)이 이벤트를 받을 준비가 될 때까지 잠깐
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if let Err(e) = start_session(&app, Some(&file)) {
            show_main(&app);
            let _ = app.emit("bd://toast", e);
        }
    });
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    tauri::Builder::default()
        // 두 번째 실행은 이미 떠 있는 앱으로 전달된다: `badasugi --toggle | --start | --stop | --pause`
        // (Stream Deck · 배치 파일 · 바로 가기 등에서 녹음 제어)
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let app = app.clone();
            // 앱이 막 켜지는 중(setup 전)에 들어온 명령도 놓치지 않도록, 준비될 때까지 기다렸다가 처리
            std::thread::spawn(move || {
                for _ in 0..150 {
                    if app.try_state::<AppState>().is_some() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if app.try_state::<AppState>().is_none() {
                    return;
                }
                handle_forwarded(&app, &argv);
            });
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        let app = app.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = toggle_recording_inner(&app) {
                                show_main(&app);
                                let _ = app.emit("bd://toast", e);
                            }
                        });
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        // 창 크기·위치를 기억 (보이기 여부는 트레이 로직이 정한다)
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE | tauri_plugin_window_state::StateFlags::POSITION,
                )
                .build(),
        )
        .setup(|app| {
            let handle = app.handle().clone();
            let path = app.path();
            let config_dir = path.app_config_dir()?;
            let models_dir = path.app_local_data_dir()?.join("models");
            let sessions_dir = path
                .document_dir()
                .map(|d| d.join("받아쓰기"))
                .unwrap_or_else(|_| path.app_data_dir().unwrap_or_default().join("sessions"));
            let settings_path = config_dir.join("settings.json");
            let mut settings = Settings::load(&settings_path);
            // 업데이트 직후 첫 실행인지 (안내 토스트용)
            let version = app.package_info().version.to_string();
            let updated_from = (!settings.last_version.is_empty() && settings.last_version != version)
                .then(|| settings.last_version.clone());
            if settings.last_version != version {
                settings.last_version = version;
                let _ = settings.save(&settings_path);
            }
            // 자동 업데이트로 다시 켜졌고 그 전에 창이 숨겨져 있었다면 계속 숨긴 채로
            let marker = config_dir.join(update::START_HIDDEN_MARKER);
            let start_hidden = marker.exists();
            let _ = std::fs::remove_file(&marker);

            let tray_base = Image::from_bytes(include_bytes!("../icons/tray.png"))?.to_owned();

            app.manage(AppState {
                settings: Mutex::new(settings.clone()),
                paths: Paths {
                    config: config_dir,
                    settings: settings_path,
                    models: models_dir,
                    sessions: sessions_dir.clone(),
                },
                rec: Mutex::new(RecState::Idle),
                pipeline: Mutex::new(None),
                store: Mutex::new(SessionStore::new(sessions_dir)),
                dl_cancel: Arc::new(AtomicBool::new(false)),
                downloading: Mutex::new(None),
                busy_ops: AtomicUsize::new(0),
                file_mode: AtomicBool::new(false),
                file_pct: AtomicUsize::new(0),
                updated_from: Mutex::new(updated_from),
                hotkey_error: Mutex::new(None),
                tray_toggle: Mutex::new(None),
                tray_pause: Mutex::new(None),
                tray_mini: Mutex::new(None),
                rec_started: Mutex::new(None),
                quit_after_stop: AtomicBool::new(false),
            });
            app.manage(Updates::default());
            app.manage(status::Status::new(tray_base));

            setup_tray(&handle)?;
            if let Err(e) = register_hotkey(&handle, &settings.hotkey) {
                log::warn!("{e}");
                *app.state::<AppState>().hotkey_error.lock() =
                    Some(format!("{e}\n다른 프로그램이 쓰고 있을 수 있어요. 설정에서 다른 조합으로 바꿔 주세요."));
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_always_on_top(settings.always_on_top);
                // 끌어 옮길 때 화면 가장자리·작업 표시줄에 자석처럼 붙게
                let h = handle.clone();
                snap::install(&w, Box::new(move || on_move_end(&h)));
                if settings.mini_mode {
                    let _ = set_mini_inner(&handle, true, true);
                }
                let minimized = start_hidden || std::env::args().any(|a| a == "--minimized");
                if !minimized {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            update::spawn_background(handle.clone());
            status::spawn_ticker(handle.clone());
            if let Some(file) = audio_arg(&std::env::args().collect::<Vec<_>>()) {
                open_file_from_cli(handle.clone(), file);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 닫기 = 트레이로 숨기기 (종료는 트레이 메뉴에서)
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                update::maybe_install(window.app_handle());
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            save_settings,
            set_api_key,
            set_autostart,
            list_devices,
            test_engine,
            download_model,
            cancel_download,
            delete_model,
            start_recording,
            stop_recording,
            toggle_recording,
            set_paused,
            transcribe_file,
            list_sessions,
            search_sessions,
            session_audio,
            load_session,
            save_session,
            delete_session,
            export_session,
            session_text,
            summarize,
            list_llm_models,
            check_update,
            install_update,
            hide_window,
            set_mini,
            open_folder,
            open_url,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("받아쓰기 실행 실패");
}
