//! 녹음 뒤 다시 받아 적기 (설정 › 녹음) — 녹음 중에는 빠른 모델로 바로바로 받아 적고, 녹음이 끝나면 저장된 녹음을
//! 더 정확한(느린) 모델로 다시 받아 적어 글만 바꾼다. 문장 나눔 · 시각 · 화자는 그대로다.
//!
//! - 한 번에 한 회의씩 백그라운드로. 녹음 · 파일 받아쓰기 · 말로 입력이 도는 동안은 쉬고(모델도 내린다) 끝나면 이어서.
//! - 그 사이 사람이 고친 문장은 두고, 다시 받아 적은 글은 여러 문장씩 모아 기록에 쓰고 화면에 알린다 (`bd://redo`).
//! - 멈추거나 앱이 꺼져도 다시 받아 적은 데까지는 기록에 남고, 회의 화면에서 이어서 할 수 있다.
//! - 바꾸기 전 글을 기록에 남겨 되돌릴 수 있다 (`SessionDoc::undo_redo`).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use badasugi_core::asr::{self, EngineConfig};
use badasugi_core::redo::{Hold, Request, Span};
use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::session::{SessionDoc, SessionStore};
use crate::settings::Settings;
use crate::{AppState, CmdResult, RecState};

/// 기록에 쓰는 간격 — 문장마다 쓰지 않고 모아서 (OneDrive 로 동기화되는 문서 폴더에서 큰 파일을 자주 다시 올리지 않게)
const SAVE_EVERY: Duration = Duration::from_secs(8);
const SAVE_BATCH: usize = 20;
/// 녹음 · 말로 입력이 끝나고 이만큼 조용해야 (다시) 시작한다
const SETTLE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub id: String,
    /// 다시 받아 적은 문장 수 (앞서 멈췄다 이어서 하면 그때 한 것까지)
    pub done: usize,
    pub total: usize,
}

/// 화면에 알리는 지금 상태 (`bd://redo-status` · `get_state`)
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub running: Option<Progress>,
    /// 차례를 기다리는 기록 id
    pub queued: Vec<String>,
    /// 녹음 · 말로 입력이 도는 동안 쉬는 중
    pub waiting: bool,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<String>,
    running: Option<Progress>,
    waiting: bool,
    /// 일하는 스레드가 있다
    worker: bool,
}

#[derive(Default)]
pub struct RedoQueue {
    inner: Mutex<Inner>,
    /// 지금 하는 회의를 멈추라는 신호
    stop: AtomicBool,
}

impl RedoQueue {
    pub fn status(&self) -> Status {
        let i = self.inner.lock();
        Status {
            running: i.running.clone(),
            queued: i.queue.iter().cloned().collect(),
            waiting: i.waiting,
        }
    }
}

fn emit_status(app: &AppHandle) {
    let _ = app.emit("bd://redo-status", app.state::<RedoQueue>().status());
}

/// 다시 받아 적을 모델 (설정의 id, 파일 경로) — 로컬 엔진에서 녹음 때와 다른 모델을 골랐고 파일이 있을 때
pub fn model(s: &Settings, models_dir: &Path) -> Result<(String, PathBuf), String> {
    let Some(path) = s.redo_model_path(models_dir) else {
        return Err("설정 › 녹음에서 '녹음이 끝나면 다시 받아 적기' 모델을 골라 주세요 (로컬 엔진)".into());
    };
    if !path.is_file() {
        return Err("다시 받아 적을 모델 파일이 없어요. 설정 › 녹음에서 내려받아 주세요.".into());
    }
    Ok((s.redo_model.trim().to_string(), path))
}

/// 다시 받아 적을 소리: 저장한 녹음, 파일 받아쓰기였으면 원본 파일 (있을 때만)
fn audio_of(store: &SessionStore, doc: &SessionDoc) -> Option<PathBuf> {
    let saved = doc.audio_file.as_ref().map(|f| store.dir.join(f));
    let source = doc.source_path.as_ref().map(PathBuf::from);
    saved.into_iter().chain(source).find(|p| p.is_file())
}

/// 이 회의를 다시 받아 적을 수 있나 — 안 되면 그 까닭
fn check(app: &AppHandle, id: &str) -> Result<(), String> {
    let st = app.state::<AppState>();
    let s = st.settings.lock().clone();
    model(&s, &st.paths.models)?;
    let store = st.store.lock();
    if store.current.as_ref().is_some_and(|c| c.id == id) {
        return Err("녹음이 끝난 뒤에 다시 받아 적을 수 있어요".into());
    }
    let doc = store.load(id).map_err(|_| "지운 기록이에요".to_string())?;
    if doc.utterances.is_empty() {
        return Err("받아 적은 문장이 없어요".into());
    }
    if audio_of(&store, &doc).is_none() {
        return Err("녹음 파일이 없어 다시 받아 적을 수 없어요 — 설정 › 녹음 › '녹음 파일도 저장'을 켜면 다음 회의부터 돼요".into());
    }
    Ok(())
}

/// 이 회의를 다시 받아 적는다 (앞 회의가 끝나면 차례로). 이미 하고 있거나 기다리는 중이면 그대로
pub fn enqueue(app: &AppHandle, id: &str) -> Result<(), String> {
    check(app, id)?;
    let q = app.state::<RedoQueue>();
    let spawn = {
        let mut i = q.inner.lock();
        if i.running.as_ref().is_some_and(|p| p.id == id) || i.queue.iter().any(|x| x == id) {
            return Ok(());
        }
        i.queue.push_back(id.to_string());
        !std::mem::replace(&mut i.worker, true)
    };
    if spawn {
        // 도는 동안 업데이트 설치를 미룬다 (일하는 스레드가 끝날 때 내린다)
        app.state::<AppState>().busy_ops.fetch_add(1, Ordering::SeqCst);
        let app2 = app.clone();
        if let Err(e) = std::thread::Builder::new().name("redo".into()).spawn(move || work(&app2)) {
            app.state::<AppState>().busy_ops.fetch_sub(1, Ordering::SeqCst);
            let mut i = q.inner.lock();
            i.worker = false;
            i.queue.retain(|x| x != id);
            return Err(format!("다시 받아 적기를 시작하지 못했어요: {e}"));
        }
    }
    emit_status(app);
    Ok(())
}

/// 녹음이 끝났다 — 설정에서 켰으면 다시 받아 적기를 시작한다 (못 하면 그 까닭을 화면에 한 번 알린다)
pub fn after_recording(app: &AppHandle, id: &str) {
    {
        let st = app.state::<AppState>();
        if st.settings.lock().redo_model_path(&st.paths.models).is_none() {
            return;
        }
    }
    match enqueue(app, id) {
        Ok(()) => log::info!("녹음 뒤 다시 받아 적기 시작: 기록 {id}"),
        Err(e) => {
            log::info!("녹음 뒤 다시 받아 적기 못 함 (기록 {id}): {e}");
            // 받아 적은 문장이 없어 지운 기록은 조용히
            if !e.starts_with("지운 기록") && !e.starts_with("받아 적은 문장이 없") {
                let _ = app.emit("bd://redo", serde_json::json!({ "kind": "error", "id": id, "message": e }));
            }
        }
    }
}

/// 멈춘다 — 하고 있으면 받아 적은 데까지 기록에 남기고, 기다리던 것이면 빼기만 한다
pub fn cancel(app: &AppHandle, id: &str) {
    let q = app.state::<RedoQueue>();
    {
        let mut i = q.inner.lock();
        if i.running.as_ref().is_some_and(|p| p.id == id) {
            q.stop.store(true, Ordering::SeqCst);
        } else {
            i.queue.retain(|x| x != id);
        }
    }
    emit_status(app);
}

/// 한 회의를 다시 받아 적은 결과
enum Outcome {
    /// 끝까지 했다 (바뀐 문장 수 · 문장 수)
    Finished { changed: usize, total: usize },
    /// 멈췄다 (받아 적은 데까지 남김)
    Stopped,
    /// 하는 사이 기록을 지웠다
    Deleted,
}

fn work(app: &AppHandle) {
    let q = app.state::<RedoQueue>();
    let mut used: Option<PathBuf> = None;
    badasugi_core::perf::disable_power_throttling();
    loop {
        let id = {
            let mut i = q.inner.lock();
            let Some(id) = i.queue.pop_front() else {
                i.worker = false;
                i.running = None;
                i.waiting = false;
                break;
            };
            i.running = Some(Progress {
                id: id.clone(),
                done: 0,
                total: 0,
            });
            id
        };
        q.stop.store(false, Ordering::SeqCst);
        emit_status(app);
        let t0 = Instant::now();
        let r = run_one(app, &id, &mut used);
        let secs = t0.elapsed().as_secs();
        let ev = match r {
            Ok(Outcome::Finished { changed, total }) => {
                log::info!("다시 받아 적기 끝: 기록 {id} · 문장 {total}개 중 {changed}개 바뀜 · {secs}초");
                serde_json::json!({ "kind": "finished", "id": id, "changed": changed, "total": total })
            }
            Ok(Outcome::Stopped) => {
                log::info!("다시 받아 적기 멈춤: 기록 {id} · {secs}초");
                serde_json::json!({ "kind": "stopped", "id": id })
            }
            Ok(Outcome::Deleted) => {
                log::info!("다시 받아 적던 기록을 지움: {id}");
                serde_json::json!({ "kind": "deleted", "id": id })
            }
            Err(e) => {
                log::warn!("다시 받아 적기 실패 (기록 {id}): {e}");
                serde_json::json!({ "kind": "error", "id": id, "message": e })
            }
        };
        q.inner.lock().running = None;
        let _ = app.emit("bd://redo", ev);
    }
    emit_status(app);
    // 녹음 때 모델과 다른 큰 모델을 메모리에 들고 있지 않게 (그 사이 녹음이 다른 모델을 열었으면 그대로)
    if let Some(p) = used {
        badasugi_core::asr::whisper::unload_if(&p);
    }
    let st = app.state::<AppState>();
    if *st.rec.lock() == RecState::Idle {
        badasugi_core::perf::restore_power_throttling();
    }
    st.busy_ops.fetch_sub(1, Ordering::SeqCst);
    crate::update::maybe_install(app);
}

/// 다시 받아 적은 글을 모아 두었다가 기록에 쓴다
struct Saver<'a> {
    app: &'a AppHandle,
    id: &'a str,
    model: &'a str,
    /// (발화 id, 시작할 때의 글, 새 글)
    batch: Vec<(u64, String, String)>,
    /// 마지막으로 다시 받아 적은 문장의 시작 시각
    last_start: Option<u64>,
    last_save: Instant,
    error: Option<String>,
    deleted: bool,
}

impl Saver<'_> {
    fn due(&self) -> bool {
        self.batch.len() >= SAVE_BATCH || self.last_save.elapsed() >= SAVE_EVERY
    }

    /// 모은 글과 진행을 기록에 쓴다 → 끝났으면 (바뀐 문장 수)
    fn save(&mut self, finished: bool) -> usize {
        if self.deleted || self.error.is_some() {
            return 0;
        }
        let batch = std::mem::take(&mut self.batch);
        let (model, last) = (self.model, self.last_start);
        let st = self.app.state::<AppState>();
        let r = st.store.lock().update_saved(self.id, |d| {
            let edits = d.apply_redo(model, &batch);
            let r = d.redo.get_or_insert_with(Default::default);
            r.model = model.to_string();
            r.done_ms = last.or(r.done_ms);
            r.finished = finished;
            (edits, r.changed.len())
        });
        self.last_save = Instant::now();
        match r {
            Ok((edits, changed)) => {
                if !edits.is_empty() {
                    let _ = self
                        .app
                        .emit("bd://redo", serde_json::json!({ "kind": "edits", "id": self.id, "edits": edits }));
                }
                changed
            }
            Err(e) => {
                if st.store.lock().load(self.id).is_err() {
                    self.deleted = true;
                } else {
                    self.error = Some(format!("기록을 저장하지 못했어요: {e}"));
                }
                // 더 받아 적지 않는다
                self.app.state::<RedoQueue>().stop.store(true, Ordering::SeqCst);
                0
            }
        }
    }
}

fn run_one(app: &AppHandle, id: &str, used: &mut Option<PathBuf>) -> Result<Outcome, String> {
    let st = app.state::<AppState>();
    let q = app.state::<RedoQueue>();
    let s = st.settings.lock().clone();
    let (model_id, path) = model(&s, &st.paths.models)?;
    let (doc, audio) = {
        let store = st.store.lock();
        if store.current.as_ref().is_some_and(|c| c.id == id) {
            return Err("녹음이 끝난 뒤에 다시 받아 적을 수 있어요".into());
        }
        let Ok(doc) = store.load(id) else {
            return Ok(Outcome::Deleted);
        };
        let audio = audio_of(&store, &doc).ok_or("녹음 파일이 없어 다시 받아 적을 수 없어요")?;
        (doc, audio)
    };
    // 같은 모델로 하다 멈춘 회의면 그 뒤부터
    let from = match &doc.redo {
        Some(r) if r.model == model_id && !r.finished => r.done_ms,
        _ => None,
    };
    let spans: Vec<Span> = doc
        .utterances
        .iter()
        .filter(|u| from.is_none_or(|f| u.start_ms > f))
        .map(|u| Span {
            id: u.id,
            start_ms: u.start_ms,
            end_ms: u.end_ms,
        })
        .collect();
    let seen: HashMap<u64, String> = doc.utterances.iter().map(|u| (u.id, u.text.clone())).collect();
    let total = doc.utterances.len();
    let mut done = total - spans.len();
    set_progress(app, id, done, total);
    // 하고 있다는 표시를 먼저 남긴다 (앱이 꺼져도 이어서 할 수 있게)
    st.store
        .lock()
        .update_saved(id, |d| {
            let r = d.redo.get_or_insert_with(Default::default);
            if r.model != model_id || r.finished {
                r.model = model_id.clone();
                r.done_ms = from;
                r.finished = false;
            }
        })
        .map_err(|e| e.to_string())?;
    *used = Some(path.clone());

    let terms = crate::glossary_terms(&s.glossary);
    let prompt = crate::glossary_prompt(&terms);
    let engine = EngineConfig::Local {
        model_path: path,
        threads: s.threads,
        beam_size: s.beam_size,
    };
    let req = Request {
        language: &s.language,
        prompt: &prompt,
        keywords: &terms,
    };
    let mut saver = Saver {
        app,
        id,
        model: &model_id,
        batch: Vec::new(),
        last_start: from,
        last_save: Instant::now(),
        error: None,
        deleted: false,
    };
    // 녹음 · 말로 입력이 끝나고 조용해진 때 (연달아 하는 일 사이의 잠깐 빈 틈에 끼어들지 않게)
    let idle_since: Mutex<Option<Instant>> = Mutex::new(None);
    // 구간 사이와 받아 적는 중에도(낱말마다) 묻는다 — 바쁘면 하던 구간도 그만두고 쉰다
    let hold = || {
        if q.stop.load(Ordering::SeqCst) {
            return Hold::Stop;
        }
        // 녹음 · 파일 받아쓰기 · 녹음 폴더 · 말로 입력이 도는 동안은 쉰다 (CPU · GPU 를 그쪽에)
        let busy = *st.rec.lock() != RecState::Idle || app.state::<crate::dictate::Dictate>().active();
        let changed = {
            let mut i = q.inner.lock();
            std::mem::replace(&mut i.waiting, busy) != busy
        };
        if changed {
            if busy {
                log::info!("다시 받아 적기 쉼 (녹음 · 파일 받아쓰기 · 말로 입력 중)");
            } else {
                log::info!("다시 받아 적기 이어서 할 준비 ({}초 조용하면)", SETTLE.as_secs());
                // 쉬는 사이 끝난 녹음이 Windows 전원 절약을 되돌려 두었다 → 다시 끈다 (백그라운드에서 느려지지 않게)
                badasugi_core::perf::disable_power_throttling();
            }
            emit_status(app);
        }
        let mut idle = idle_since.lock();
        if busy {
            *idle = None;
            return Hold::Wait;
        }
        if idle.get_or_insert_with(Instant::now).elapsed() < SETTLE {
            Hold::Wait
        } else {
            Hold::Go
        }
    };
    let r = badasugi_core::redo::run(
        &audio,
        &spans,
        &req,
        &mut || asr::create(&engine),
        &hold,
        &mut |span, text| {
            if let (Some(new), Some(old)) = (text, seen.get(&span.id)) {
                saver.batch.push((span.id, old.clone(), new));
            }
            saver.last_start = Some(span.start_ms);
            done += 1;
            set_progress(app, id, done, total);
            if saver.due() {
                saver.save(false);
            }
        },
    );
    q.inner.lock().waiting = false;
    match r {
        Ok(true) => {
            let changed = saver.save(true);
            match (saver.deleted, saver.error.take()) {
                (true, _) => Ok(Outcome::Deleted),
                (_, Some(e)) => Err(e),
                _ => Ok(Outcome::Finished { changed, total }),
            }
        }
        Ok(false) => {
            saver.save(false);
            match (saver.deleted, saver.error.take()) {
                (true, _) => Ok(Outcome::Deleted),
                (_, Some(e)) => Err(e),
                _ => Ok(Outcome::Stopped),
            }
        }
        Err(e) => {
            // 받아 적은 데까지는 남긴다 (이어서 할 수 있게)
            saver.save(false);
            Err(format!("다시 받아 적지 못했어요: {e}"))
        }
    }
}

fn set_progress(app: &AppHandle, id: &str, done: usize, total: usize) {
    {
        let q = app.state::<RedoQueue>();
        let mut i = q.inner.lock();
        match i.running.as_mut() {
            Some(p) if p.id == id => {
                p.done = done;
                p.total = total;
            }
            _ => return,
        }
    }
    emit_status(app);
}

// ───────────────────────── 명령 ─────────────────────────

/// 회의 화면의 '더 정확한 모델로 다시 받아 적기' · '이어서 하기'
#[tauri::command]
pub fn redo_session(app: AppHandle, id: String) -> CmdResult<()> {
    enqueue(&app, &id)
}

#[tauri::command]
pub fn cancel_redo(app: AppHandle, id: String) {
    cancel(&app, &id);
}

/// 다시 받아 적기 되돌리기 → 되돌린 기록
#[tauri::command]
pub async fn undo_redo(app: AppHandle, id: String) -> CmdResult<SessionDoc> {
    let busy = {
        let st = app.state::<RedoQueue>().status();
        st.running.is_some_and(|p| p.id == id) || st.queued.contains(&id)
    };
    if busy {
        return Err("다시 받아 적는 중이에요 — 멈춘 뒤에 되돌려 주세요".into());
    }
    let (doc, n) = app.state::<AppState>().store.lock().undo_redo(&id).map_err(|e| e.to_string())?;
    log::info!("다시 받아 적기 되돌림: 기록 {id} · 문장 {n}개");
    Ok(doc)
}
