// 릴리스 빌드에서는 콘솔 창을 띄우지 않는다
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ant;
mod ask;
mod ask_all;
mod calendar;
mod dictate;
mod inject;
mod export;
#[cfg(windows)]
mod forward;
mod logging;
mod meeting;
mod power;
mod prompts;
mod session;
mod share;
mod settings;
mod smoke;
mod snap;
mod status;
mod summary;
mod tails;
mod templates;
mod translate;
mod update;
mod pricing;
mod privacy;
mod redo;
mod usage_book;
mod voices;
mod watch;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use badasugi_core::asr::EngineConfig;
use badasugi_core::audio::{CaptureConfig, DeviceInfo, SourceKind};
use badasugi_core::models::{self, ModelSpec};
use badasugi_core::pipeline::{Event, EventSink, Pipeline, PipelineConfig};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, WindowEvent, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

use session::{SearchHit, SessionDoc, SessionMeta, SessionStore, Utterance};
use settings::Settings;
use update::{UpdateStatus, Updates};
use voices::VoiceBook;

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
    /// 요약 번호 → 멈추라는 신호. 화면이 앞선 저장을 기다리느라 요약이 아직 시작되지 않았을 때 멈춰도
    /// 그 번호에 신호를 먼저 꽂아 두어, 요약이 시작되자마자 요청 없이 멈춘다
    summary_cancel: Mutex<BTreeMap<u64, Arc<AtomicBool>>>,
    /// 번역 단추로 번역하고 있는 회의 id — 그동안 그 회의의 실시간 번역은 쉰다 (두 작업이 서로의 번역을 지우지 않게)
    tr_busy: Mutex<BTreeSet<String>>,
    /// 지금 세션이 파일 받아쓰기인지
    pub file_mode: AtomicBool,
    /// 파일 받아쓰기 진행률 + 1 (0 = 모름)
    pub file_pct: AtomicUsize,
    /// 이번 녹음에서 자동 저장 실패를 이미 알렸나 (한 번만 알림)
    save_warned: AtomicBool,
    /// 지금 녹음이 쓰는 음성 인식 API 공급자 · 모델 (로컬이면 None) — 사용량 집계용
    api_provider: Mutex<Option<(&'static str, String)>>,
    /// 화면 스크립트가 끝까지 실행돼 준비됐는지 (자동 점검이 확인)
    pub ui_ready: AtomicBool,
    /// 녹음 중 지금 위치 (ms) — 다시 듣기 구간을 녹음 시간축에 맞추는 데 쓴다
    live_pos_ms: AtomicU64,
    /// 녹음 중 다시 들은 구간들 (그 소리가 다시 받아 적히면 버린다)
    echo: Mutex<Vec<EchoWindow>>,
    /// 업데이트 직후 첫 실행이면 이전 버전
    updated_from: Mutex<Option<String>>,
    /// 시작할 때 단축키 등록에 실패했다면 그 이유 (다른 앱이 쓰는 중 등)
    hotkey_error: Mutex<Option<String>>,
    tray_toggle: Mutex<Option<MenuItem<Wry>>>,
    tray_pause: Mutex<Option<MenuItem<Wry>>>,
    tray_mini: Mutex<Option<MenuItem<Wry>>>,
    pub rec_started: Mutex<Option<Instant>>,
    /// 지금 녹음이 받는 소리 (회의 시작 알림에서 시작하면 설정과 다를 수 있다 · 파일 받아쓰기면 None)
    rec_source: Mutex<Option<SourceKind>>,
    quit_after_stop: AtomicBool,
    /// PC 가 잠들어서 녹음을 일시정지했다 (사람이 이어서 녹음하면 풀림)
    sleep_paused: AtomicBool,
    /// 이번 잠에 대해 화면에 이미 알렸다 (깨어남 알림이 여러 번 와도 한 번만)
    sleep_notified: AtomicBool,
    /// 이번 녹음에서 목소리로 자동으로 붙인 이름: 화자 → 이름 (회의가 끝나면 전체를 보고 다시 확인한다).
    /// 사람이 그 화자 이름을 고치면 빠진다.
    auto_named: Mutex<BTreeMap<u32, String>>,
    /// 지금 녹음에서 사람이 합친 화자 · 이름 붙인 화자를 파이프라인에 알려 주는 손잡이 (정리하는 동안에도)
    speaker_hints: Mutex<Option<badasugi_core::pipeline::SharedHints>>,
}

pub(crate) type CmdResult<T> = Result<T, String>;

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
    rec_source: Option<SourceKind>,
    update: Option<UpdateStatus>,
    updated_from: Option<String>,
    hotkey_error: Option<String>,
    audio_exts: &'static [&'static str],
    demo: bool,
    /// 지금 받아 적는 녹음 폴더 파일 (녹음 폴더 지켜보기)
    watch_job: Option<String>,
    /// GPU(Vulkan) 와 함께 빌드한 배포본인지 · 마지막으로 모델을 연 장치 ("GPU · …" / "CPU")
    gpu_built: bool,
    gpu_device: Option<String>,
    /// 녹음 뒤 다시 받아 적기 — 하고 있는 회의 · 기다리는 회의
    redo: redo::Status,
}

#[tauri::command]
fn get_state(app: AppHandle) -> StateDto {
    let st = app.state::<AppState>();
    let models_dir = &st.paths.models;
    // 다른 잠금을 쥐기 전에 (녹음 폴더 지켜보기는 자기 잠금을 쥔 채 설정을 본다)
    let watch_job = watch::job_name(&app);
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
        rec_source: *st.rec_source.lock(),
        update: app.state::<Updates>().status.lock().clone(),
        updated_from: st.updated_from.lock().take(),
        hotkey_error: st.hotkey_error.lock().take(),
        audio_exts: badasugi_core::decode::AUDIO_EXTENSIONS,
        demo: std::env::var("BADASUGI_DEMO").is_ok(),
        watch_job,
        gpu_built: badasugi_core::asr::gpu::BUILT_WITH_GPU,
        gpu_device: badasugi_core::asr::local_device(),
        redo: app.state::<redo::RedoQueue>().status(),
    };
    dto
}

// ───────────────────────── 설정 ─────────────────────────

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> CmdResult<()> {
    let st = app.state::<AppState>();
    let mut settings = settings;
    let (old_hotkey, old_star, old_dict) = {
        // 창 모드·위치·버전은 앱(백엔드)이 관리한다 — 화면이 들고 있던 옛 값으로 덮어쓰지 않게
        let cur = st.settings.lock();
        settings.mini_mode = cur.mini_mode;
        settings.mini_pos = cur.mini_pos;
        settings.mini_docked = cur.mini_docked;
        settings.full_bounds = cur.full_bounds;
        settings.last_version = cur.last_version.clone();
        (cur.hotkey.clone(), cur.star_hotkey.clone(), dictation_key(&cur).to_string())
    };
    // 새 단축키부터 등록해 본다 — 다른 프로그램이 쓰는 조합이면 원래 단축키를 되살리고 그 값은 저장하지 않는다
    // (그대로 두면 단축키가 하나도 없게 되고, 켤 때마다 같은 오류가 난다)
    let mut hotkey_err = None;
    let new_dict = dictation_key(&settings).to_string();
    if old_hotkey != settings.hotkey || old_star != settings.star_hotkey || old_dict != new_dict {
        let same = |a: &str, b: &str| settings::same_hotkey(a, b);
        let res = if same(&settings.hotkey, &settings.star_hotkey) {
            Err("녹음 단축키와 중요 표시 단축키를 다른 조합으로 골라 주세요.".to_string())
        } else if same(&new_dict, &settings.hotkey) || same(&new_dict, &settings.star_hotkey) {
            Err("말로 입력 단축키를 녹음 · 중요 표시 단축키와 다른 조합으로 골라 주세요.".to_string())
        } else {
            register_hotkeys(&app, &settings.hotkey, &settings.star_hotkey, &new_dict)
        };
        if let Err(e) = res {
            let _ = register_hotkeys(&app, &old_hotkey, &old_star, &old_dict);
            settings.hotkey = old_hotkey;
            settings.star_hotkey = old_star;
            // 말로 입력은 켜기 전 상태로 (단축키를 쓸 수 없으면 켜지 않는다)
            if old_dict != new_dict {
                let cur = st.settings.lock();
                settings.dictation = cur.dictation;
                settings.dictation_hotkey = cur.dictation_hotkey.clone();
            }
            hotkey_err = Some(e);
        }
    }
    settings.save(&st.paths.settings).map_err(err)?;
    if let Some(w) = app.get_webview_window("main") {
        // 미니 창은 늘 위에
        let _ = w.set_always_on_top(settings.always_on_top || settings.mini_mode);
    }
    // GPU 가속을 끄고 켠 것은 다음에 모델을 열 때부터 (열린 모델이 다른 장치면 다시 연다) — 녹음 · 파일 · 받아쓰기 입력 모두
    badasugi_core::asr::gpu::set_enabled(settings.gpu);
    let keep_changed = st.settings.lock().privacy.audio_keep_days != settings.privacy.audio_keep_days;
    *st.settings.lock() = settings;
    // 녹음 폴더 지켜보기를 켰거나 폴더를 바꿨으면 바로 기준점을 잡게
    watch::wake(&app);
    // 녹음 보관 기간을 바꾸면 바로 한 번 (기록을 모두 읽으므로 따로)
    if keep_changed {
        let app2 = app.clone();
        let _ = std::thread::Builder::new().name("bd-expire-now".into()).spawn(move || expire_audio_now(&app2));
    }
    hotkey_err.map_or(Ok(()), Err)
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

/// 말로 입력 단축키 — 켰을 때만 (끄면 빈 값)
fn dictation_key(s: &Settings) -> &str {
    if s.dictation {
        s.dictation_hotkey.trim()
    } else {
        ""
    }
}

/// 전역 단축키(녹음 · 중요 표시 ⭐ · 말로 입력)를 다시 등록한다. 하나가 실패해도 나머지는 등록해 두고 첫 오류를 돌려준다
fn register_hotkeys(app: &AppHandle, rec: &str, star: &str, dictate: &str) -> CmdResult<()> {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    let mut first_err = None;
    for (key, what) in [(rec.trim(), "녹음"), (star.trim(), "중요 표시"), (dictate.trim(), "말로 입력")] {
        if key.is_empty() {
            continue;
        }
        if let Err(e) = gs.register(key) {
            first_err.get_or_insert(format!(
                "{what} 단축키 '{key}' 는 다른 프로그램이 쓰고 있거나 쓸 수 없는 조합이에요. 다른 조합을 골라 주세요. ({e})"
            ));
        }
    }
    first_err.map_or(Ok(()), Err)
}

/// 눌린 전역 단축키가 말로 입력 단축키인지 (켰을 때만 · 녹음 · 중요 표시와 같은 조합이면 아니다)
fn is_dictation_shortcut(app: &AppHandle, pressed: &Shortcut) -> bool {
    app.try_state::<AppState>().is_some_and(|st| {
        let s = st.settings.lock();
        let id = |k: &str| k.trim().parse::<Shortcut>().ok().map(|s| s.id());
        let dict = id(dictation_key(&s));
        dict.is_some() && dict == Some(pressed.id()) && id(&s.hotkey) != dict && id(&s.star_hotkey) != dict
    })
}

/// 눌린 전역 단축키가 중요 표시 ⭐ 단축키인지 (아니면 녹음 단축키).
/// 두 단축키가 같은 조합이면(설정 파일을 손으로 고친 경우 등) 녹음 단축키로 본다
fn is_star_shortcut(app: &AppHandle, pressed: &Shortcut) -> bool {
    app.try_state::<AppState>().is_some_and(|st| {
        let (rec, star) = {
            let s = st.settings.lock();
            (s.hotkey.clone(), s.star_hotkey.clone())
        };
        let id = |k: &str| k.trim().parse::<Shortcut>().ok().map(|s| s.id());
        let star_id = id(&star);
        star_id.is_some() && star_id == Some(pressed.id()) && id(&rec) != star_id
    })
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

/// 지금 설정으로 받아 적을 수 있나 — 안 되면 그 까닭 (녹음 폴더 지켜보기가 설정 문제를 파일 실패로 세지 않게 먼저 본다)
pub(crate) fn engine_problem(app: &AppHandle) -> Option<String> {
    let st = app.state::<AppState>();
    let s = st.settings.lock().clone();
    match build_engine(&s, &st.paths) {
        Err(e) => Some(e),
        Ok(EngineConfig::Local { model_path, .. }) if !model_path.exists() => {
            Some("로컬 음성 인식 모델이 없어요. 설정 › 녹음에서 내려받아 주세요.".into())
        }
        Ok(_) => None,
    }
}

/// 용어집 낱말 (쉼표 · 줄바꿈 · 세미콜론으로 구분)
fn glossary_terms(glossary: &str) -> Vec<String> {
    glossary
        .split([',', '\n', ';'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// 용어집을 whisper 프롬프트로 (자연스러운 문장 형태가 효과가 좋다)
fn glossary_prompt(terms: &[String]) -> String {
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
    // 녹음 폴더 파일을 받아 적는 중이면 트레이의 녹음 단추는 '녹음 시작' (누르면 녹음을 먼저 한다)
    let recording = matches!(state, RecState::Recording | RecState::Starting) && !watch::job_running(app);
    status::refresh(app);
    let item = st.tray_toggle.lock().clone();
    if let Some(item) = item {
        let _ = item.set_text(if recording { "녹음 중지" } else { "녹음 시작" });
    }
    let item = st.tray_pause.lock().clone();
    if let Some(item) = item {
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
    if !paused {
        st.sleep_paused.store(false, Ordering::SeqCst);
        st.sleep_notified.store(false, Ordering::SeqCst);
    }
    status::refresh(app);
    let item = st.tray_pause.lock().clone();
    if let Some(item) = item {
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

/// PC 가 잠들거나 깨어남 (power.rs) — 실시간 녹음 중이면 일시정지해 두고, 깨어나면 한 번 알린다.
/// 파일 받아쓰기는 깨어난 뒤 그대로 이어서 하면 되므로 건드리지 않는다.
pub(crate) fn on_power(app: &AppHandle, ev: power::PowerEvent) {
    let Some(st) = app.try_state::<AppState>() else {
        return;
    };
    let live = *st.rec.lock() == RecState::Recording && !st.file_mode.load(Ordering::SeqCst);
    if !live {
        return;
    }
    let paused = st.pipeline.lock().as_ref().is_some_and(|p| p.is_paused());
    // 사람이 이미 일시정지해 둔 녹음은 그대로 둔다 (깨어나도 알리지 않음)
    if !paused && set_paused_inner(app, true).is_ok() {
        st.sleep_paused.store(true, Ordering::SeqCst);
        log::info!("절전 — 녹음을 일시정지함");
    }
    match ev {
        power::PowerEvent::Suspend => {
            st.sleep_notified.store(false, Ordering::SeqCst);
            // 잠든 사이 전원이 꺼져도 여기까지는 남게
            if let Err(e) = st.store.lock().flush() {
                log::warn!("절전 전 저장 실패: {e}");
            }
        }
        power::PowerEvent::Resume { slept_secs } => {
            if st.sleep_paused.load(Ordering::SeqCst) && !st.sleep_notified.swap(true, Ordering::SeqCst) {
                let _ = app.emit("bd://sleep", slept_secs);
            }
        }
    }
}

fn start_recording_inner(app: &AppHandle) -> CmdResult<()> {
    start_session(app, None, None)
}

/// 녹음(마이크·시스템 소리) 또는 파일 받아쓰기를 시작한다. `source` 를 주면 설정 대신 그 소리를 받는다(회의 시작 알림)
pub(crate) fn start_session(app: &AppHandle, file: Option<&Path>, source: Option<SourceKind>) -> CmdResult<()> {
    let st = app.state::<AppState>();
    // 녹음 폴더 파일을 받아 적는 중이면 멈추고 녹음을 먼저 (그 파일은 녹음 뒤 다시) — 시작이 끝날 때까지 새 파일은 미룬다
    let _way = if file.is_none() { Some(watch::make_way(app)?) } else { None };
    // 마이크를 받는 녹음이면 듣던 말로 입력을 멈춘다 (같은 마이크 — 말로 입력한 말이 회의 기록에 섞이지 않게)
    if file.is_none() {
        let mic = source.unwrap_or_else(|| st.settings.lock().source).uses_mic();
        if mic {
            dictate::stop_for_meeting(app);
        }
    }
    {
        let mut rec = st.rec.lock();
        if *rec != RecState::Idle {
            return Err("이미 녹음 중이거나 정리 중입니다".into());
        }
        *rec = RecState::Starting;
    }
    set_rec(app, RecState::Starting);

    // 안에서 패닉이 나도(손상된 파일 등) Err 로 바꿔 아래에서 대기 상태로 되돌린다 — '시작 중'에 갇히지 않게
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> CmdResult<Pipeline> {
        let s = st.settings.lock().clone();
        // 녹음 파일 · 녹음 폴더는 기다리며 받아 적으므로, 녹음 뒤 다시 받아 적기 모델을 골라 두었으면 처음부터 그 모델로
        let engine = match (file, redo::model(&s, &st.paths.models)) {
            (Some(_), Ok((_, model_path))) => EngineConfig::Local {
                model_path,
                threads: s.threads,
                beam_size: s.beam_size,
            },
            _ => build_engine(&s, &st.paths)?,
        };
        *st.api_provider.lock() = match &engine {
            EngineConfig::Api { base_url, model, .. } => {
                Some((badasugi_core::usage::provider_of(base_url), model.trim().to_string()))
            }
            _ => None,
        };
        let mut doc = SessionDoc::new(engine.label());
        doc.id = st.store.lock().unique_id(&doc.id);
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
        // 이름을 기억해 둔 목소리 → 녹음 중 같은 목소리가 들리면 이름을 붙인다
        let known_voices = if s.remember_voices && s.diarization.enabled {
            app.state::<VoiceBook>().known()
        } else {
            Vec::new()
        };
        let terms = glossary_terms(&s.glossary);
        let cfg = PipelineConfig {
            capture: CaptureConfig {
                source: source.unwrap_or(s.source),
                mic_device: s.mic_device.clone(),
            },
            engine,
            diarization: s.diarization.clone(),
            speaker_model: speaker_model.exists().then_some(speaker_model),
            segmenter: Default::default(),
            language: s.language.clone(),
            prompt: glossary_prompt(&terms),
            keywords: terms,
            record_path,
            known_voices,
            // 말하는 동안 미리 보기는 실시간 녹음만 (파일은 녹음보다 빨리 받아 적으므로 필요 없다)
            preview: file.is_none() && s.wants_preview(),
        };
        st.store.lock().begin(doc);
        badasugi_core::perf::disable_power_throttling();
        let app2 = app.clone();
        let sink: EventSink = Arc::new(move |ev| on_pipeline_event(&app2, ev));
        st.file_mode.store(file.is_some(), Ordering::SeqCst);
        st.file_pct.store(0, Ordering::SeqCst);
        st.save_warned.store(false, Ordering::SeqCst);
        st.live_pos_ms.store(0, Ordering::SeqCst);
        st.sleep_paused.store(false, Ordering::SeqCst);
        st.sleep_notified.store(false, Ordering::SeqCst);
        st.echo.lock().clear();
        st.auto_named.lock().clear();
        match file {
            Some(f) => Pipeline::start_file(f, cfg, sink).map_err(err),
            None => Pipeline::start(cfg, sink).map_err(err),
        }
    }))
    .unwrap_or_else(|_| Err("시작하는 중 예기치 못한 오류가 났어요. 다른 파일로 다시 시도해 주세요.".to_string()));

    match result {
        Ok(p) => {
            *st.speaker_hints.lock() = Some(p.speaker_hints());
            *st.pipeline.lock() = Some(p);
            *st.rec_started.lock() = Some(Instant::now());
            let src = file.is_none().then(|| source.unwrap_or(st.settings.lock().source));
            *st.rec_source.lock() = src;
            let _ = app.emit("bd://rec-source", src);
            set_rec(app, RecState::Recording);
            // 실시간 번역 (설정에서 켠 경우) — 확정된 문장이 쌓이는 대로 번역한다
            if st.settings.lock().translate.live {
                if let Some(id) = st.store.lock().current.as_ref().map(|d| d.id.clone()) {
                    spawn_live_translation(app.clone(), id);
                }
            }
            if file.is_none() {
                apply_calendar(app);
            }
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

/// 녹음을 시작했다 — 지금 시각의 Outlook 일정이 있으면 그 제목을 회의 제목으로, 참석자를 화자 이름 후보로
/// (설정에서 켰을 때만 · Outlook 을 기다리는 동안 녹음은 이미 돌고 있다)
fn apply_calendar(app: &AppHandle) {
    if !app.state::<AppState>().settings.lock().calendar {
        return;
    }
    let Some(id) = app.state::<AppState>().store.lock().current.as_ref().map(|d| d.id.clone()) else {
        return;
    };
    let app = app.clone();
    std::thread::spawn(move || {
        let Some(ev) = calendar::current(&app) else {
            return;
        };
        let applied = {
            let st = app.state::<AppState>();
            let mut store = st.store.lock();
            match store.current.as_mut().filter(|d| d.id == id) {
                // 사람이 이미 제목을 고쳤으면 제목은 두고 참석자만
                Some(d) => {
                    if d.auto_title && !ev.subject.is_empty() {
                        d.title = ev.subject.chars().take(80).collect();
                        d.auto_title = false;
                    }
                    d.attendees = ev.attendees.clone();
                    store.mark_dirty();
                    store.current.as_ref().map(|d| (d.title.clone(), d.auto_title))
                }
                None => None,
            }
        };
        if let Some((title, auto_title)) = applied {
            log::info!("일정에서 회의 제목을 가져옴 (참석자 {}명)", ev.attendees.len());
            let _ = app.emit(
                "bd://calendar",
                serde_json::json!({ "id": id, "title": title, "autoTitle": auto_title, "attendees": ev.attendees, "subject": ev.subject }),
            );
        }
    });
}

pub(crate) fn stop_recording_inner(app: &AppHandle) {
    let st = app.state::<AppState>();
    watch::on_stop(app);
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
        // 녹음 폴더 파일을 받아 적는 중이면 녹음을 먼저 (그 파일은 녹음을 마친 뒤 다시)
        _ if watch::job_running(app) => start_recording_inner(app),
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
    start_session(&app, Some(&p), None)
}

fn on_pipeline_event(app: &AppHandle, ev: Event) {
    let st = app.state::<AppState>();
    // 진단 로그 (앱이 만든 문구만 — 녹취록 내용은 남기지 않는다)
    match &ev {
        Event::Started {
            mic,
            system,
            engine,
            diarization,
        } => log::info!("녹음 시작됨: 엔진 {engine}, 마이크 {mic:?}, 시스템/파일 {system:?}, 화자 구분 {diarization}"),
        Event::Error { message } => {
            log::error!("{message}");
            watch::on_error(app, message);
        }
        Event::Warning { message } => log::warn!("{message}"),
        Event::Failed { id, message, retry } => {
            log::warn!("발화 {id} 인식 실패{}: {message}", if *retry { " (녹음 뒤 다시 시도)" } else { "" });
            if !*retry {
                watch::on_utterance(app, Some(message));
            }
        }
        Event::Status { message } => log::info!("{message}"),
        _ => {}
    }
    match &ev {
        Event::Transcript {
            id,
            start_ms,
            end_ms,
            speaker,
            text,
            ..
        } => {
            // 녹음 중 다시 들은 소리가 스피커·시스템 소리로 다시 녹음돼 받아 적힌 것이면 버린다
            let now = st.live_pos_ms.load(Ordering::Relaxed);
            if is_echo(&st.echo.lock(), *start_ms, *end_ms, text, now) {
                log::info!("다시 듣기 메아리로 보고 버림 (발화 {id}, {}자)", text.chars().count());
                let _ = app.emit("bd://event", &Event::Discarded { id: *id });
                return;
            }
            watch::on_utterance(app, None);
            let mut store = st.store.lock();
            if let Some(doc) = store.current.as_mut() {
                let speaker = doc.resolve(*speaker);
                doc.upsert(Utterance {
                    id: *id,
                    start_ms: *start_ms,
                    end_ms: *end_ms,
                    speaker,
                    text: text.clone(),
                    edited: false,
                });
            }
            store.mark_dirty();
            if let Err(e) = store.autosave() {
                if !st.save_warned.swap(true, Ordering::SeqCst) {
                    let _ = app.emit(
                        "bd://toast",
                        format!("회의 기록을 저장하지 못하고 있어요 ({e}). 녹음은 계속돼요 — 문서 폴더·디스크 공간을 확인해 주세요."),
                    );
                }
            }
        }
        Event::SpeakerNamed { speaker, name } => {
            // 기억해 둔 목소리로 알아본 화자: 아직 이름이 없고, 그 이름을 다른 화자가 쓰고 있지 않을 때만
            let target = {
                let mut store = st.store.lock();
                let Some(doc) = store.current.as_mut() else {
                    return;
                };
                let target = doc.resolve(Some(*speaker)).unwrap_or(*speaker);
                if doc.speakers.contains_key(&target) || doc.speakers.values().any(|n| n == name) {
                    return;
                }
                doc.speakers.insert(target, name.clone());
                store.mark_dirty();
                target
            };
            st.auto_named.lock().insert(target, name.clone());
            let _ = app.emit(
                "bd://event",
                &Event::SpeakerNamed {
                    speaker: target,
                    name: name.clone(),
                },
            );
            return;
        }
        Event::SpeakersRefined { labels, utterances } => {
            on_speakers_refined(app, labels, utterances);
            return;
        }
        Event::ApiUsage { audio_ms, ok } => {
            // 실제로 보낸 요청만 센다 — 실패한 요청도 요청 수에 넣고, 연결 문제로 미뤄 두거나 건너뛴 발화는 세지 않는다
            let provider = st.api_provider.lock().clone();
            if let Some((p, model)) = provider {
                let book = app.state::<usage_book::UsageBook>();
                if *ok {
                    book.stt(p, &model, *audio_ms as f64 / 1000.0);
                } else {
                    book.stt_failed(p);
                }
            }
            return;
        }
        Event::Progress { done_ms, total_ms } if *total_ms > 0 => {
            let pct = (done_ms * 100 / total_ms).min(100) as usize + 1;
            if st.file_pct.swap(pct, Ordering::SeqCst) != pct {
                status::refresh(app);
            }
        }
        Event::Level { position_ms, .. } => {
            st.live_pos_ms.store(*position_ms, Ordering::Relaxed);
            if let Some(mut store) = st.store.try_lock() {
                if let Some(doc) = store.current.as_mut() {
                    doc.duration_ms = doc.duration_ms.max(*position_ms);
                }
            }
        }
        Event::Stopped => {
            // 파일 받아쓰기는 끝까지 가면 저절로 멈춘다
            drop(st.pipeline.lock().take());
            let ended_id = st.store.lock().current.as_ref().map(|d| d.id.clone());
            *st.speaker_hints.lock() = None;
            let was_file = st.file_mode.swap(false, Ordering::SeqCst);
            {
                let mut store = st.store.lock();
                if let Some(d) = store.current.as_ref() {
                    log::info!(
                        "녹음 끝: 기록 {} · 발화 {}개 · 길이 {}초",
                        d.id,
                        d.utterances.len(),
                        d.duration_ms / 1000
                    );
                }
                // 마지막 저장: 실패하면(디스크 가득·백신·OneDrive 잠금 등) 잠깐 뒤 다시, 그래도 안 되면 앱 폴더에 따로 저장하고 알린다
                let mut saved = store.flush();
                for _ in 0..2 {
                    if saved.is_ok() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    saved = store.flush();
                }
                if let Err(e) = saved {
                    log::warn!("기록 저장 실패: {e}");
                    let rescue = st.paths.config.join("recovered");
                    let msg = match store.current.as_ref().map(|d| session::write_doc(&rescue, d)) {
                        Some(Ok(())) => format!(
                            "회의 기록을 문서 폴더에 저장하지 못해 따로 저장했어요: {} ({e})",
                            rescue.display()
                        ),
                        _ => format!("회의 기록을 저장하지 못했어요: {e}"),
                    };
                    let _ = app.emit("bd://toast", msg);
                }
                if let Some(doc) = store.current.take() {
                    if doc.utterances.is_empty() {
                        let audio = store.audio_path(&doc.id);
                        // 받아 적힌 문장이 없어도 회의 중 직접 쓴 메모 · 요약은 버리지 않는다 (녹음 파일도 함께 둔다)
                        if doc.has_content() {
                            if session::write_doc(&store.dir, &doc).is_ok() {
                                let _ = app.emit(
                                    "bd://toast",
                                    "받아 적힌 문장은 없지만 메모는 회의 기록에 남겨 두었어요".to_string(),
                                );
                            }
                        // 받아 적힌 문장이 없어도(인터넷 끊김·API 키 오류 등) 5초 넘는 녹음은 지우지 않고 기록으로 남긴다
                        // → 나중에 회의 기록에서 다시 듣거나, 녹음 파일로 다시 받아 적을 수 있다
                        } else if doc.audio_file.is_some() && doc.duration_ms >= 5_000 && audio.is_file() {
                            if session::write_doc(&store.dir, &doc).is_ok() {
                                let _ = app.emit(
                                    "bd://toast",
                                    "받아 적힌 문장은 없지만 녹음 파일은 회의 기록에 남겨 두었어요".to_string(),
                                );
                            }
                        } else {
                            let _ = std::fs::remove_file(audio);
                        }
                    }
                }
            }
            *st.rec_started.lock() = None;
            *st.rec_source.lock() = None;
            app.state::<usage_book::UsageBook>().save();
            if st.settings.lock().unload_when_idle {
                badasugi_core::asr::whisper::unload();
            }
            badasugi_core::perf::restore_power_throttling();
            watch::job_ended(app, ended_id.as_deref());
            set_rec(app, RecState::Idle);
            if st.quit_after_stop.load(Ordering::SeqCst) {
                app.exit(0);
                return;
            }
            // 실시간 녹음이었으면 더 정확한 모델로 다시 받아 적기 (설정에서 켰을 때 · 파일은 처음부터 그 모델로 받아 적었다)
            if let (false, Some(id)) = (was_file, ended_id.as_deref()) {
                redo::after_recording(app, id);
            }
            update::maybe_install(app);
        }
        _ => {}
    }
    let _ = app.emit("bd://event", &ev);
}

/// 회의가 끝난 뒤 다시 정리한 화자를 기록에 반영하고, 이름 붙은 목소리를 기억한다
fn on_speakers_refined(app: &AppHandle, labels: &[[u64; 3]], utterances: &[badasugi_core::diarize::UtteranceVoice]) {
    let st = app.state::<AppState>();
    let remember = st.settings.lock().remember_voices;
    let known = if remember { app.state::<VoiceBook>().known() } else { Vec::new() };
    let auto = st.auto_named.lock().clone();
    let (id, dir, r, kept, named) = {
        let mut store = st.store.lock();
        let dir = store.dir.clone();
        let Some(doc) = store.current.as_mut().filter(|d| !d.utterances.is_empty()) else {
            return;
        };
        let r = doc.apply_refinement(labels, utterances, &auto, &known);
        // 기록에 남은 문장의 목소리만 (메아리로 버렸거나 빈 문장은 뺀다)
        let ids: BTreeSet<u64> = doc.utterances.iter().map(|u| u.id).collect();
        let kept: Vec<_> = utterances.iter().filter(|u| ids.contains(&u.id)).cloned().collect();
        let named = doc.named_voices(&kept);
        let id = doc.id.clone();
        store.mark_dirty();
        (id, dir, r, kept, named)
    };
    log::info!(
        "화자 다시 정리 반영: 문장 {}개 옮김 · 화자 {}명 · 목소리로 이름 {}명",
        r.moved,
        r.speakers,
        r.named
    );
    if remember && !kept.is_empty() {
        // 회의가 끝난 뒤에 이름을 붙이거나 문단을 옮겨도 기억할 수 있게 문장별 목소리를 기록 옆에 둔다
        match voices::write_session_voices(&dir, &id, &kept) {
            Ok(()) => app.state::<VoiceBook>().sync_session(&id, &named),
            Err(e) => log::warn!("문장 목소리 저장 실패: {e}"),
        }
    }
    let _ = app.emit(
        "bd://event",
        serde_json::json!({ "type": "speakersRefined", "moved": r.moved, "named": r.named, "speakers": r.speakers }),
    );
}

/// 기록에서 이름 붙은 화자의 목소리를 목소리 기억에 반영한다 (이름을 바꾸거나 화자를 합친 뒤)
fn sync_session_voices(app: &AppHandle, id: &str) {
    let st = app.state::<AppState>();
    if !st.settings.lock().remember_voices {
        return;
    }
    let (doc, dir) = {
        let store = st.store.lock();
        // 녹음 중인 기록은 끝날 때 한꺼번에
        if store.current.as_ref().is_some_and(|c| c.id == id) {
            return;
        }
        (store.load(id), store.dir.clone())
    };
    let Ok(doc) = doc else {
        return;
    };
    let voices = voices::read_session_voices(&dir, id);
    if !voices.is_empty() {
        app.state::<VoiceBook>().sync_session(id, &doc.named_voices(&voices));
    }
}

/// 설정 › 화자 › 목소리 기억: 기억한 목소리 목록
#[tauri::command]
fn list_voices(app: AppHandle) -> Vec<voices::VoiceInfo> {
    app.state::<VoiceBook>().list()
}

/// 기록 폴더와 지금 녹음 중인 기록 id
fn sessions_dir_and_live(app: &AppHandle) -> (PathBuf, Option<String>) {
    let st = app.state::<AppState>();
    let store = st.store.lock();
    (store.dir.clone(), store.current.as_ref().map(|d| d.id.clone()))
}

/// 기억한 목소리 하나 잊기: 그 이름이 붙은 문장의 목소리를 회의별 파일에서도 지운다
/// (예전 회의의 제목 · 글자만 고쳐 저장해도 다시 기억되지 않게)
#[tauri::command]
async fn forget_voice(app: AppHandle, name: String) -> CmdResult<()> {
    let (dir, live) = sessions_dir_and_live(&app);
    let n = name.clone();
    let fixed = tauri::async_runtime::spawn_blocking(move || voices::strip_name(&dir, &n, live.as_deref()))
        .await
        .map_err(err)?;
    app.state::<VoiceBook>().forget(&name);
    log::info!("기억한 목소리 하나를 잊음 (회의 {fixed}개의 목소리도 지움)");
    Ok(())
}

#[tauri::command]
async fn forget_all_voices(app: AppHandle) -> CmdResult<()> {
    let (dir, live) = sessions_dir_and_live(&app);
    tauri::async_runtime::spawn_blocking(move || voices::strip_all(&dir, live.as_deref()))
        .await
        .map_err(err)?;
    app.state::<VoiceBook>().forget_all();
    log::info!("기억한 목소리를 모두 잊음");
    Ok(())
}

// ───────────────────────── 기록 ─────────────────────────

#[tauri::command]
async fn list_sessions(app: AppHandle) -> Vec<SessionMeta> {
    // 파일을 모두 읽는 일이라, 녹음 중 오디오 처리가 기록 잠금을 기다리지 않게 잠금 밖·작업 스레드에서
    let dir = app.state::<AppState>().store.lock().dir.clone();
    tauri::async_runtime::spawn_blocking(move || SessionStore::new(dir).list())
        .await
        .unwrap_or_default()
}

#[tauri::command]
async fn search_sessions(app: AppHandle, query: String) -> Vec<SearchHit> {
    let dir = app.state::<AppState>().store.lock().dir.clone();
    tauri::async_runtime::spawn_blocking(move || SessionStore::new(dir).search(&query))
        .await
        .unwrap_or_default()
}

/// 녹음 중 다시 들은 구간 (녹음 시간축)과 그때 재생한 문장들
struct EchoWindow {
    from_ms: u64,
    /// 아직 재생 중이면 None
    to_ms: Option<u64>,
    texts: Vec<String>,
}

/// 메아리로 볼 글자쌍 일치율
const ECHO_RATIO: f32 = 0.6;

/// 이 발화가 다시 듣기 구간에 녹음됐고, 그때 재생한 문장과 거의 같으면 메아리
fn is_echo(windows: &[EchoWindow], start_ms: u64, end_ms: u64, text: &str, now_ms: u64) -> bool {
    windows.iter().any(|w| {
        // 소리가 스피커에서 나와 다시 녹음되기까지·발화 끝맺음까지 여유를 둔다
        let to = w.to_ms.unwrap_or(now_ms) + 2_500;
        let from = w.from_ms.saturating_sub(500);
        end_ms >= from && start_ms <= to && badasugi_core::text::echo_ratio(text, &w.texts) >= ECHO_RATIO
    })
}

/// 화면이 녹음 중 다시 듣기를 시작·계속·멈출 때 알려 준다 (재생 중인 문장들과 함께)
#[tauri::command]
fn set_playback(app: AppHandle, playing: bool, texts: Vec<String>) {
    let st = app.state::<AppState>();
    if *st.rec.lock() != RecState::Recording || st.file_mode.load(Ordering::SeqCst) {
        return;
    }
    let now = st.live_pos_ms.load(Ordering::Relaxed);
    let mut echo = st.echo.lock();
    let open = echo.last_mut().filter(|w| w.to_ms.is_none());
    match (playing, open) {
        (true, Some(w)) => w.texts.extend(texts),
        (true, None) => echo.push(EchoWindow {
            from_ms: now,
            to_ms: None,
            texts,
        }),
        (false, Some(w)) => w.to_ms = Some(now),
        (false, None) => {}
    }
    // 오래 녹음해도 쌓이지 않게 최근 것만
    if echo.len() > 50 {
        echo.remove(0);
    }
}

/// 기록의 음성 파일 경로 (다시 듣기). 화면이 읽을 수 있게 그 파일 하나만 허용한다.
///
/// - 실시간 녹음: 기록 폴더의 `<id>.opus` (녹음 중에도 1초마다 Ogg 페이지를 닫아 써서 그때까지 들을 수 있다).
///   예전 기록은 `<id>.wav`.
/// - 파일 받아쓰기: 원본 파일
#[tauri::command]
async fn session_audio(app: AppHandle, id: String) -> CmdResult<Option<String>> {
    off_main(move || session_audio_now(&app, &id)).await
}

/// 기록 파일을 읽고 쓰는 명령은 작업 스레드에서 — 동기 명령은 화면(메인) 스레드에서 돌아, 녹음 중 자동 저장이
/// 기록 잠금을 쥐고 있거나 문서 폴더(OneDrive)에 쓰는 동안 창이 멈칫한다.
/// (화면은 이 명령들을 보낸 순서대로 하나씩 보내 저장 · 읽기 순서가 뒤바뀌지 않는다)
async fn off_main<T: Send + 'static>(f: impl FnOnce() -> CmdResult<T> + Send + 'static) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(err)?
}

fn session_audio_now(app: &AppHandle, id: &str) -> CmdResult<Option<String>> {
    let st = app.state::<AppState>();
    let (doc, dir) = {
        let store = st.store.lock();
        (store.load(id).map_err(err)?, store.dir.clone())
    };
    let wav = doc
        .audio_file
        .as_deref()
        // 기록 파일을 고쳐 다른 경로를 가리키게 해도 기록 폴더 밖은 열지 않는다
        .and_then(|f| std::path::Path::new(f).file_name())
        .map(|f| dir.join(f))
        .filter(|p| p.is_file());
    let original = doc.source_path.as_deref().map(PathBuf::from).filter(|p| {
        // 먼저 로컬 드라이브인지 본다: \\server\… 같은 경로는 is_file() 만 해도 Windows 가 그 서버에 로그인을 시도한다
        is_local_disk(p)
            && p.is_file()
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

/// C:\… 같은 로컬 드라이브 경로인가 (네트워크·장치 경로 제외)
fn is_local_disk(p: &Path) -> bool {
    use std::path::{Component, Prefix};
    matches!(
        p.components().next(),
        Some(Component::Prefix(x)) if matches!(x.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    )
}

#[tauri::command]
async fn load_session(app: AppHandle, id: String) -> CmdResult<SessionDoc> {
    off_main(move || app.state::<AppState>().store.lock().load(&id).map_err(err)).await
}

#[tauri::command]
async fn save_session(app: AppHandle, doc: SessionDoc) -> CmdResult<()> {
    off_main(move || save_session_now(&app, doc)).await
}

fn save_session_now(app: &AppHandle, doc: SessionDoc) -> CmdResult<()> {
    let id = doc.id.clone();
    let st = app.state::<AppState>();
    let live = {
        let mut store = st.store.lock();
        store.save_doc(doc).map_err(err)?;
        store.current.as_ref().filter(|c| c.id == id).map(|c| (c.speaker_map.clone(), c.speakers.clone()))
    };
    if let Some((merges, names)) = live {
        // 사람이 자동 이름을 고쳤으면 그 화자는 더 이상 자동 이름이 아니다
        let manual: BTreeMap<u32, String> = {
            let mut auto = st.auto_named.lock();
            auto.retain(|l, n| names.get(l) == Some(n));
            names.into_iter().filter(|(l, n)| !auto.contains_key(l) && voices::is_real_name(n)).collect()
        };
        // 녹음 중에 합친 화자 · 사람이 붙인 이름은 회의 후 다시 정리에서도 지키게 파이프라인에 알린다
        // (합친 화자는 한 화자로 묶고, 이름이 다른 화자끼리는 합치지 않는다)
        if let Some(h) = st.speaker_hints.lock().as_ref() {
            *h.lock() = badasugi_core::diarize::SpeakerHints { merges, names: manual };
        }
    }
    sync_session_voices(app, &id);
    Ok(())
}

/// 회의 후 화자 다시 정리를 되돌린다 (녹음 중에 정해진 화자 배정 · 이름으로) → 되돌린 기록
#[tauri::command]
async fn undo_refinement(app: AppHandle, id: String) -> CmdResult<SessionDoc> {
    off_main(move || {
        let doc = app.state::<AppState>().store.lock().undo_refinement(&id).map_err(err)?;
        log::info!("화자 다시 정리를 되돌림: 기록 {id}");
        sync_session_voices(&app, &id);
        Ok(doc)
    })
    .await
}

#[tauri::command]
async fn delete_session(app: AppHandle, id: String) -> CmdResult<()> {
    off_main(move || delete_session_now(&app, &id)).await
}

fn delete_session_now(app: &AppHandle, id: &str) -> CmdResult<()> {
    let st = app.state::<AppState>();
    if st.store.lock().current.as_ref().is_some_and(|c| c.id == id) {
        return Err("녹음 중인 기록은 지울 수 없습니다".into());
    }
    let r = st.store.lock().delete(id).map_err(err);
    // 이 회의에서 기억한 목소리도 함께 잊는다 — 파일 하나를 못 지웠어도 지우려던 회의다
    app.state::<VoiceBook>().forget_session(id);
    r
}

#[tauri::command]
async fn export_session(app: AppHandle, id: String, format: String, path: String, tr: Option<String>) -> CmdResult<()> {
    off_main(move || export_session_now(&app, &id, &format, path, session::TrMode::parse(tr.as_deref()))).await
}

/// `tr`: 녹취록을 원문 · 번역 · 둘 다 (번역이 없는 문장은 원문, JSON 은 늘 그대로)
fn export_session_now(app: &AppHandle, id: &str, format: &str, path: String, tr: session::TrMode) -> CmdResult<()> {
    let doc = load_for_export(app, id, format != "json")?;
    if format == "docx" {
        return std::fs::write(path, export::docx(&doc, tr)).map_err(err);
    }
    let body = match format {
        "md" => doc.markdown(tr),
        "txt" => doc.text(tr),
        "srt" => doc.srt(tr),
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

/// 내보낼 기록 — 설정에서 "내보낼 때 가리기" 를 켰으면 가린 사본 (`maskable`: JSON 은 원본 데이터라 그대로).
/// 파일 내보내기 · PDF · 전체 복사 · 보내기(메신저 · 메일 초안 · 일정 파일) 모두 여기서
fn load_for_export(app: &AppHandle, id: &str, maskable: bool) -> CmdResult<SessionDoc> {
    let st = app.state::<AppState>();
    let doc = st.store.lock().load(id).map_err(err)?;
    let mask = maskable && st.settings.lock().privacy.mask_export;
    Ok(if mask { privacy::mask_doc(&doc) } else { doc })
}

/// 보관 기간이 지난 녹음 파일을 지운다. 기록 폴더는 잠금 없이 훑고(기록이 많아도 녹음 · 저장을 붙잡지 않게),
/// 지울 기록마다 잠금을 쥐고 다시 본다 — 그 사이 저장과 엇갈리지 않게, 녹음 중인 기록은 빼고
fn expire_audio_now(app: &AppHandle) -> usize {
    if smoke::enabled() {
        return 0;
    }
    let st = app.state::<AppState>();
    let days = st.settings.lock().privacy.audio_keep_days;
    if days == 0 {
        return 0;
    }
    let today = chrono::Local::now().date_naive();
    let dir = st.store.lock().dir.clone();
    let mut n = 0;
    for json in privacy::expired(&dir, days, today) {
        let store = st.store.lock();
        let skip = store.current.as_ref().map(|d| d.id.clone());
        if privacy::expire_one(&dir, &json, days, today, skip.as_deref()) {
            n += 1;
        }
    }
    if n > 0 {
        log::info!("녹음 보관 기간 {days}일: 녹음 파일 {n}개 지움");
    }
    n
}

/// 앱을 켤 때 · 6시간마다 보관 기간이 지난 녹음 파일을 지운다
fn spawn_audio_expiry(app: AppHandle) {
    let _ = std::thread::Builder::new().name("bd-expire".into()).spawn(move || loop {
        // 켜자마자는 화면 · 모델을 먼저 — 1분 뒤에
        std::thread::sleep(std::time::Duration::from_secs(60));
        expire_audio_now(&app);
        std::thread::sleep(std::time::Duration::from_secs(6 * 3600 - 60));
    });
}

/// PDF 로 낼 회의록 HTML — 화면이 인쇄용 칸(`#print-doc`)에 넣고 [`print_pdf`] 를 부른다
#[tauri::command]
async fn export_html(app: AppHandle, id: String, tr: Option<String>) -> CmdResult<String> {
    off_main(move || {
        let doc = load_for_export(&app, &id, true)?;
        Ok(export::html(&doc, session::TrMode::parse(tr.as_deref())))
    })
    .await
}

/// 화면의 인쇄용 칸을 PDF 파일로 (WebView2 PrintToPdf — 인쇄 대화상자 없이)
#[tauri::command]
async fn print_pdf(app: AppHandle, path: String, title: String) -> CmdResult<()> {
    off_main(move || {
        // 페이지 머리글의 제목도 본문(export_html)처럼 — 화면은 원래 제목을 넘긴다
        let title = if app.state::<AppState>().settings.lock().privacy.mask_export { privacy::mask(&title).0 } else { title };
        let w = app.get_webview_window("main").ok_or("창을 찾을 수 없어요")?;
        export::print_pdf(&w, Path::new(&path), &title)
    })
    .await
}

/// 요약을 보내기 좋게 — 할 일 수 (전체, 기한을 날짜로 읽을 수 있는 것)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ShareInfo {
    todos: usize,
    dated: usize,
}

#[tauri::command]
async fn share_info(app: AppHandle, id: String) -> CmdResult<ShareInfo> {
    off_main(move || {
        let doc = app.state::<AppState>().store.lock().load(&id).map_err(err)?;
        let (todos, dated) = share::todo_counts(&doc);
        Ok(ShareInfo { todos, dated })
    })
    .await
}

/// 메신저용 짧은 정리 (요약을 다시 쓴 것, AI 를 부르지 않는다)
#[tauri::command]
async fn share_messenger(app: AppHandle, id: String) -> CmdResult<String> {
    off_main(move || {
        let doc = load_for_export(&app, &id, true)?;
        share::messenger(&doc).ok_or_else(|| "요약이 없어요. 먼저 요약해 주세요".to_string())
    })
    .await
}

/// 후속 메일 초안을 새 메일 창으로 연다 (.eml, X-Unsent) → 메일 본문 (화면이 클립보드에도 넣는다 —
/// 메일 앱이 .eml 을 읽기 전용으로 열거나 열지 못해도 새 메일에 붙여 넣을 수 있게)
#[tauri::command]
async fn open_mail_draft(app: AppHandle, id: String) -> CmdResult<String> {
    off_main(move || {
        let doc = load_for_export(&app, &id, true)?;
        let (subject, body) = share::email(&doc).ok_or_else(|| "요약이 없어요. 먼저 요약해 주세요".to_string())?;
        let safe: String = doc.id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        let path = std::env::temp_dir().join(format!("badasugi-mail-{safe}.eml"));
        std::fs::write(&path, share::eml(&subject, &body)).map_err(err)?;
        log::info!("후속 메일 초안 열기: {}", path.display());
        if let Err(e) = app.opener().open_path(path.display().to_string(), None::<&str>) {
            log::warn!("메일 초안을 열지 못함: {e}");
        }
        Ok(body)
    })
    .await
}

/// 기한이 있는 할 일 → 일정 파일(.ics) → 넣은 할 일 수
#[tauri::command]
async fn export_todos_ics(app: AppHandle, id: String, path: String) -> CmdResult<usize> {
    off_main(move || {
        let doc = load_for_export(&app, &id, true)?;
        let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let (text, n) = share::ics(&doc, &now);
        if n == 0 {
            return Err("기한을 날짜로 읽을 수 있는 할 일이 없어요".into());
        }
        std::fs::write(path, text).map_err(err)?;
        Ok(n)
    })
    .await
}

/// 화면에서 바로 복사하는 글(요약 · 묻고 답한 것) — "내보낼 때 가리기" 를 켰으면 가린 것
#[tauri::command]
fn export_text(app: AppHandle, text: String) -> String {
    if app.state::<AppState>().settings.lock().privacy.mask_export {
        privacy::mask(&text).0
    } else {
        text
    }
}

#[tauri::command]
async fn session_text(app: AppHandle, id: String, format: String) -> CmdResult<String> {
    off_main(move || {
        // 전체 복사도 밖으로 나가는 글 — 내보낼 때 가리기를 따른다
        let doc = load_for_export(&app, &id, true)?;
        Ok(match format.as_str() {
            "md" => doc.to_markdown(),
            _ => doc.to_text(),
        })
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SummaryDto {
    pub text: String,
    /// 자동 제목이었다면 AI 가 붙인 새 제목
    pub title: Option<String>,
    /// 쓴 요약 틀 id
    pub template: String,
}

struct BusyGuard<'a>(&'a AtomicUsize);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[tauri::command]
/// `template` 은 쓸 요약 틀 — 없으면 이 회의에 마지막으로 쓴 틀, 그것도 없으면 설정의 기본 틀
async fn summarize(app: AppHandle, id: String, token: u64, template: Option<String>) -> CmdResult<SummaryDto> {
    summarize_doc(&app, id, token, template, false).await
}

/// 자동 요약을 넣지 않았다 — 그 사이 사람이 요약을 고쳤거나 직접 요약했다
pub(crate) const SUMMARY_KEPT: &str = "그 사이 요약이 생겨 자동 요약은 넣지 않았어요";

/// 회의를 요약해 기록에 얹는다 (화면의 요약 · 녹음 폴더 지켜보기의 자동 요약).
/// `auto` 면 요약하는 사이 사람이 요약을 고쳐 저장했거나 직접 요약한 것을 덮어쓰지 않는다 ([`SUMMARY_KEPT`])
pub(crate) async fn summarize_doc(
    app: &AppHandle,
    id: String,
    token: u64,
    template: Option<String>,
    auto: bool,
) -> CmdResult<SummaryDto> {
    let st = app.state::<AppState>();
    st.busy_ops.fetch_add(1, Ordering::SeqCst);
    let _busy = BusyGuard(&st.busy_ops);
    let (cfg, doc) = {
        let cfg = st.settings.lock().summary.clone();
        let doc = st.store.lock().load(&id).map_err(err)?;
        (cfg, doc)
    };
    let tpl = templates::resolve(&cfg, template.as_deref().or(doc.summary_template.as_deref()));
    log::info!("요약 틀: {} ({})", tpl.name, tpl.id);
    let tpl_id = tpl.id.clone();
    let doc2 = doc.clone();
    // AI 로 보낼 사본만 가린다 (설정에서 켰으면) — 저장은 원본(doc2 · 최신 기록)에
    let doc = privacy::for_ai(doc, &st.settings.lock().privacy);
    let cancel = st.summary_cancel.lock().entry(token).or_default().clone();
    // 진행을 화면에 알린다 (답을 쓰는 섹션 · 긴 회의는 구간까지).
    // 멈춘 요약은 보내 둔 요청의 답을 끝까지 받으므로 더 알리지 않고, 화면도 요약 번호로 가려 받는다
    // (멈추고 바로 같은 회의를 다시 요약해도 앞 요약의 진행이 새 요약을 덮지 않게)
    let (app2, id2, cancel2) = (app.clone(), id.clone(), cancel.clone());
    let progress = move |p: summary::Progress| {
        if cancel2.load(Ordering::SeqCst) {
            return;
        }
        let _ = app2.emit(
            "bd://summary-progress",
            serde_json::json!({
                "id": id2, "token": token, "done": p.done, "total": p.total, "waitSecs": p.wait_secs,
                "frac": p.frac, "writing": p.writing, "section": p.section,
            }),
        );
    };
    // 요청이 성공할 때마다 바로 사용량에 센다 — 긴 회의 중간에 실패해도 이미 쓴 만큼은 남게
    let app3 = app.clone();
    let on_usage = move |provider: &str, r: &summary::Reply| {
        app3.state::<usage_book::UsageBook>().summary(provider, &r.model, r.in_tokens, r.out_tokens, r.cache_write, r.cache_read);
    };
    // 요약은 따로 돌리고, 멈추면 보내 둔 요청(로컬 LLM 은 몇 분)을 기다리지 않고 화면을 먼저 돌려보낸다.
    // 따로 도는 쪽은 다음 요청부터 보내지 않고 끝난다.
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let cancel = cancel.clone();
        let _ = std::thread::Builder::new().name("bd-summary".into()).spawn(move || {
            let _ = tx.send(summary::summarize(&cfg, &tpl, &doc, &progress, &on_usage, &cancel));
        });
    }
    let res = tauri::async_runtime::spawn_blocking(move || loop {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(r) => return r,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if cancel.load(Ordering::SeqCst) => {
                return Err(anyhow::Error::new(summary::Cancelled))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("요약하다 문제가 생겼어요"))
            }
        }
    })
    .await;
    st.summary_cancel.lock().remove(&token);
    app.state::<usage_book::UsageBook>().save();
    let sum = res.map_err(err)?.map_err(err)?;
    // 요약하는 동안 바뀐 내용(이름 바꾸기 등)을 덮어쓰지 않도록 최신 기록에 요약만 얹는다
    let loaded = st.store.lock().load(&id);
    let mut latest = loaded.unwrap_or_else(|_| doc2.clone());
    if auto && (latest.summary_edited || latest.summary.as_deref().is_some_and(|s| !s.trim().is_empty())) {
        return Err(SUMMARY_KEPT.into());
    }
    // 그 사이 화자에 이름을 붙였거나 합쳤으면 요약 속 "화자 3" 도 지금 이름으로
    let text = latest.carry_speaker_names(&sum.text, &doc2);
    latest.summary = Some(text.clone());
    latest.summary_edited = false;
    latest.summary_template = Some(tpl_id.clone());
    let title = sum.title.filter(|_| latest.auto_title);
    if let Some(t) = &title {
        latest.title = t.clone();
        latest.auto_title = false;
    }
    let saved = st.store.lock().save_doc(latest);
    saved.map_err(err)?;
    Ok(SummaryDto { text, title, template: tpl_id })
}

/// 고를 수 있는 요약 틀 (기본 틀 + 설정의 내 틀)
#[tauri::command]
fn summary_templates(app: AppHandle) -> Vec<templates::TemplateInfo> {
    let cfg = app.state::<AppState>().settings.lock().summary.clone();
    templates::list(&cfg)
}

/// 그 번호의 요약을 멈춘다 — 아직 시작 전이면(화면이 앞선 저장을 기다리는 중) 시작하자마자 멈추게 신호를 먼저 꽂아 둔다
#[tauri::command]
fn cancel_summary(app: AppHandle, token: u64) {
    log::info!("요약 멈춤");
    cancel_token(&app, token);
}

/// 요약 · 묻기 번호에 멈춤 신호를 꽂는다
fn cancel_token(app: &AppHandle, token: u64) {
    let st = app.state::<AppState>();
    let mut map = st.summary_cancel.lock();
    // 이미 끝난 요약을 멈춰 남은 신호는 치운다 — 돌고 있는 요약이 쥐고 있는 신호(복제본이 있는 것)는 남긴다
    map.retain(|&t, flag| t == token || Arc::strong_count(flag) > 1);
    map.entry(token).or_default().store(true, Ordering::SeqCst);
}

/// 번역 한 묶음: 보내고, 받은 번역을 기록에 넣고, 화면에 알린다(`bd://translation`) → 보낸 문장
fn translate_step(
    app: &AppHandle,
    chat: &mut summary::Chat,
    provider: &str,
    lang: &str,
    doc: &SessionDoc,
    todo: &[u64],
    live: bool,
) -> anyhow::Result<Vec<u64>> {
    let mask = app.state::<AppState>().settings.lock().privacy.mask_ai;
    let b = translate::translate_batch(chat, lang, doc, todo, mask)?;
    let book = app.state::<usage_book::UsageBook>();
    book.summary(provider, &b.reply.model, b.reply.in_tokens, b.reply.out_tokens, b.reply.cache_write, b.reply.cache_read);
    book.save();
    // 실시간 번역의 답이 오는 사이 번역 단추가 이 회의를 맡았으면 버린다 (다른 언어면 단추의 번역을 지우게 된다)
    let yielded = live && app.state::<AppState>().tr_busy.lock().contains(&doc.id);
    if !b.items.is_empty() && !yielded {
        let items: BTreeMap<u64, translate::TrItem> = b.items.iter().cloned().collect();
        // 위 확인은 '지금 단추가 도는가'만 본다 — 답을 기다리는 사이 단추가 다른 언어로 시작해 끝까지 마쳤으면 지나친다.
        // 그래서 실시간 번역은 기록을 고치는 잠금 안에서 언어를 한 번 더 본다
        let mut applied = true;
        app.state::<AppState>().store.lock().update_translation(&doc.id, |t| {
            if live {
                applied = translate::merge_live(t, lang, b.items);
            } else {
                translate::merge(t, lang, b.items);
            }
        })?;
        if applied {
            let _ = app.emit("bd://translation", serde_json::json!({ "id": doc.id, "lang": lang, "items": items }));
        } else {
            log::info!("실시간 번역 답을 버림 — 그 사이 번역 단추가 다른 언어로 번역함");
        }
    }
    Ok(b.sent)
}

/// 번역할 기록 — 녹음 중인 기록이면 앱이 들고 있는 것, 아니면 저장된 것
fn doc_for_translation(app: &AppHandle, id: &str) -> anyhow::Result<SessionDoc> {
    let st = app.state::<AppState>();
    let cur = st.store.lock().current.as_ref().filter(|d| d.id == id).cloned();
    match cur {
        Some(d) => Ok(d),
        None => st.store.lock().load(id),
    }
}

/// 보냈는데도 번역이 안 된(번호가 빠진) 문장을 센다 — 두 번 빠진 문장은 건너뛴다 (모델이 늘 빠뜨리는 문장에 갇히지 않게)
fn count_misses(app: &AppHandle, id: &str, lang: &str, sent: Vec<u64>, misses: &mut std::collections::HashMap<u64, u32>) {
    let still: BTreeSet<u64> = doc_for_translation(app, id).map(|d| translate::pending(&d, lang).into_iter().collect()).unwrap_or_default();
    for s in sent.into_iter().filter(|s| still.contains(s)) {
        *misses.entry(s).or_default() += 1;
    }
}

/// 녹음 중 실시간 번역 — 새로 확정된 문장이 3개 넘게 쌓이거나 첫 문장이 6초 넘게 기다렸으면 한 묶음을 보낸다.
/// 녹음이 끝나면 남은 문장까지 번역하고 끝낸다. 설정에서 끄면 다음 차례에 멈춘다. 두 번 빠진 문장은 건너뛴다
/// (회의 화면의 번역 단추로 다시 할 수 있다). 오류는 화면에 한 번만 알리고 30초 뒤 다시.
fn spawn_live_translation(app: AppHandle, id: String) {
    let _ = std::thread::Builder::new().name("bd-translate".into()).spawn(move || {
        let st = app.state::<AppState>();
        // 녹음이 끝난 뒤 남은 문장을 번역하는 동안 업데이트 설치(앱 종료)가 기다리게 — 번역 단추와 같이 바쁨으로 센다
        st.busy_ops.fetch_add(1, Ordering::SeqCst);
        let _busy = BusyGuard(&st.busy_ops);
        let mut misses = std::collections::HashMap::new();
        let mut first_seen: Option<Instant> = None;
        let mut resume_at: Option<Instant> = None;
        let mut warned = false;
        // 녹음이 끝난 뒤 요청 한도로 기다린 횟수
        let mut ended_waits = 0;
        log::info!("실시간 번역 시작: {id}");
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let (cfg, tcfg) = {
                let s = st.settings.lock();
                (s.summary.clone(), s.translate.clone())
            };
            if !tcfg.live {
                break;
            }
            let ended = !st.store.lock().current.as_ref().is_some_and(|d| d.id == id);
            if resume_at.is_some_and(|t| Instant::now() < t) {
                continue;
            }
            // 번역 단추가 이 회의를 번역하는 동안은 쉰다
            if st.tr_busy.lock().contains(&id) {
                continue;
            }
            let Ok(doc) = doc_for_translation(&app, &id) else {
                break;
            };
            // 이 회의에 이미 번역이 있으면 그 언어로 (번역 단추로 다른 언어를 고른 회의를 설정 언어로 되돌리지 않게)
            let lang =
                doc.translation.as_ref().filter(|t| !t.items.is_empty()).map_or_else(|| tcfg.lang.clone(), |t| t.lang.clone());
            let todo: Vec<u64> =
                translate::pending(&doc, &lang).into_iter().filter(|u| misses.get(u).copied().unwrap_or(0) < 2).collect();
            if todo.is_empty() {
                first_seen = None;
                if ended {
                    break;
                }
                continue;
            }
            let since = *first_seen.get_or_insert_with(Instant::now);
            if !ended && todo.len() < 3 && since.elapsed() < std::time::Duration::from_secs(6) {
                continue;
            }
            let step = summary::chat_caller(&cfg)
                .and_then(|(provider, mut chat)| translate_step(&app, &mut chat, &provider, &lang, &doc, &todo, true));
            match step {
                Ok(sent) => {
                    first_seen = None;
                    count_misses(&app, &id, &lang, sent, &mut misses);
                }
                Err(e) => {
                    // 요청 한도(429): 짧게 기다리라면 기다렸다 다시. 다만 녹음이 끝난 뒤에는 몇 번까지만 —
                    // 하루 한도처럼 오래 기다리라거나 계속 막히면 알리고 끝낸다 (업데이트를 끝없이 붙잡지 않게)
                    let wait = summary::wait_for(&e);
                    if let Some(w) = wait.filter(|w| *w <= summary::MAX_WAIT_SECS) {
                        if ended {
                            ended_waits += 1;
                        }
                        if ended_waits <= 3 {
                            resume_at = Some(Instant::now() + std::time::Duration::from_secs(w));
                            continue;
                        }
                    }
                    log::warn!("실시간 번역 실패: {e:#}");
                    if !warned || ended {
                        warned = true;
                        let msg = if ended { format!("{e:#}\n남은 문장은 번역 단추로 이어서 할 수 있어요") } else { format!("{e:#}") };
                        let _ = app.emit("bd://translate-error", serde_json::json!({ "id": id, "message": msg, "ended": ended }));
                    }
                    if ended {
                        break;
                    }
                    // 오래 기다리라는 한도면 그만큼(10분까지), 그 밖의 오류는 30초 뒤 다시
                    let secs = wait.map_or(30, |w| w.min(600));
                    resume_at = Some(Instant::now() + std::time::Duration::from_secs(secs));
                }
            }
        }
        log::info!("실시간 번역 끝: {id}");
    });
}

/// 번역 단추의 결과: 번역 · 못 한 문장 수
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TranslateResult {
    translation: Option<translate::Translation>,
    missed: usize,
}

/// 번역 단추: 번역 안 된 문장을 모두 번역한다 (언어가 다르면 처음부터).
/// 진행은 `bd://translate-progress` { id, token, done, total, waitSecs }, 번역은 받는 대로 `bd://translation`.
/// 멈추기는 요약과 같은 `cancel_summary(token)`
#[tauri::command]
async fn translate_session(app: AppHandle, id: String, lang: String, token: u64) -> CmdResult<TranslateResult> {
    let st = app.state::<AppState>();
    // 회의마다 번역 단추 작업은 하나 — 그동안 그 회의의 실시간 번역은 쉰다
    if !st.tr_busy.lock().insert(id.clone()) {
        return Err("이 회의를 이미 번역하고 있어요".into());
    }
    struct TrGuard<'a>(&'a Mutex<BTreeSet<String>>, String);
    impl Drop for TrGuard<'_> {
        fn drop(&mut self) {
            self.0.lock().remove(&self.1);
        }
    }
    let _owner = TrGuard(&st.tr_busy, id.clone());
    st.busy_ops.fetch_add(1, Ordering::SeqCst);
    let _busy = BusyGuard(&st.busy_ops);
    let cfg = st.settings.lock().summary.clone();
    let cancel = st.summary_cancel.lock().entry(token).or_default().clone();
    let (app2, id2) = (app.clone(), id.clone());
    let res = tauri::async_runtime::spawn_blocking(move || -> anyhow::Result<TranslateResult> {
        let (provider, mut chat) = summary::chat_caller(&cfg)?;
        let progress = |done: usize, total: usize, wait: Option<u64>| {
            let _ = app2.emit(
                "bd://translate-progress",
                serde_json::json!({ "id": id2, "token": token, "done": done, "total": total, "waitSecs": wait }),
            );
        };
        let total = translate::pending(&doc_for_translation(&app2, &id2)?, &lang).len();
        let mut misses = std::collections::HashMap::new();
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(anyhow::anyhow!(translate::CANCELLED_MSG));
            }
            let doc = doc_for_translation(&app2, &id2)?;
            let left = translate::pending(&doc, &lang);
            let todo: Vec<u64> = left.iter().copied().filter(|u| misses.get(u).copied().unwrap_or(0) < 2).collect();
            progress(total.saturating_sub(left.len()), total, None);
            if todo.is_empty() {
                let translation = doc.translation.filter(|t| t.lang == lang);
                return Ok(TranslateResult { translation, missed: left.len() });
            }
            match translate_step(&app2, &mut chat, &provider, &lang, &doc, &todo, false) {
                Ok(sent) => count_misses(&app2, &id2, &lang, sent, &mut misses),
                Err(e) => {
                    let Some(w) = summary::wait_for(&e).filter(|w| *w <= summary::MAX_WAIT_SECS) else {
                        return Err(e);
                    };
                    progress(total.saturating_sub(left.len()), total, Some(w));
                    let until = Instant::now() + std::time::Duration::from_secs(w);
                    while Instant::now() < until {
                        if cancel.load(Ordering::SeqCst) {
                            return Err(anyhow::anyhow!(translate::CANCELLED_MSG));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }
        }
    })
    .await;
    st.summary_cancel.lock().remove(&token);
    res.map_err(err)?.map_err(err)
}

/// 번역 지우기 (원문은 그대로)
#[tauri::command]
async fn clear_translation(app: AppHandle, id: String) -> CmdResult<()> {
    off_main(move || {
        app.state::<AppState>().store.lock().update_translation(&id, |t| *t = None).map_err(err)?;
        Ok(())
    })
    .await
}

/// 회의에 묻는다 → 받은 답 (기록의 묻고 답하기 목록 끝에 저장한 것).
/// 답은 받는 대로 `bd://ask-progress` 로 알린다: { id, token, delta | restart | waitSecs }.
/// `recent_mins` 가 있으면 지금까지 따라잡기 — 받아 적은 마지막 말에서 그 분 앞까지만 정리 (`question` 은 기록에 남길 이름)
#[tauri::command]
async fn ask_meeting(
    app: AppHandle,
    id: String,
    question: String,
    token: u64,
    recent_mins: Option<u32>,
) -> CmdResult<session::QaItem> {
    let st = app.state::<AppState>();
    st.busy_ops.fetch_add(1, Ordering::SeqCst);
    let _busy = BusyGuard(&st.busy_ops);
    let (cfg, doc) = {
        let cfg = st.settings.lock().summary.clone();
        let doc = st.store.lock().load(&id).map_err(err)?;
        (cfg, doc)
    };
    // 개인정보 가리기: 구간은 원문으로 고르고 AI 로 나가는 글만 가린다 (기록 원문 · 남기는 질문은 그대로)
    let mask = st.settings.lock().privacy.mask_ai;
    let cancel = st.summary_cancel.lock().entry(token).or_default().clone();
    let (app2, id2, cancel2) = (app.clone(), id.clone(), cancel.clone());
    let mut on_event = move |e: ask::Event| {
        if cancel2.load(Ordering::SeqCst) {
            return;
        }
        let payload = match e {
            ask::Event::Text(t) => serde_json::json!({ "id": id2, "token": token, "delta": t }),
            ask::Event::Restart => serde_json::json!({ "id": id2, "token": token, "restart": true }),
            ask::Event::Wait(w) => serde_json::json!({ "id": id2, "token": token, "waitSecs": w }),
        };
        let _ = app2.emit("bd://ask-progress", payload);
    };
    let app3 = app.clone();
    let on_usage = move |provider: &str, r: &summary::Reply| {
        app3.state::<usage_book::UsageBook>().summary(provider, &r.model, r.in_tokens, r.out_tokens, r.cache_write, r.cache_read);
    };
    // 요약처럼 따로 돌리고, 멈추면 보내 둔 요청을 기다리지 않고 화면을 먼저 돌려보낸다
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let cancel = cancel.clone();
        let question = question.clone();
        let _ = std::thread::Builder::new().name("bd-ask".into()).spawn(move || {
            let _ = tx.send(ask::ask(&cfg, &doc, &question, recent_mins, mask, &mut on_event, &on_usage, &cancel));
        });
    }
    let res = tauri::async_runtime::spawn_blocking(move || loop {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(r) => return r,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if cancel.load(Ordering::SeqCst) => {
                return Err(anyhow::Error::new(summary::AskCancelled))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("답을 받다가 문제가 생겼어요"))
            }
        }
    })
    .await;
    st.summary_cancel.lock().remove(&token);
    app.state::<usage_book::UsageBook>().save();
    let a = res.map_err(err)?.map_err(err)?;
    let item = session::QaItem {
        q: question.trim().to_string(),
        a: a.text,
        at: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        model: a.model,
        partial: a.partial,
    };
    let (it, app4) = (item.clone(), app.clone());
    off_main(move || app4.state::<AppState>().store.lock().update_qa(&id, |qa| qa.push(it)).map_err(err)).await?;
    Ok(item)
}

/// 모든 회의에 묻기 → 답 (기록에 남기지 않는다). `days` 가 있으면 최근 그 날 수 안의 회의만, `history` 는 이 대화의
/// 앞선 질문 · 답. 답은 받는 대로 `bd://ask-all-progress` 로 알린다: { token, delta | restart | waitSecs }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AskAllDto {
    a: String,
    model: String,
    sources: Vec<ask_all::Source>,
    considered: usize,
    recent: bool,
    at: String,
}

#[tauri::command]
async fn ask_all(
    app: AppHandle,
    question: String,
    token: u64,
    days: Option<u32>,
    history: Vec<ask_all::Past>,
) -> CmdResult<AskAllDto> {
    let st = app.state::<AppState>();
    st.busy_ops.fetch_add(1, Ordering::SeqCst);
    let _busy = BusyGuard(&st.busy_ops);
    let (cfg, dir, current) = {
        let store = st.store.lock();
        (st.settings.lock().summary.clone(), store.dir.clone(), store.current_doc())
    };
    let mask = st.settings.lock().privacy.mask_ai;
    let cancel = st.summary_cancel.lock().entry(token).or_default().clone();
    let (app2, cancel2) = (app.clone(), cancel.clone());
    let mut on_event = move |e: ask::Event| {
        if cancel2.load(Ordering::SeqCst) {
            return;
        }
        let payload = match e {
            ask::Event::Text(t) => serde_json::json!({ "token": token, "delta": t }),
            ask::Event::Restart => serde_json::json!({ "token": token, "restart": true }),
            ask::Event::Wait(w) => serde_json::json!({ "token": token, "waitSecs": w }),
        };
        let _ = app2.emit("bd://ask-all-progress", payload);
    };
    let app3 = app.clone();
    let on_usage = move |provider: &str, r: &summary::Reply| {
        app3.state::<usage_book::UsageBook>().summary(provider, &r.model, r.in_tokens, r.out_tokens, r.cache_write, r.cache_read);
    };
    // 기록을 모두 읽는 일이라 기록 잠금 밖에서 (녹음 중 저장이 기다리지 않게). 녹음 중인 기록은 앱이 들고 있는 것으로
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let cancel = cancel.clone();
        let question = question.clone();
        let _ = std::thread::Builder::new().name("bd-ask-all".into()).spawn(move || {
            let mut docs = SessionStore::new(dir).all_docs();
            if let Some(cur) = current {
                docs.retain(|d| d.id != cur.id);
                docs.insert(0, cur);
            }
            if let Some(n) = days {
                let from = (chrono::Local::now().date_naive() - chrono::Duration::days(n as i64)).format("%Y-%m-%d").to_string();
                docs.retain(|d| d.started_at.get(..10).is_some_and(|day| day >= from.as_str()));
            }
            // 개인정보 가리기: 회의는 원문으로 고르고 AI 로 나가는 글만 가린다 — 회의 기록 · 직접 쓴 질문 · 앞선 대화
            // (가리기를 켜기 전 것도). 화면에 돌려주는 근거 회의 제목은 원래 제목
            let _ = tx.send(ask_all::ask_all(&cfg, &docs, &question, &history, mask, &mut on_event, &on_usage, &cancel));
        });
    }
    let res = tauri::async_runtime::spawn_blocking(move || loop {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(r) => return r,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if cancel.load(Ordering::SeqCst) => {
                return Err(anyhow::Error::new(summary::AskCancelled))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("답을 받다가 문제가 생겼어요"))
            }
        }
    })
    .await;
    st.summary_cancel.lock().remove(&token);
    app.state::<usage_book::UsageBook>().save();
    let a = res.map_err(err)?.map_err(err)?;
    Ok(AskAllDto {
        a: a.text,
        model: a.model,
        sources: a.sources,
        considered: a.considered,
        recent: a.recent,
        at: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
    })
}

#[tauri::command]
fn cancel_ask(app: AppHandle, token: u64) {
    log::info!("묻기 멈춤");
    cancel_token(&app, token);
}

/// 묻고 답한 것 지우기 — 화면이 보여 준 그 질문 · 답(번호가 아니라 내용으로 찾는다: 지우기를 두 번 눌러도
/// 다음 답이 지워지지 않게). `item` 이 없으면 모두
#[tauri::command]
async fn delete_qa(app: AppHandle, id: String, item: Option<session::QaItem>) -> CmdResult<()> {
    off_main(move || {
        app.state::<AppState>()
            .store
            .lock()
            .update_qa(&id, |qa| session::remove_qa(qa, item.as_ref()))
            .map_err(err)
    })
    .await
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

/// 업데이트 팝업: 지금 버전 이후 바뀐 내용 (버전별, 최신순)
#[tauri::command]
async fn update_notes(app: AppHandle) -> Vec<update::VersionNotes> {
    tauri::async_runtime::spawn_blocking(move || update::release_notes(&app))
        .await
        .unwrap_or_default()
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
        // 전체 화면 · 최대화된 상태였다면 먼저 원래 크기로 돌려놓고 그 크기를 기억한다 (돌아올 때 꽉 찬 창이 되지 않게)
        if w.is_fullscreen().unwrap_or(false) {
            let _ = w.set_fullscreen(false);
        }
        if w.is_maximized().unwrap_or(false) {
            let _ = w.unmaximize();
        }
        if !s.mini_mode {
            // 크기는 배율과 상관없는 논리 픽셀로 기억한다 (원격 100% ↔ 노트북 125% 를 오가도 같은 크기)
            if let (Ok(p), Ok(sz), Ok(sf)) = (w.outer_position(), w.inner_size(), w.scale_factor()) {
                let lw = (sz.width as f64 / sf).round() as i32;
                let lh = (sz.height as f64 / sf).round() as i32;
                s.full_bounds = Some([p.x, p.y, lw, lh]);
            }
        }
        let _ = w.set_min_size(None::<tauri::Size>);
        let _ = w.set_resizable(false);
        let _ = w.set_size(LogicalSize::new(MINI_SIZE.0, MINI_SIZE.1));
        let _ = w.set_always_on_top(true);
        // 시계 위에 붙여 두었으면 지금 화면(배율·해상도) 기준으로 다시 붙이고,
        // 다른 곳에 두었으면 그 자리로 — 단 화면 밖으로 나간 부분은 안으로 들인다
        match s.mini_pos.filter(|p| !s.mini_docked && on_screen(app, p[0], p[1])) {
            Some([x, y]) => {
                let _ = w.set_position(PhysicalPosition::new(x, y));
                snap::keep_inside(&w);
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
                let _ = w.set_size(LogicalSize::new(bw.max(FULL_MIN.0 as i32) as f64, bh.max(FULL_MIN.1 as i32) as f64));
                let _ = w.set_position(PhysicalPosition::new(x, y));
                // 다른 배율·해상도에서 저장한 크기라 화면을 넘을 수 있다
                snap::keep_inside(&w);
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
    let item = st.tray_mini.lock().clone();
    if let Some(item) = item {
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
    let docked = snap::docked_bottom_right(&w);
    if let Ok(p) = w.outer_position() {
        let mut s = st.settings.lock();
        s.mini_pos = Some([p.x, p.y]);
        s.mini_docked = docked;
        let _ = s.save(&st.paths.settings);
    }
}

/// 배율(DPI) · 해상도 · 작업 표시줄이 바뀌었을 때 (원격 접속 ↔ 노트북 화면, 모니터 연결 등)
/// 창 크기가 새 배율로 바뀐 뒤 자리를 다시 잡는다: 시계 위에 붙은 미니 창은 다시 붙이고, 나머지는 화면 안으로.
/// 작업 표시줄 단추가 새로 생김 — 탐색기가 다시 켜지면 녹음 중 빨간 배지 · 업데이트 초록 배지가 지워진 채 남았다.
/// 창 메시지 처리 중에 불리므로 다른 스레드에서, 단추가 다 만들어질 틈을 두고 다시 얹는다.
fn on_taskbar_button(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        status::reapply_taskbar(&app);
    });
}

fn on_display_change(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let h = app.clone();
        let _ = app.run_on_main_thread(move || {
            let Some(w) = h.get_webview_window("main") else {
                return;
            };
            if w.is_maximized().unwrap_or(false) || w.is_minimized().unwrap_or(false) {
                return;
            }
            let (mini, docked) = {
                let s = h.state::<AppState>().settings.lock().clone();
                (s.mini_mode, s.mini_docked)
            };
            if mini && docked {
                snap::dock_bottom_right(&w);
            } else {
                snap::keep_inside(&w);
            }
        });
    });
}

/// 화면 스크립트가 시작을 끝냈다는 신호 (스크립트 오류로 화면이 비면 오지 않는다)
/// 설정 › 사용량: 이 앱으로 쓴 양(오늘 · 이번 달) + 공급자가 알려 준 남은 한도
#[tauri::command]
fn get_usage(app: AppHandle) -> usage_book::UsageReport {
    let book = app.state::<usage_book::UsageBook>();
    book.save();
    // 모델을 기록하기 전(v0.4.6 까지) 사용분은 지금 설정한 모델 단가로 셈한다
    let s = app.state::<AppState>().settings.lock().clone();
    let legacy = |provider: &str, u: usage_book::Use| -> Option<String> {
        let m = match (u, provider) {
            (usage_book::Use::Stt, "groq") => &s.groq_model,
            (usage_book::Use::Stt, "openai") => &s.openai_model,
            (usage_book::Use::Summary, "groq") => &s.summary.groq_model,
            (usage_book::Use::Summary, "openai") => &s.summary.openai_model,
            (usage_book::Use::Summary, "gemini") => &s.summary.gemini_model,
            (usage_book::Use::Summary, "anthropic") => &s.summary.anthropic_model,
            _ => return None,
        };
        match m.trim() {
            // 요약 설정이 비어 있으면 Claude 는 기본 모델로 보낸다 (summary::anthropic)
            "" if provider == "anthropic" => Some("claude-opus-5-5".into()),
            "" => None,
            m => Some(m.to_string()),
        }
    };
    book.report(&legacy)
}

#[tauri::command]
fn ui_ready(app: AppHandle) {
    app.state::<AppState>().ui_ready.store(true, Ordering::SeqCst);
    log::info!("화면 준비됨");
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
        "logs" => logging::log_dir(),
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
    } else if st.quit_after_stop.swap(true, Ordering::SeqCst) {
        // 두 번째 종료 요청: 남은 인식을 기다리지 않고 지금까지를 저장하고 끝낸다
        let _ = st.store.lock().flush();
        app.exit(0);
    } else {
        // 남은 발화를 마저 인식하고 저장한 뒤 종료 (녹음 폴더 파일이면 다음에 켤 때 처음부터 다시)
        watch::interrupt(app);
        stop_recording_inner(app);
    }
}

// ───────────────────────── 창 · 트레이 ─────────────────────────

/// 창만 앞으로 (메인 스레드에서 부르면 메시지를 거치지 않고 그 자리에서 보인다)
fn raise_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

pub(crate) fn show_main(app: &AppHandle) {
    raise_main(app);
    // 숨겨져 있던 창은 작업 표시줄 단추가 새로 생기면서 배지 · 진행 막대가 비어 있다
    status::reapply_taskbar(app);
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
        if let Err(e) = start_session(&app, Some(&file), None) {
            show_main(&app);
            let _ = app.emit("bd://toast", e);
        }
    });
}

fn main() {
    let smoke = smoke::enabled();
    // 이미 떠 있으면 tauri 를 준비하기 전에 명령만 넘기고 끝낸다 — 바로 가기를 눌렀을 때 창이 빨리 뜨게 (forward.rs)
    // main 에서 돌아가면 C/C++ 라이브러리(ONNX Runtime · whisper.cpp)의 정적 소멸자가 돌며 실행 파일을 더 읽으므로 곧장 끝낸다.
    // 자동 점검은 사용 중인 앱과 따로 돈다
    #[cfg(windows)]
    if !smoke && forward::to_running_app() {
        std::process::exit(0);
    }
    logging::init();

    let mut builder = tauri::Builder::default();
    // 자동 점검은 사용 중인 앱과 따로 돈다 (명령이 사용 중인 앱으로 넘어가지 않게)
    if !smoke {
        builder = builder
        // 두 번째 실행은 이미 떠 있는 앱으로 전달된다: `badasugi --toggle | --start | --stop | --pause`
        // (Stream Deck · 배치 파일 · 바로 가기 등에서 녹음 제어)
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // 그냥 다시 실행(바로 가기 · 시작 메뉴 · 파일 열기)이면 창부터 띄운다 — 이 콜백은 메인 스레드라 작업 스레드를
            // 거치지 않고 곧장 보인다. 작업 표시줄 배지 등 나머지는 아래 handle_forwarded 가 마저 한다 (다시 띄워도 그대로)
            let ready = app.try_state::<AppState>().is_some_and(|st| st.ui_ready.load(Ordering::SeqCst));
            if ready && argv.iter().skip(1).all(|a| !a.starts_with("--")) {
                raise_main(app);
            }
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
        }));
    }
    builder
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    // 말로 입력: 누른 채 말하고 떼면 입력 (톡 누르면 말이 끝날 때까지)
                    if is_dictation_shortcut(app, shortcut) {
                        match event.state() {
                            ShortcutState::Pressed => dictate::press(app),
                            ShortcutState::Released => dictate::release(app),
                        }
                        return;
                    }
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    // 중요 표시 ⭐: 메모는 화면만 고치므로(지금 쓰는 글과 겹치지 않게) 화면에 알려 표시하게 한다
                    if is_star_shortcut(app, shortcut) {
                        let _ = app.emit("bd://star", ());
                        return;
                    }
                    let app = app.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = toggle_recording_inner(&app) {
                            show_main(&app);
                            let _ = app.emit("bd://toast", e);
                        }
                    });
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
        // 창이 앱 화면 밖(외부 페이지)으로 이동하지 못하게 — 녹취록·기록 파일에 심은 HTML 이 창을 가로채는 것 방지
        .plugin(
            tauri::plugin::Builder::<Wry>::new("nav-guard")
                .on_navigation(|_webview, url| url.scheme() == "tauri" || url.host_str() == Some("tauri.localhost"))
                .build(),
        )
        // 창 크기·위치를 기억 (보이기 여부는 트레이 로직이 정한다)
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE | tauri_plugin_window_state::StateFlags::POSITION,
                )
                .build(),
        )
        .setup(move |app| {
            let handle = app.handle().clone();
            let path = app.path();
            let models_dir = path.app_local_data_dir()?.join("models");
            let (config_dir, sessions_dir) = match smoke::home() {
                // 자동 점검: 설정 · 기록은 임시 폴더에 (모델만 설치된 것을 읽는다)
                Some(home) => (home.join("config"), home.join("sessions")),
                None => (
                    path.app_config_dir()?,
                    path.document_dir()
                        .map(|d| d.join("받아쓰기"))
                        .unwrap_or_else(|_| path.app_data_dir().unwrap_or_default().join("sessions")),
                ),
            };
            let settings_path = config_dir.join("settings.json");
            let config_dir_for_usage = config_dir.clone();
            let mut settings = Settings::load(&settings_path);
            badasugi_core::asr::gpu::set_enabled(settings.gpu);
            if smoke {
                settings.save_audio = true;
                settings.mini_mode = false;
            }
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
                summary_cancel: Mutex::new(BTreeMap::new()),
                tr_busy: Mutex::new(BTreeSet::new()),
                file_mode: AtomicBool::new(false),
                file_pct: AtomicUsize::new(0),
                save_warned: AtomicBool::new(false),
                api_provider: Mutex::new(None),
                ui_ready: AtomicBool::new(false),
                live_pos_ms: AtomicU64::new(0),
                echo: Mutex::new(Vec::new()),
                updated_from: Mutex::new(updated_from),
                hotkey_error: Mutex::new(None),
                tray_toggle: Mutex::new(None),
                tray_pause: Mutex::new(None),
                tray_mini: Mutex::new(None),
                rec_started: Mutex::new(None),
                rec_source: Mutex::new(None),
                quit_after_stop: AtomicBool::new(false),
                sleep_paused: AtomicBool::new(false),
                sleep_notified: AtomicBool::new(false),
                auto_named: Mutex::new(BTreeMap::new()),
                speaker_hints: Mutex::new(None),
            });
            app.manage(Updates::default());
            app.manage(meeting::Meet::new());
            app.manage(calendar::Calendar::new());
            app.manage(VoiceBook::load(config_dir_for_usage.join("voices.json")));
            app.manage(usage_book::UsageBook::load(config_dir_for_usage.join("usage.json")));
            app.manage(status::Status::new(tray_base));
            app.manage(watch::Watch::load(config_dir_for_usage.join("watch.json")));
            app.manage(dictate::Dictate::new());
            app.manage(redo::RedoQueue::default());
            log::info!(
                "받아쓰기 {} 시작 — 엔진 {}, 모델 {}, 언어 {}, 입력 {:?}, 화자 구분 {}, 녹음 저장 {}, 미니 창 {}{}",
                app.package_info().version,
                settings.engine,
                settings.local_model,
                settings.language,
                settings.source,
                settings.diarization.enabled,
                settings.save_audio,
                settings.mini_mode,
                if smoke { " (자동 점검)" } else { "" }
            );
            if smoke {
                // 트레이 · 단축키 · 업데이트 없이, 창은 숨긴 채로 점검만 하고 끝낸다
                smoke::spawn(handle.clone());
                return Ok(());
            }

            setup_tray(&handle)?;
            if let Err(e) = register_hotkeys(&handle, &settings.hotkey, &settings.star_hotkey, dictation_key(&settings)) {
                log::warn!("{e}");
                *app.state::<AppState>().hotkey_error.lock() =
                    Some(format!("{e}\n다른 프로그램이 쓰고 있을 수 있어요. 설정 › 일반에서 다른 조합으로 바꿔 주세요."));
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_always_on_top(settings.always_on_top);
                // 끌어 옮길 때 화면 가장자리·작업 표시줄에 자석처럼 붙게
                let h = handle.clone();
                let h2 = handle.clone();
                let h4 = handle.clone();
                snap::install(
                    &w,
                    Box::new(move || on_move_end(&h)),
                    Box::new(move || on_display_change(&h2)),
                    Box::new(move || on_taskbar_button(&h4)),
                );
                // 녹음 중 PC 가 잠들면 일시정지 (덮개 닫고 가방에 넣었다 열었을 때 저절로 이어 녹음되지 않게)
                let h3 = handle.clone();
                power::install(&w, Box::new(move |ev| on_power(&h3, ev)));
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
            meeting::spawn(handle.clone());
            watch::spawn(handle.clone());
            spawn_audio_expiry(handle.clone());
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
            undo_refinement,
            delete_session,
            export_session,
            export_html,
            translate_session,
            clear_translation,
            print_pdf,
            share_info,
            share_messenger,
            open_mail_draft,
            export_todos_ics,
            session_text,
            export_text,
            summarize,
            cancel_summary,
            ask_meeting,
            ask_all,
            cancel_ask,
            summary_templates,
            delete_qa,
            list_llm_models,
            check_update,
            install_update,
            hide_window,
            set_mini,
            set_playback,
            ui_ready,
            smoke::smoke_probe,
            meeting::meet_state,
            meeting::meet_ready,
            meeting::meet_answer,
            watch::watch_status,
            calendar::calendar_status,
            dictate::dictate_state,
            dictate::dictate_ready,
            redo::redo_session,
            redo::cancel_redo,
            redo::undo_redo,
            get_usage,
            list_voices,
            forget_voice,
            forget_all_voices,
            update_notes,
            open_folder,
            open_url,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("받아쓰기 실행 실패");
}

#[cfg(test)]
mod tests {
    use super::is_local_disk;
    use std::path::Path;

    #[test]
    fn only_local_drive_paths_are_replayed() {
        assert!(is_local_disk(Path::new(r"C:\Users\me\Documents\회의.m4a")));
        assert!(is_local_disk(Path::new(r"\\?\D:\rec\a.mp3")));
        // 네트워크·WebDAV 경로는 열기만 해도 Windows 가 그 서버에 로그인을 시도한다
        assert!(!is_local_disk(Path::new(r"\\attacker\share\a.mp3")));
        assert!(!is_local_disk(Path::new(r"\\attacker@SSL\DavWWWRoot\a.mp3")));
        assert!(!is_local_disk(Path::new(r"\\?\UNC\attacker\share\a.mp3")));
        // 드라이브 없는 루트 경로(현재 드라이브 기준)도 막는다
        assert!(!is_local_disk(Path::new(r"\attacker\share\a.mp3")));
        assert!(!is_local_disk(Path::new(r"relative\a.mp3")));
    }
}

#[cfg(test)]
mod echo_tests {
    use super::{is_echo, EchoWindow};

    fn w(from: u64, to: Option<u64>, t: &str) -> EchoWindow {
        EchoWindow {
            from_ms: from,
            to_ms: to,
            texts: vec![t.to_string()],
        }
    }

    #[test]
    fn drops_only_matching_text_inside_the_playback_window() {
        let ws = [w(60_000, Some(64_000), "금요일까지 배포가 가능할 것 같습니다.")];
        // 재생하던 동안 녹음된 같은 말 → 메아리
        assert!(is_echo(&ws, 60_800, 63_900, "금요일까지 배포가 가능할 것 같습니다", 70_000));
        // 같은 시간대의 다른 말 → 남긴다
        assert!(!is_echo(&ws, 61_000, 63_000, "그럼 다음 안건으로 넘어가죠.", 70_000));
        // 재생 전에 실제로 했던 같은 말 → 남긴다
        assert!(!is_echo(&ws, 15_000, 18_000, "금요일까지 배포가 가능할 것 같습니다.", 70_000));
        // 아직 재생 중(끝 모름)이면 지금까지를 구간으로
        let open = [w(60_000, None, "좋습니다. 그럼 시작하죠.")];
        assert!(is_echo(&open, 61_000, 62_500, "좋습니다 그럼 시작하죠", 62_600));
    }
}
