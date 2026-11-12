// 릴리스 빌드에서는 콘솔 창을 띄우지 않는다
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod session;
mod settings;
mod summary;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
use tauri::{AppHandle, Emitter, Manager, WindowEvent, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tauri_plugin_opener::OpenerExt;

use session::{SessionDoc, SessionMeta, SessionStore, Utterance};
use settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum RecState {
    Idle,
    Starting,
    Recording,
    Stopping,
}

struct Paths {
    settings: PathBuf,
    models: PathBuf,
    sessions: PathBuf,
}

struct AppState {
    settings: Mutex<Settings>,
    paths: Paths,
    rec: Mutex<RecState>,
    pipeline: Mutex<Option<Pipeline>>,
    store: Mutex<SessionStore>,
    dl_cancel: Arc<AtomicBool>,
    downloading: Mutex<Option<String>>,
    tray_toggle: Mutex<Option<MenuItem<Wry>>>,
    icon_idle: Image<'static>,
    icon_rec: Image<'static>,
    rec_started: Mutex<Option<Instant>>,
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
        demo: std::env::var("BADASUGI_DEMO").is_ok(),
    };
    dto
}

// ───────────────────────── 설정 ─────────────────────────

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> CmdResult<()> {
    let st = app.state::<AppState>();
    let old_hotkey = st.settings.lock().hotkey.clone();
    settings.save(&st.paths.settings).map_err(err)?;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_always_on_top(settings.always_on_top);
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
    if !["openai", "groq", "custom", "anthropic", "llm-custom"].contains(&name.as_str()) {
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
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_icon(Some(if recording {
            st.icon_rec.clone()
        } else {
            st.icon_idle.clone()
        }));
        let _ = tray.set_tooltip(Some(match state {
            RecState::Idle => "받아쓰기 — 대기 중",
            RecState::Starting => "받아쓰기 — 시작하는 중…",
            RecState::Recording => "받아쓰기 — 녹음 중",
            RecState::Stopping => "받아쓰기 — 정리하는 중…",
        }));
    }
    if let Some(item) = st.tray_toggle.lock().as_ref() {
        let _ = item.set_text(if recording { "녹음 중지" } else { "녹음 시작" });
    }
    let _ = app.emit("bd://rec", state);
}

fn start_recording_inner(app: &AppHandle) -> CmdResult<()> {
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
        let record_path = if s.save_audio {
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
        Pipeline::start(cfg, sink).map_err(err)
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
                doc.upsert(Utterance {
                    id: *id,
                    start_ms: *start_ms,
                    end_ms: *end_ms,
                    speaker: *speaker,
                    text: text.clone(),
                });
            }
            store.mark_dirty();
            store.autosave();
        }
        Event::Level { position_ms, .. } => {
            if let Some(doc) = st.store.lock().current.as_mut() {
                doc.duration_ms = doc.duration_ms.max(*position_ms);
            }
        }
        Event::Stopped => {
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
            }
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

#[tauri::command]
async fn summarize(app: AppHandle, id: String) -> CmdResult<String> {
    let (cfg, doc) = {
        let st = app.state::<AppState>();
        let cfg = st.settings.lock().summary.clone();
        let doc = st.store.lock().load(&id).map_err(err)?;
        (cfg, doc)
    };
    let mut doc2 = doc.clone();
    let text = tauri::async_runtime::spawn_blocking(move || summary::summarize(&cfg, &doc))
        .await
        .map_err(err)?
        .map_err(err)?;
    doc2.summary = Some(text.clone());
    let _ = app.state::<AppState>().store.lock().save_doc(doc2);
    Ok(text)
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
}

fn toggle_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false) {
            let _ = w.hide();
        } else {
            show_main(app);
        }
    }
}

/// 트레이 아이콘 오른쪽 아래에 빨간 점을 찍어 "녹음 중" 아이콘을 만든다
fn recording_icon(base: &Image<'_>) -> Image<'static> {
    let (w, h) = (base.width() as i32, base.height() as i32);
    let mut px = base.rgba().to_vec();
    let r = (w as f32 * 0.24).max(4.0);
    let (cx, cy) = (w as f32 - r - 0.5, h as f32 - r - 0.5);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let i = ((y * w + x) * 4) as usize;
            if d <= r - 1.0 {
                px[i..i + 4].copy_from_slice(&[239, 68, 68, 255]);
            } else if d <= r + 0.6 {
                px[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    Image::new_owned(px, w as u32, h as u32)
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "녹음 시작", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "창 열기", true, None::<&str>)?;
    let folder = MenuItem::with_id(app, "folder", "회의 기록 폴더", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&toggle, &show, &folder, &sep, &quit])?;
    *app.state::<AppState>().tray_toggle.lock() = Some(toggle);

    TrayIconBuilder::with_id("main")
        .icon(app.state::<AppState>().icon_idle.clone())
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
            "show" => show_main(app),
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

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    tauri::Builder::default()
        // 두 번째 실행은 이미 떠 있는 앱으로 전달된다: `badasugi --toggle | --start | --stop`
        // (Stream Deck · 배치 파일 · 바로 가기 등에서 녹음 제어)
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let cmd = argv.iter().skip(1).find(|a| a.starts_with("--")).cloned();
            let app = app.clone();
            match cmd.as_deref() {
                Some("--toggle") | Some("--start") | Some("--stop") => {
                    std::thread::spawn(move || {
                        let rec = *app.state::<AppState>().rec.lock();
                        let r = match cmd.as_deref() {
                            Some("--start") if rec == RecState::Idle => start_recording_inner(&app),
                            Some("--stop") if rec == RecState::Recording => {
                                stop_recording_inner(&app);
                                Ok(())
                            }
                            Some("--toggle") => toggle_recording_inner(&app),
                            _ => Ok(()),
                        };
                        if let Err(e) = r {
                            show_main(&app);
                            let _ = app.emit("bd://toast", e);
                        }
                    });
                }
                _ => show_main(&app),
            }
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
            let settings = Settings::load(&settings_path);

            let icon_idle = Image::from_bytes(include_bytes!("../icons/tray.png"))?.to_owned();
            let icon_rec = recording_icon(&icon_idle);

            app.manage(AppState {
                settings: Mutex::new(settings.clone()),
                paths: Paths {
                    settings: settings_path,
                    models: models_dir,
                    sessions: sessions_dir.clone(),
                },
                rec: Mutex::new(RecState::Idle),
                pipeline: Mutex::new(None),
                store: Mutex::new(SessionStore::new(sessions_dir)),
                dl_cancel: Arc::new(AtomicBool::new(false)),
                downloading: Mutex::new(None),
                tray_toggle: Mutex::new(None),
                icon_idle,
                icon_rec,
                rec_started: Mutex::new(None),
                quit_after_stop: AtomicBool::new(false),
            });

            setup_tray(&handle)?;
            if let Err(e) = register_hotkey(&handle, &settings.hotkey) {
                log::warn!("{e}");
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_always_on_top(settings.always_on_top);
                let minimized = std::env::args().any(|a| a == "--minimized");
                if !minimized {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // 닫기 = 트레이로 숨기기 (종료는 트레이 메뉴에서)
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
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
            list_sessions,
            load_session,
            save_session,
            delete_session,
            export_session,
            session_text,
            summarize,
            open_folder,
            open_url,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("받아쓰기 실행 실패");
}
