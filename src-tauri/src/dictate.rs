//! 받아쓰기 입력 모드 (어디서나 말로 입력) — 단축키를 누른 채 말하면 지금 커서가 있는 창(메일 · 메신저 · 문서)에
//! 글자가 들어간다. 설정에서 켰을 때만.
//!
//! - **엔진은 그대로**: 회의 받아쓰기와 같은 파이프라인(`Pipeline::start` — 인식 엔진 · VAD · 용어집 · 환각 거르기)을
//!   마이크만, 화자 구분 · 녹음 저장 없이 돌린다. 떼면 남은 말을 마저 받아 적고(`Stopped`) 이어 붙인다.
//! - **누르는 법**: 누른 채 말하고 떼면 입력. 짧게 톡 누르면(0.35초 안에 뗌) 손을 떼고 말해도 되고, 다시 누르거나
//!   말이 끝나고 2초 조용하면 입력한다(8초 동안 말이 없으면 그만). 한 번에 2분까지.
//! - **다듬기**: 군말(음 · 어 · 머뭇거린 그…) · 되풀이 · 말로 한 문장부호는 가벼운 규칙(`tidy`)으로. AI 다듬기
//!   (설정 › 요약의 AI 로 맞춤법 · 띄어쓰기)는 켰을 때만 — 20초 안에 답이 없으면 규칙으로 다듬은 글을 넣는다.
//! - **넣기**: 클립보드 붙여넣기(원래 클립보드는 되돌림) 또는 한 글자씩(`inject.rs`). 넣지 못하면 클립보드에 남겨 둔다.
//! - **마이크 나누기**: 회의 녹음이 마이크를 받는 동안(마이크 · 마이크 + 시스템)은 쉰다 — 같은 마이크라 받아쓰기로
//!   한 말이 회의 기록에도 들어가고, 회의 상대에게도 들리기 때문. 시스템 소리만 받는 녹음 · 파일 받아쓰기 · 녹음 폴더
//!   받아쓰기 중에는 된다(로컬 모델은 같은 모델을 함께 쓴다).
//! - **알림**: 작은 알약 창(`dictate.html`)을 지금 창이 있는 화면 아래 가운데에 포커스를 빼앗지 않고 띄운다 —
//!   듣는 중(소리 막대) · 받아 적는 중 · 입력함 · 문제.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use badasugi_core::asr::EngineConfig;
use badasugi_core::audio::{CaptureConfig, SourceKind};
use badasugi_core::pipeline::{Event, EventSink, Pipeline, PipelineConfig};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::inject;
use crate::{AppState, RecState};

/// 이보다 짧게 누르면 톡 — 손을 떼고 말해도 된다
const TAP: Duration = Duration::from_millis(350);
/// 톡 누른 뒤: 말이 끝나고 이만큼 조용하면 입력
const QUIET_END: Duration = Duration::from_secs(2);
/// 톡 누른 뒤: 이만큼 말이 없으면 그만
const NO_SPEECH: Duration = Duration::from_secs(8);
/// 한 번에 이만큼까지
const MAX_LEN: Duration = Duration::from_secs(120);
/// AI 다듬기를 기다리는 시간
const POLISH_WAIT: Duration = Duration::from_secs(20);
/// 이만큼 쓰지 않으면 알림 창을 없애고 (쉴 때 모델 비우기를 켰으면) 모델도 내린다
const IDLE_UNLOAD: Duration = Duration::from_secs(5 * 60);
const LABEL: &str = "dictate";

/// 알림 창에 보일 것
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum View {
    /// 듣는 중 (`latched` = 톡 눌러 손을 뗀 채 듣는 중)
    Listening { latched: bool },
    /// 남은 말을 받아 적는 중
    Working,
    /// AI 로 다듬는 중
    Polishing,
    /// 넣었다 (`missed` = 받아 적지 못한 발화 수)
    Done { chars: usize, missed: usize },
    /// 넣지 못했다 · 말이 없었다 · 쉬는 중
    Error { message: String },
    Hidden,
}

/// 말로 입력이 듣기부터 넣기까지 도는 동안 업데이트 설치를 미룬다 (`busy_ops` — 세션이 없어지면 내린다)
struct Busy(AppHandle);

impl Busy {
    fn new(app: &AppHandle) -> Busy {
        app.state::<AppState>().busy_ops.fetch_add(1, Ordering::SeqCst);
        Busy(app.clone())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.state::<AppState>().busy_ops.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Session {
    id: u64,
    _busy: Busy,
    pipeline: Option<Pipeline>,
    started: Instant,
    latched: bool,
    stopping: bool,
    /// 발화 id → 받아 적은 글
    texts: BTreeMap<u64, String>,
    spoke: bool,
    quiet_since: Option<Instant>,
    error: Option<String>,
    /// 끝내 받아 적지 못한 발화 id (일부만 받아 적었으면 넣고 나서 알린다). 다시 시도로 받아 적히면 뺀다
    failed: std::collections::BTreeSet<u64>,
    /// 회의 녹음이 마이크를 가져갔다 — 들은 것을 넣지 않고 버린다
    discard: bool,
    /// API 엔진이면 (공급자, 모델) — 사용량
    api: Option<(&'static str, String)>,
}

pub struct Dictate {
    cur: Mutex<Option<Session>>,
    next: AtomicU64,
    view: Mutex<View>,
    /// 알림 창을 바꿀 때마다 1씩 (늦게 온 '닫기'가 새 알림을 닫지 않게)
    view_gen: AtomicU64,
    last_used: Mutex<Option<Instant>>,
    /// 앞서 말한 글을 다듬고 넣는 중 — 끝날 때까지 새로 듣지 않는다 (넣는 순서가 뒤바뀌거나 클립보드를 서로 덮지 않게)
    finishing: AtomicBool,
    /// 자동 점검: 넣지 않고 넣을 글을 남긴다
    pub smoke_out: Mutex<Option<String>>,
}

impl Dictate {
    pub fn new() -> Self {
        Self {
            cur: Mutex::new(None),
            next: AtomicU64::new(0),
            view: Mutex::new(View::Hidden),
            view_gen: AtomicU64::new(0),
            last_used: Mutex::new(None),
            finishing: AtomicBool::new(false),
            smoke_out: Mutex::new(None),
        }
    }

    pub fn view(&self) -> View {
        self.view.lock().clone()
    }
}

// ───────────────────────── 단축키 ─────────────────────────

/// 단축키를 눌렀다 — 듣기 시작 (톡 눌러 듣는 중이면 입력)
pub fn press(app: &AppHandle) {
    let d = app.state::<Dictate>();
    if d.finishing.load(Ordering::SeqCst) {
        show(app, View::Error { message: "앞서 말한 글을 넣는 중이에요 — 잠시 뒤 다시 눌러 주세요".into() });
        hide_later(app, Duration::from_secs(2));
        return;
    }
    let id = {
        let mut cur = d.cur.lock();
        if let Some(s) = cur.as_mut() {
            let stop = s.latched && !s.stopping;
            if stop {
                stop_locked(s);
            }
            drop(cur);
            if stop {
                show(app, View::Working);
            }
            return;
        }
        let id = d.next.fetch_add(1, Ordering::SeqCst) + 1;
        *cur = Some(Session {
            id,
            _busy: Busy::new(app),
            pipeline: None,
            started: Instant::now(),
            latched: false,
            stopping: false,
            texts: BTreeMap::new(),
            spoke: false,
            quiet_since: None,
            error: None,
            failed: Default::default(),
            discard: false,
            api: None,
        });
        id
    };
    *d.last_used.lock() = Some(Instant::now());
    // 마이크를 여는 일은 단축키를 받는 곳 밖에서 (떼는 것을 놓치지 않게)
    let app = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = begin(&app, id) {
            let d = app.state::<Dictate>();
            let mut cur = d.cur.lock();
            if cur.as_ref().is_some_and(|s| s.id == id) {
                *cur = None;
            }
            drop(cur);
            log::info!("말로 입력 못 함: {e}");
            show(&app, View::Error { message: e });
            hide_later(&app, Duration::from_secs(4));
        }
    });
}

/// 단축키를 뗐다 — 길게 눌렀으면 입력, 톡 눌렀으면 계속 듣는다
pub fn release(app: &AppHandle) {
    let d = app.state::<Dictate>();
    let mut cur = d.cur.lock();
    let Some(s) = cur.as_mut().filter(|s| !s.latched && !s.stopping) else {
        return;
    };
    let tap = s.started.elapsed() < TAP;
    if tap {
        s.latched = true;
    } else {
        stop_locked(s);
    }
    // 창은 잠금을 놓고 건드린다 (창 주인 스레드가 이 잠금을 기다리면 서로 멈춘다)
    drop(cur);
    show(app, if tap { View::Listening { latched: true } } else { View::Working });
}

/// 회의 녹음이 마이크를 쓰기 시작한다 — 듣던 말로 입력은 멈추고 들은 말은 버린다
/// (같은 마이크라 받아쓰기로 한 말이 회의 기록에 섞이지 않게 · 반대 방향은 [`mic_busy`])
pub fn stop_for_meeting(app: &AppHandle) {
    let Some(d) = app.try_state::<Dictate>() else {
        return;
    };
    let stopped = {
        let mut cur = d.cur.lock();
        match cur.as_mut() {
            Some(s) => {
                s.discard = true;
                let was = s.stopping;
                if !was {
                    stop_locked(s);
                }
                !was
            }
            None => false,
        }
    };
    if stopped {
        log::info!("회의 녹음이 마이크를 써서 말로 입력을 멈춤");
        show(app, View::Working);
    }
}

/// 회의 녹음이 지금 마이크를 받고 있나
fn mic_busy(app: &AppHandle) -> bool {
    let st = app.state::<AppState>();
    let rec = *st.rec.lock();
    if rec == RecState::Idle || st.file_mode.load(Ordering::SeqCst) || crate::watch::job_running(app) {
        return false;
    }
    // 잠금은 하나씩 (get_state 는 설정을 쥔 채 녹음 소리를 읽는다)
    let recording = *st.rec_source.lock();
    let source = recording.unwrap_or_else(|| st.settings.lock().source);
    source.uses_mic()
}

/// 마이크를 열고 듣기 시작
fn begin(app: &AppHandle, id: u64) -> Result<(), String> {
    if mic_busy(app) {
        return Err("회의를 받아 적는 중이라 말로 입력은 쉬어요 — 내 말이 회의 기록에 섞이지 않게".into());
    }
    let st = app.state::<AppState>();
    let s = st.settings.lock().clone();
    let engine = crate::build_engine(&s, &st.paths)?;
    if let EngineConfig::Local { model_path, .. } = &engine {
        if !model_path.exists() {
            return Err("로컬 음성 인식 모델이 없어요 — 설정 › 녹음에서 내려받아 주세요".into());
        }
    }
    let api = match &engine {
        EngineConfig::Api { base_url, model, .. } => Some((badasugi_core::usage::provider_of(base_url), model.trim().to_string())),
        _ => None,
    };
    let terms = crate::glossary_terms(&s.glossary);
    let mut diarization = s.diarization.clone();
    diarization.enabled = false;
    let cfg = PipelineConfig {
        capture: CaptureConfig { source: SourceKind::Mic, mic_device: s.mic_device.clone() },
        engine,
        diarization,
        speaker_model: None,
        segmenter: Default::default(),
        language: s.language.clone(),
        prompt: crate::glossary_prompt(&terms),
        keywords: terms,
        record_path: None,
        known_voices: Vec::new(),
    };
    let d = app.state::<Dictate>();
    if let Some(sess) = d.cur.lock().as_mut().filter(|x| x.id == id) {
        sess.api = api;
    }
    show(app, View::Listening { latched: false });
    let app2 = app.clone();
    let sink: EventSink = Arc::new(move |ev| on_event(&app2, id, ev));
    let p = Pipeline::start(cfg, sink).map_err(|e| format!("마이크를 열지 못했어요: {e}"))?;
    let mut cur = d.cur.lock();
    match cur.as_mut().filter(|x| x.id == id) {
        // 마이크를 여는 사이에 이미 뗐다 → 그때까지 들은 것으로
        Some(sess) if sess.stopping => p.stop(),
        Some(sess) => {
            sess.pipeline = Some(p);
            // 단축키 아래에서 사람이 이미 뗀 채로 톡이 되었으면 그대로 듣는다
        }
        None => p.stop(),
    }
    Ok(())
}

/// 그만 듣고 남은 말을 받아 적는다 (`cur` 잠금 안에서 — 알림 창은 잠금을 놓은 뒤에 바꾼다)
fn stop_locked(s: &mut Session) {
    s.stopping = true;
    if let Some(p) = s.pipeline.take() {
        p.stop();
    }
}

fn on_event(app: &AppHandle, id: u64, ev: Event) {
    let d = app.state::<Dictate>();
    match ev {
        Event::Level { rms, speaking, .. } => {
            let stopped = {
                let mut cur = d.cur.lock();
                let Some(s) = cur.as_mut().filter(|s| s.id == id) else {
                    return;
                };
                if speaking {
                    s.spoke = true;
                    s.quiet_since = None;
                } else if s.spoke {
                    s.quiet_since.get_or_insert_with(Instant::now);
                }
                let el = s.started.elapsed();
                let quiet = s.quiet_since.is_some_and(|t| t.elapsed() >= QUIET_END);
                let was = s.stopping;
                if !s.stopping && (el >= MAX_LEN || (s.latched && (quiet || (!s.spoke && el >= NO_SPEECH)))) {
                    stop_locked(s);
                }
                if was {
                    return;
                }
                s.stopping
            };
            if stopped {
                show(app, View::Working);
            } else {
                let _ = app.emit_to(LABEL, "bd://dictate-level", rms);
            }
        }
        Event::Transcript { id: uid, text, .. } => {
            if let Some(s) = d.cur.lock().as_mut().filter(|s| s.id == id) {
                s.failed.remove(&uid);
                s.texts.insert(uid, text);
            }
        }
        Event::Error { message } => {
            log::warn!("말로 입력: {message}");
            if let Some(s) = d.cur.lock().as_mut().filter(|s| s.id == id) {
                s.error.get_or_insert(message);
            }
        }
        // 다시 시도할 실패(연결 문제 — 멈춘 뒤 한 번 더 받아 적는다)는 세지 않고, 끝내 못 받아 적은 것만
        Event::Failed { id: uid, message, retry } => {
            if let Some(s) = d.cur.lock().as_mut().filter(|s| s.id == id) {
                if !retry && !s.texts.contains_key(&uid) {
                    s.failed.insert(uid);
                }
                s.error.get_or_insert(format!("받아 적지 못했어요 — {message}"));
            }
        }
        Event::ApiUsage { audio_ms, ok } => {
            let api = d.cur.lock().as_ref().filter(|s| s.id == id).and_then(|s| s.api.clone());
            if let Some((p, model)) = api {
                let book = app.state::<crate::usage_book::UsageBook>();
                if ok {
                    book.stt(p, &model, audio_ms as f64 / 1000.0);
                } else {
                    book.stt_failed(p);
                }
            }
        }
        Event::Stopped => {
            let s = {
                let mut cur = d.cur.lock();
                match cur.as_ref() {
                    Some(s) if s.id == id => cur.take(),
                    _ => None,
                }
            };
            if let Some(s) = s {
                d.finishing.store(true, Ordering::SeqCst);
                let app = app.clone();
                std::thread::spawn(move || {
                    // 다듬기 · 넣기가 어떻게 끝나든(패닉 포함) 다시 들을 수 있게
                    struct Done(AppHandle);
                    impl Drop for Done {
                        fn drop(&mut self) {
                            self.0.state::<Dictate>().finishing.store(false, Ordering::SeqCst);
                        }
                    }
                    let _done = Done(app.clone());
                    finish(&app, s);
                });
            }
        }
        _ => {}
    }
}

/// 받아 적은 말을 다듬어 지금 창에 넣는다
fn finish(app: &AppHandle, s: Session) {
    let raw = s.texts.values().map(|t| t.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ");
    let mut text = tidy(&raw);
    let secs = s.started.elapsed().as_secs_f32();
    *app.state::<Dictate>().last_used.lock() = Some(Instant::now());
    if s.discard {
        log::info!("말로 입력: 회의 녹음이 시작돼 넣지 않고 버림");
        show(app, View::Error { message: "회의 녹음을 시작해 말로 입력을 멈췄어요 — 들은 말은 넣지 않았어요".into() });
        hide_later(app, Duration::from_secs(4));
        return;
    }
    if text.is_empty() {
        let message = s.error.unwrap_or_else(|| "받아 적을 말이 들리지 않았어요".into());
        show(app, View::Error { message });
        hide_later(app, Duration::from_secs(3));
        unload_later(app);
        return;
    }
    let (polish, mode) = {
        let st = app.state::<AppState>();
        let s = st.settings.lock();
        (s.dictation_polish, inject::Mode::from_setting(&s.dictation_insert))
    };
    if polish {
        show(app, View::Polishing);
        match polish_text(app, &text) {
            Ok(t) => text = t,
            Err(e) => log::warn!("말로 입력 AI 다듬기 못 함 — 규칙으로 다듬은 글을 넣음: {e}"),
        }
    }
    let chars = text.chars().count();
    log::info!("말로 입력: {chars}자 ({secs:.1}초){}", if polish { " · AI 다듬기" } else { "" });
    if crate::smoke::enabled() {
        *app.state::<Dictate>().smoke_out.lock() = Some(text);
        show(app, View::Done { chars, missed: s.failed.len() });
        hide_later(app, Duration::from_millis(900));
        return;
    }
    // 알림 창을 먼저 닫지 않는다 — 포커스를 가져가지 않으므로 지금 창에 그대로 들어간다
    match inject::insert(&text, mode) {
        // 일부 발화를 받아 적지 못했으면 넣은 뒤에 알린다 (녹음을 남기지 않으므로 다시 말해야 한다)
        Ok(()) if !s.failed.is_empty() => {
            log::warn!("말로 입력: 발화 {}개를 받아 적지 못한 채 넣음", s.failed.len());
            show(app, View::Done { chars, missed: s.failed.len() });
            hide_later(app, Duration::from_secs(4));
        }
        Ok(()) => {
            show(app, View::Done { chars, missed: 0 });
            hide_later(app, Duration::from_millis(900));
        }
        Err(e) => {
            // 넣지 못한 글은 잃지 않게 클립보드에 남겨 둔다
            log::warn!("말로 입력: 넣지 못함 — {e}");
            let kept = inject::copy(&text).is_ok();
            let message = if kept { "넣지 못해 받아 적은 글을 클립보드에 두었어요 — Ctrl+V 로 붙여 넣으세요".to_string() } else { e };
            show(app, View::Error { message });
            hide_later(app, Duration::from_secs(6));
        }
    }
    unload_later(app);
}

/// AI 로 맞춤법 · 띄어쓰기 · 문장부호를 다듬는다 (설정 › 요약의 AI). 시간이 걸리면 그만둔다
fn polish_text(app: &AppHandle, text: &str) -> anyhow::Result<String> {
    let cfg = app.state::<AppState>().settings.lock().summary.clone();
    let (provider, mut chat) = crate::summary::chat_caller(&cfg)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let input = text.to_string();
    let app2 = app.clone();
    std::thread::spawn(move || {
        let r = chat(crate::prompts::file("dictate.md"), &[crate::summary::Turn::user(&input)], &mut |_| {});
        if let Ok(r) = &r {
            app2.state::<crate::usage_book::UsageBook>().summary(&provider, &r.model, r.in_tokens, r.out_tokens, r.cache_write, r.cache_read);
        }
        let _ = tx.send(r);
    });
    let r = rx.recv_timeout(POLISH_WAIT).map_err(|_| anyhow::anyhow!("{}초 안에 답이 없음", POLISH_WAIT.as_secs()))??;
    polished(text, &r.text).ok_or_else(|| anyhow::anyhow!("다듬은 글이 원래 글과 너무 달라 쓰지 않음"))
}

/// AI 가 다듬은 글 — 따옴표 · 코드 블록을 벗기고, 원래 글과 길이가 너무 다르면(설명을 붙임 · 지어냄) 쓰지 않는다
pub fn polished(original: &str, reply: &str) -> Option<String> {
    let t = reply.trim().trim_start_matches("```").trim_end_matches("```").trim();
    let t = t
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| t.strip_prefix('“').and_then(|x| x.strip_suffix('”')))
        .unwrap_or(t)
        .trim();
    let (a, b) = (original.chars().count(), t.chars().count());
    (b > 0 && b * 2 >= a && b <= a * 3 / 2 + 10).then(|| t.to_string())
}

// ───────────────────────── 알림 창 ─────────────────────────

fn show(app: &AppHandle, v: View) {
    let d = app.state::<Dictate>();
    *d.view.lock() = v.clone();
    d.view_gen.fetch_add(1, Ordering::SeqCst);
    match (v == View::Hidden, app.get_webview_window(LABEL)) {
        (true, Some(w)) => {
            let _ = w.hide();
        }
        (true, None) => {}
        (false, Some(w)) => {
            let _ = app.emit_to(LABEL, "bd://dictate", &v);
            place(&w);
        }
        (false, None) => {
            // 처음 한 번 만든다 — 화면이 그린 뒤 dictate_ready 에서 보인다
            let built = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("dictate.html".into()))
                .title("받아쓰기 — 말로 입력")
                .inner_size(300.0, 52.0)
                .resizable(false)
                .maximizable(false)
                .minimizable(false)
                .decorations(false)
                .shadow(true)
                .always_on_top(true)
                .skip_taskbar(true)
                .focused(false)
                .visible(false)
                .build();
            match built {
                Ok(w) => no_activate(&w),
                Err(e) => log::warn!("말로 입력 알림 창을 만들지 못함: {e}"),
            }
        }
    }
}

/// 잠시 뒤 닫는다 — 그 사이 다른 알림이 떴으면 그대로 둔다
fn hide_later(app: &AppHandle, after: Duration) {
    let gen = app.state::<Dictate>().view_gen.load(Ordering::SeqCst);
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(after);
        if app.state::<Dictate>().view_gen.load(Ordering::SeqCst) == gen {
            show(&app, View::Hidden);
        }
    });
}

/// 한동안 쓰지 않으면 알림 창을 없애고, 쉴 때 모델 비우기를 켰으면 모델도 내린다 (녹음 중이 아닐 때)
fn unload_later(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(IDLE_UNLOAD + Duration::from_secs(1));
        let d = app.state::<Dictate>();
        let idle = d.cur.lock().is_none() && d.last_used.lock().is_some_and(|t| t.elapsed() >= IDLE_UNLOAD);
        if !idle {
            return;
        }
        if let Some(w) = app.get_webview_window(LABEL) {
            let _ = w.destroy();
        }
        let st = app.state::<AppState>();
        let unload = st.settings.lock().unload_when_idle;
        if unload && *st.rec.lock() == RecState::Idle {
            badasugi_core::asr::whisper::unload();
        }
    });
}

/// 눌러도 이 창이 앞으로 나와 포커스를 가져가지 않게
#[cfg(windows)]
fn no_activate(w: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE};
    if let Ok(h) = w.hwnd() {
        unsafe {
            let hwnd = h.0 as windows_sys::Win32::Foundation::HWND;
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
        }
    }
}

#[cfg(not(windows))]
fn no_activate(_: &tauri::WebviewWindow) {}

/// 지금 창(입력할 곳)이 있는 화면의 아래 가운데 — 포커스를 빼앗지 않고
#[cfg(windows)]
fn place(w: &tauri::WebviewWindow) {
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowRect, SetWindowPos, ShowWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, SW_SHOWNOACTIVATE,
    };
    let Ok(h) = w.hwnd() else {
        return;
    };
    let hwnd = h.0 as HWND;
    unsafe {
        let fg = GetForegroundWindow();
        let mon = MonitorFromWindow(if fg.is_null() { hwnd } else { fg }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let mut wr: RECT = std::mem::zeroed();
        if GetMonitorInfoW(mon, &mut mi) == 0 || GetWindowRect(hwnd, &mut wr) == 0 {
            let _ = w.show();
            return;
        }
        let r = mi.rcWork;
        let (ww, wh) = (wr.right - wr.left, wr.bottom - wr.top);
        let gap = 28 * GetDpiForWindow(hwnd).max(96) as i32 / 96;
        let x = r.left + (r.right - r.left - ww) / 2;
        let y = r.bottom - wh - gap;
        SetWindowPos(hwnd, HWND_TOPMOST, x, y, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
}

#[cfg(not(windows))]
fn place(w: &tauri::WebviewWindow) {
    let _ = w.show();
}

#[tauri::command]
pub fn dictate_state(app: AppHandle) -> View {
    app.state::<Dictate>().view()
}

/// 알림 창이 처음 그렸다 → 보인다 (그 사이 닫혔으면 그대로)
#[tauri::command]
pub fn dictate_ready(app: AppHandle) {
    let v = app.state::<Dictate>().view();
    if let Some(w) = app.get_webview_window(LABEL) {
        if v == View::Hidden {
            let _ = w.hide();
        } else {
            let _ = app.emit_to(LABEL, "bd://dictate", &v);
            place(&w);
        }
    }
}

// ───────────────────────── 글 다듬기 (가벼운 규칙) ─────────────────────────

/// 늘 빼는 군말 (낱말 하나로 떨어져 있을 때만)
const FILLERS: &[&str] = &["음", "음음", "으음", "으", "어", "어어", "흠", "에", "에에", "아아", "엄", "업"];
/// 머뭇거림 표시(… · , )가 붙었을 때만 빼는 군말 — 그냥 쓰이면 뜻이 있는 말 ("그 사람" · "저 사람" · "아 그렇구나")
const HESITANT: &[&str] = &["아", "그", "저", "뭐", "막", "이제", "그러니까", "그니까", "약간"];
/// 말로 한 문장부호
const SPOKEN_MARKS: &[(&str, &str)] = &[
    ("마침표", "."),
    ("쉼표", ","),
    ("콤마", ","),
    ("물음표", "?"),
    ("느낌표", "!"),
];

/// 낱말 끝의 문장부호를 뗀 알맹이와 떼어 낸 것
fn core_of(tok: &str) -> (&str, &str) {
    let core = tok.trim_end_matches(|c: char| matches!(c, '.' | ',' | '…' | '~' | '?' | '!'));
    (core, &tok[core.len()..])
}

fn is_hangul(c: char) -> bool {
    ('가'..='힣').contains(&c)
}

/// 받아 적은 말을 입력하기 좋게 — 군말(음 · 어 · 머뭇거린 그…) · 되풀이(같은 낱말을 바로 또) · 말로 한 문장부호
/// ("마침표" · "쉼표" · "물음표" · "느낌표" · "줄 바꿈")를 고치고, 끝에 문장부호가 없으면 붙인다
pub fn tidy(text: &str) -> String {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let tok = toks[i];
        let (core, tail) = core_of(tok);
        // 줄 바꿈 · 새 줄 (두 낱말)
        let next_core = toks.get(i + 1).map(|t| core_of(t).0);
        if (core == "줄" && next_core == Some("바꿈")) || (core == "새" && next_core == Some("줄")) || core == "줄바꿈" {
            out.push("\n".into());
            i += if core == "줄바꿈" { 1 } else { 2 };
            continue;
        }
        if let Some((_, mark)) = SPOKEN_MARKS.iter().find(|(w, _)| *w == core) {
            match out.last_mut() {
                // 앞 낱말 끝의 문장부호를 말한 것으로 바꾼다
                Some(prev) if prev != "\n" => {
                    let keep = core_of(prev).0.len();
                    prev.truncate(keep);
                    prev.push_str(mark);
                }
                _ => {}
            }
            i += 1;
            continue;
        }
        let hesitated = tail.contains('…') || tail.contains("...") || tail.contains(',') || tail.contains('~');
        if FILLERS.contains(&core) || (hesitated && HESITANT.contains(&core)) {
            // 문장 끝 표시는 앞 낱말로 넘긴다 ("좋아요 음." → "좋아요.")
            if let (Some(prev), Some(end)) = (out.last_mut(), tail.chars().find(|c| matches!(c, '.' | '?' | '!'))) {
                if prev != "\n" && !prev.ends_with(['.', '?', '!']) {
                    prev.push(end);
                }
            }
            i += 1;
            continue;
        }
        // 같은 낱말을 바로 되풀이 ("그 그 사람" · "제가 제가") — 문장부호는 뒤엣것을 따른다
        if let Some(prev) = out.last_mut() {
            let (pc, pt) = core_of(prev);
            if !core.is_empty() && pc == core && pt.is_empty() {
                *prev = tok.to_string();
                i += 1;
                continue;
            }
        }
        out.push(tok.to_string());
        i += 1;
    }
    // 낱말 사이는 빈칸 하나, 줄 바꿈 앞뒤 빈칸은 뺀다
    let mut s = String::new();
    for t in &out {
        if t == "\n" {
            s = s.trim_end().to_string();
            s.push('\n');
        } else {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push(' ');
            }
            s.push_str(t);
        }
    }
    let mut s = s.trim().to_string();
    // 문장부호 앞 빈칸 ("좋아요 ." → "좋아요.")
    for p in [" .", " ,", " ?", " !"] {
        while let Some(at) = s.find(p) {
            s.remove(at);
        }
    }
    // 끝에 문장부호가 없으면 — 묻는 말이면 ?, 아니면 .
    if let Some(last) = s.chars().last() {
        if is_hangul(last) || last.is_ascii_alphanumeric() {
            let ask = ["까", "니", "나요", "가요", "냐", "는지요", "을래요", "할래"].iter().any(|e| s.ends_with(e));
            s.push(if ask { '?' } else { '.' });
        }
    }
    s
}

#[cfg(test)]
mod tidy_tests {
    use super::tidy;

    #[test]
    fn drops_fillers_and_keeps_meaningful_words() {
        assert_eq!(tidy("음 내일 회의는 세 시로 하죠"), "내일 회의는 세 시로 하죠.");
        assert_eq!(tidy("어, 그… 자료는 제가 보내 드릴게요."), "자료는 제가 보내 드릴게요.");
        // 머뭇거림 없이 쓴 "그" · "저" · "아" 는 뜻이 있는 말
        assert_eq!(tidy("그 사람이 저 자료를 보냈어요"), "그 사람이 저 자료를 보냈어요.");
        assert_eq!(tidy("아 그렇군요"), "아 그렇군요.");
        // 문장 끝 군말의 마침표는 앞 낱말로
        assert_eq!(tidy("좋아요 음."), "좋아요.");
    }

    #[test]
    fn drops_stutters() {
        assert_eq!(tidy("제가 제가 확인해 볼게요"), "제가 확인해 볼게요.");
        assert_eq!(tidy("그 그 문서 말이에요"), "그 문서 말이에요.");
        // 문장부호로 끊긴 되풀이는 둔다 ("네, 네.")
        assert_eq!(tidy("네, 네."), "네, 네.");
    }

    #[test]
    fn spoken_punctuation_and_line_breaks() {
        assert_eq!(tidy("안녕하세요 쉼표 김 팀장님 마침표"), "안녕하세요, 김 팀장님.");
        assert_eq!(tidy("회의는 언제 하나요 물음표"), "회의는 언제 하나요?");
        assert_eq!(tidy("첫째 줄 줄 바꿈 둘째 줄"), "첫째 줄\n둘째 줄.");
        // 이미 붙은 문장부호를 말한 것으로 바꾼다
        assert_eq!(tidy("좋습니다. 느낌표"), "좋습니다!");
        // 다른 낱말에 붙은 것은 그대로 ("마침표를 찍다")
        assert_eq!(tidy("마침표를 찍어 주세요"), "마침표를 찍어 주세요.");
    }

    #[test]
    fn ends_with_a_mark() {
        assert_eq!(tidy("내일 오시나요"), "내일 오시나요?");
        assert_eq!(tidy("이거 보셨어요?"), "이거 보셨어요?");
        assert_eq!(tidy("확인했습니다"), "확인했습니다.");
        assert_eq!(tidy("OK"), "OK.");
        assert_eq!(tidy("  "), "");
        assert_eq!(tidy("음"), "");
    }
}
