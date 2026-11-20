//! 녹음 폴더 지켜보기 — 휴대폰 녹음 · 녹음기 파일이 쌓이는 폴더(OneDrive 등)에 새 녹음 파일이 생기면
//! 알아서 받아 적고(녹음 파일 받아쓰기 흐름 그대로) 요약까지 한다. 설정에서 켰을 때만.
//!
//! - **새 파일만**: 켜는 순간 폴더에 있던 파일은 건너뛴다(기준점). 앱이 꺼져 있던 동안 생긴 파일은 다음에 켜졌을 때
//!   받아 적는다. 끄고 다시 켜면 그때 있던 파일이 새 기준점이다 (쌓여 있던 녹음을 한꺼번에 API 로 보내지 않게).
//! - **다 쓴 파일만**: 아직 쓰는 중(동기화 · 복사 중)인 파일은 크기 · 수정 시각이 15초 넘게 그대로이고, 다른 프로그램이
//!   쓰기로 열고 있지 않을 때까지 기다린다.
//! - **녹음이 먼저**: 녹음 · 정리 중이면 끝나고 10초 쉰 뒤에 한다. 받아 적는 중에 녹음을 시작하면(단축키 · 트레이 ·
//!   회의 알림 · 녹음 단추) 받아쓰기를 멈추고 녹음을 먼저 하며, 그 파일은 녹음이 끝난 뒤 처음부터 다시 받아 적는다.
//! - **두 번 하지 않기**: 받아 적은 파일은 설정 폴더의 `watch.json` 에 (이름 + 크기로) 기억한다. 사람이 멈춘 파일 ·
//!   세 번 실패한 파일도 다시 하지 않는다. 받아 적는 사이 앱이 꺼졌으면 다음에 켤 때 받아 적던 기록을 지우고 다시 한다.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::{AppState, RecState};

/// 켜 두었을 때 폴더를 훑는 간격
const POLL: Duration = Duration::from_secs(5);
/// 크기 · 수정 시각이 이만큼 그대로여야 다 쓴(동기화가 끝난) 파일로 본다
const STABLE_FOR: Duration = Duration::from_secs(15);
/// 앱을 켜고 이만큼 지나야 받아 적기 시작한다 (켜자마자 무겁지 않게)
const START_DELAY: Duration = Duration::from_secs(20);
/// 녹음을 마치고 이만큼 쉰 뒤에 받아 적는다 (곧바로 다음 녹음을 시작할 수 있게)
const IDLE_GAP: Duration = Duration::from_secs(10);
/// 오류로 멈춘 파일은 이만큼 쉬었다가 다시 ([`MAX_TRIES`] 번까지)
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);
const MAX_TRIES: u32 = 3;
/// 녹음에 비켜 주는 데(이미 넘긴 발화 인식 · 화자 정리) 기다리는 최대 시간
const YIELD_WAIT: Duration = Duration::from_secs(90);
/// 자동 요약 번호 — 화면의 요약 번호(Date.now())와 겹치지 않게
const SUMMARY_TOKEN_BASE: u64 = 1 << 50;

/// 자동 점검(smoke)은 기다리는 시간을 1/5 로 (점검이 너무 길어지지 않게)
fn wait(d: Duration) -> Duration {
    if crate::smoke::enabled() {
        d / 5
    } else {
        d
    }
}

// ───────────────────────── 기억 (watch.json) ─────────────────────────

/// 파일 하나를 어떻게 했나
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum FileState {
    /// 켜기 전부터 있던 파일 — 건너뜀
    #[default]
    Skip,
    /// 받아 적음
    Done,
    /// 받아 적을 말이 없었다 (기록이 남지 않음)
    Empty,
    /// 사람이 멈춤 — 받아 적은 데까지 기록에 남기고 다시 하지 않는다
    Stopped,
    /// 오류로 멈춤 — 잠시 뒤 다시
    Retry,
    /// 세 번 실패
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Entry {
    pub state: FileState,
    /// 파일 이름 (화면에 보일 것)
    pub name: String,
    /// 받아 적은 기록 id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub tries: u32,
    /// 마지막으로 처리한 때 "2026-11-20 15:10"
    #[serde(skip_serializing_if = "String::is_empty")]
    pub at: String,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// 받아 적는 중이던 파일 (앱이 그 사이 꺼지면 다음에 켤 때 그 기록을 지우고 다시)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Active {
    pub key: String,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Book {
    /// 이 기억이 가리키는 폴더 (설정의 폴더가 바뀌면 처음부터)
    pub dir: String,
    /// 지켜보기를 켠 뒤로 이어지는 중 — 끄면 false, 다시 켜면 그때 있던 파일이 새 기준점
    pub armed: bool,
    /// 파일 키(이름 소문자 + 크기) → 한 일
    pub files: BTreeMap<String, Entry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<Active>,
}

/// 폴더에서 찾은 녹음 파일
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl Found {
    pub fn key(&self) -> String {
        file_key(&self.name, self.size)
    }
}

/// 같은 파일인지 가리는 키 — 이름(대소문자 무시) + 크기. 다 쓴 뒤의 크기라 쓰는 중에는 바뀐다
pub fn file_key(name: &str, size: u64) -> String {
    format!("{}|{size}", name.to_lowercase())
}

/// 같은 폴더인가 (대소문자 · 끝의 `\` 무시 — Windows)
pub fn same_dir(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// 받아 적을 수 있는 녹음 파일 이름인가 (숨김 · 임시 파일 제외)
pub fn is_audio_name(name: &str) -> bool {
    if name.starts_with('.') || name.starts_with('~') {
        return false;
    }
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| badasugi_core::decode::AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
}

impl Book {
    /// 지켜보기를 켠다 — 지금 폴더에 있는 파일은 건너뛴다. 다른 폴더였으면 처음부터 → 건너뛴 파일 수
    pub fn arm(&mut self, dir: &str, found: &[Found]) -> usize {
        if !same_dir(&self.dir, dir) {
            self.files.clear();
        }
        self.dir = dir.trim().to_string();
        let mut n = 0;
        for f in found {
            if let std::collections::btree_map::Entry::Vacant(v) = self.files.entry(f.key()) {
                v.insert(Entry { state: FileState::Skip, name: f.name.clone(), ..Default::default() });
                n += 1;
            }
        }
        self.armed = true;
        n
    }

    /// 아직 받아 적을 파일인가 — 기억에 없거나, 오류로 멈춰 다시 할 차례인 파일
    pub fn wants(&self, key: &str) -> bool {
        self.files.get(key).is_none_or(|e| e.state == FileState::Retry && e.tries < MAX_TRIES)
    }

    fn tries(&self, key: &str) -> u32 {
        self.files.get(key).map_or(0, |e| e.tries)
    }

    fn set(&mut self, key: &str, name: &str, state: FileState, doc: Option<String>, tries: u32) {
        self.files.insert(
            key.to_string(),
            Entry { state, name: name.to_string(), doc, tries, at: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string() },
        );
    }

    fn load(path: &Path) -> Book {
        crate::settings::load_json_or_default(path)
    }

    fn save(&self, path: &Path) {
        let r = (|| -> anyhow::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
            std::fs::rename(tmp, path)?;
            Ok(())
        })();
        if let Err(e) = r {
            log::warn!("녹음 폴더 기억(watch.json)을 저장하지 못함: {e}");
        }
    }
}

// ───────────────────────── 다 쓴 파일 가리기 ─────────────────────────

/// 크기 · 수정 시각이 멈춘 지 얼마나 됐나 (쓰는 중인 파일은 계속 바뀐다)
#[derive(Default)]
pub struct Settle {
    seen: HashMap<PathBuf, (u64, Option<SystemTime>, Instant)>,
}

impl Settle {
    /// 이번에 훑은 (아직 받아 적을) 파일 → 크기 · 수정 시각이 `stable` 넘게 그대로인 파일.
    /// 이번 훑기에 없는 파일(지워짐 · 이름 바뀜)은 잊는다
    pub fn settled<'a>(&mut self, found: &[&'a Found], now: Instant, stable: Duration) -> Vec<&'a Found> {
        let here: HashSet<&PathBuf> = found.iter().map(|f| &f.path).collect();
        self.seen.retain(|p, _| here.contains(p));
        let mut out = Vec::new();
        for f in found {
            let e = self.seen.entry(f.path.clone()).or_insert((f.size, f.modified, now));
            if e.0 != f.size || e.1 != f.modified {
                *e = (f.size, f.modified, now);
                continue;
            }
            if f.size > 0 && now.duration_since(e.2) >= stable {
                out.push(*f);
            }
        }
        out
    }

    fn clear(&mut self) {
        self.seen.clear();
    }
}

/// 폴더의 녹음 파일 (하위 폴더는 보지 않는다)
fn scan(dir: &Path) -> std::io::Result<Vec<Found>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !is_audio_name(&name) {
            continue;
        }
        let Ok(md) = e.metadata() else {
            continue;
        };
        if !md.is_file() || hidden_or_temp(&md) {
            continue;
        }
        out.push(Found { path: e.path(), name, size: md.len(), modified: md.modified().ok() });
    }
    Ok(out)
}

#[cfg(windows)]
fn hidden_or_temp(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_HIDDEN | SYSTEM | TEMPORARY
    md.file_attributes() & (0x2 | 0x4 | 0x100) != 0
}

#[cfg(not(windows))]
fn hidden_or_temp(_: &std::fs::Metadata) -> bool {
    false
}

/// 다른 프로그램이 쓰기로 열고 있지 않은가 — 읽기만 함께 쓰게 열어 본다 (쓰는 쪽이 있으면 공유 위반으로 실패)
#[cfg(windows)]
fn writer_gone(p: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;
    std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(p).is_ok()
}

#[cfg(not(windows))]
fn writer_gone(p: &Path) -> bool {
    std::fs::File::open(p).is_ok()
}

// ───────────────────────── 받아쓰기 하나 ─────────────────────────

/// 지금 받아 적는 파일
#[derive(Debug, Clone, Default)]
struct Job {
    key: String,
    name: String,
    doc: Option<String>,
    /// 녹음에 자리를 비켜 줌 (또는 앱 종료) → 받아 적던 기록을 지우고 다음에 처음부터
    yielded: bool,
    /// 사람이 멈춤 → 받아 적은 데까지 남기고 다시 하지 않는다
    stopped: bool,
    /// 오류로 멈춤 (연결이 계속 안 됨 · 엔진을 못 엶 등) → 잠시 뒤 다시
    error: Option<String>,
    /// 받아 적은 문장 수 · 다시 시도하지 않고 실패한 문장 수 (잘못된 API 키처럼 문장마다 실패하면 오류로 본다)
    ok: usize,
    failed: usize,
    /// 처음 실패한 까닭
    fail_msg: Option<String>,
    /// 받아쓰기가 끝났다 (`Stopped`)
    ended: bool,
}

impl Job {
    /// 오류로 볼 까닭 — 받아쓰기가 오류로 멈췄거나, 문장의 10% 넘게 받아 적지 못했다 (빈 기록 · 구멍 난 기록으로 끝내지 않게)
    fn failure(&self) -> Option<String> {
        self.error.clone().or_else(|| {
            failed_too_many(self.ok, self.failed).then(|| {
                format!("문장 {}개 중 {}개를 받아 적지 못했어요 — {}", self.ok + self.failed, self.failed, self.fail_msg.clone().unwrap_or_default())
            })
        })
    }
}

/// 받아 적지 못한 문장이 10% 넘으면 그 파일은 다시 받아 적는다
pub fn failed_too_many(ok: usize, failed: usize) -> bool {
    failed > 0 && failed * 10 > ok + failed
}

/// 받아쓰기가 끝난 뒤 할 일
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 기록에 남김 (요약까지)
    Done,
    /// 받아 적을 말이 없었다
    Empty,
    /// 사람이 멈춤 — 받아 적은 데까지 남긴다
    Stopped,
    /// 녹음에 비켜 줌 — 받아 적던 기록을 지우고 다시 (횟수에 세지 않음)
    Requeue,
    /// 오류 — 받아 적던 기록을 지우고 잠시 뒤 다시
    Retry,
    /// 세 번째 오류 — 받아 적은 데까지 남기고 그만
    Failed,
}

/// `tries`: 이번까지 오류로 멈춘 횟수
pub fn outcome(yielded: bool, stopped: bool, error: bool, saved: bool, tries: u32) -> Outcome {
    match () {
        _ if yielded => Outcome::Requeue,
        _ if stopped => Outcome::Stopped,
        _ if error && tries >= MAX_TRIES => Outcome::Failed,
        _ if error => Outcome::Retry,
        _ if saved => Outcome::Done,
        _ => Outcome::Empty,
    }
}

// ───────────────────────── 상태 ─────────────────────────

/// 설정 화면에 보일 상태
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchStatus {
    pub on: bool,
    /// 폴더 · 엔진 문제 (있으면 받아 적지 못하고 기다린다)
    pub problem: Option<String>,
    /// 지금 받아 적는 파일
    pub working: Option<String>,
    /// 받아 적기를 기다리는 파일 (아직 쓰는 중 · 녹음이 끝나기를 기다리는 중)
    pub waiting: usize,
    pub done: usize,
    pub skipped: usize,
    pub stopped: usize,
    pub failed: usize,
    /// 마지막으로 받아 적은 파일
    pub last: Option<Entry>,
}

#[derive(Default)]
struct Inner {
    book: Book,
    job: Option<Job>,
    /// 녹음을 시작하려고 받아쓰기를 비키게 하는 중 — 새 파일을 시작하지 않는다
    hold: bool,
    settle: Settle,
    /// 오류로 멈춘 파일 → 다시 해도 되는 때
    cooldown: HashMap<String, Instant>,
    on: bool,
    problem: Option<String>,
    waiting: usize,
}

pub struct Watch {
    path: PathBuf,
    inner: Mutex<Inner>,
    /// 받아 적는 중인 파일이 있다 (잠금 없이 — 트레이 글자 · 회의 알림 · 녹음 단축키)
    busy: AtomicBool,
    wake_tx: Mutex<Sender<()>>,
    wake_rx: Mutex<Option<Receiver<()>>>,
    tokens: AtomicU64,
}

impl Watch {
    pub fn load(path: PathBuf) -> Self {
        let (tx, rx) = channel();
        let book = Book::load(&path);
        Self {
            path,
            inner: Mutex::new(Inner { book, ..Default::default() }),
            busy: AtomicBool::new(false),
            wake_tx: Mutex::new(tx),
            wake_rx: Mutex::new(Some(rx)),
            tokens: AtomicU64::new(0),
        }
    }

    fn wake(&self) {
        let _ = self.wake_tx.lock().send(());
    }

    pub fn status(&self) -> WatchStatus {
        let i = self.inner.lock();
        let mut s = WatchStatus {
            on: i.on,
            problem: i.problem.clone(),
            working: i.job.as_ref().filter(|j| !j.ended).map(|j| j.name.clone()),
            waiting: i.waiting,
            ..Default::default()
        };
        for e in i.book.files.values() {
            match e.state {
                FileState::Done | FileState::Empty => s.done += 1,
                FileState::Skip => s.skipped += 1,
                FileState::Stopped => s.stopped += 1,
                FileState::Failed => s.failed += 1,
                FileState::Retry => {}
            }
        }
        s.last = i
            .book
            .files
            .values()
            .filter(|e| e.state == FileState::Done)
            .max_by(|a, b| a.at.cmp(&b.at))
            .cloned();
        s
    }
}

// ───────────────────────── 앱이 부르는 곳 ─────────────────────────

/// 받아 적는 중인 녹음 폴더 파일이 있나
pub fn job_running(app: &AppHandle) -> bool {
    app.try_state::<Watch>().is_some_and(|w| w.busy.load(Ordering::SeqCst))
}

/// 지금 받아 적는 녹음 폴더 파일 이름
pub fn job_name(app: &AppHandle) -> Option<String> {
    let w = app.try_state::<Watch>()?;
    let i = w.inner.lock();
    i.job.as_ref().filter(|j| !j.ended).map(|j| j.name.clone())
}

/// 설정을 저장했다 — 곧바로 다시 본다 (켜자마자 기준점을 잡게)
pub fn wake(app: &AppHandle) {
    if let Some(w) = app.try_state::<Watch>() {
        w.wake();
    }
}

/// 받아쓰기가 끝났다 (`Stopped`) — 끝난 기록 id
pub fn job_ended(app: &AppHandle, doc: Option<&str>) {
    let Some(w) = app.try_state::<Watch>() else {
        return;
    };
    let mut i = w.inner.lock();
    if let Some(j) = i.job.as_mut().filter(|j| !j.ended) {
        j.ended = true;
        if j.doc.is_none() {
            j.doc = doc.map(str::to_string);
        }
        w.busy.store(false, Ordering::SeqCst);
        w.wake();
    }
}

/// 받아쓰기 중 오류 (연결이 계속 안 돼 멈춤 · 엔진을 못 엶 등)
pub fn on_error(app: &AppHandle, message: &str) {
    if let Some(w) = app.try_state::<Watch>() {
        if let Some(j) = w.inner.lock().job.as_mut().filter(|j| !j.ended) {
            j.error.get_or_insert_with(|| message.to_string());
        }
    }
}

/// 문장 하나를 받아 적었다 · 받아 적지 못했다 (다시 시도하지 않는 실패) — 받아 적는 중인 녹음 폴더 파일만 센다
pub fn on_utterance(app: &AppHandle, failed: Option<&str>) {
    if let Some(w) = app.try_state::<Watch>() {
        if !w.busy.load(Ordering::SeqCst) {
            return;
        }
        if let Some(j) = w.inner.lock().job.as_mut().filter(|j| !j.ended) {
            match failed {
                None => j.ok += 1,
                Some(m) => {
                    j.failed += 1;
                    j.fail_msg.get_or_insert_with(|| m.to_string());
                }
            }
        }
    }
}

/// 사람이 멈추기를 눌렀다 (녹음에 비켜 주는 중이면 그대로)
pub fn on_stop(app: &AppHandle) {
    if let Some(w) = app.try_state::<Watch>() {
        if let Some(j) = w.inner.lock().job.as_mut().filter(|j| !j.ended && !j.yielded) {
            log::info!("녹음 폴더 파일 받아쓰기를 사람이 멈춤");
            j.stopped = true;
        }
    }
}

/// 앱을 끈다 — 받아 적던 파일은 다음에 처음부터
pub fn interrupt(app: &AppHandle) {
    if let Some(w) = app.try_state::<Watch>() {
        if let Some(j) = w.inner.lock().job.as_mut().filter(|j| !j.ended) {
            j.yielded = true;
        }
    }
}

/// 녹음을 시작하기 전에: 녹음 폴더 파일을 받아 적는 중이면 멈추고(그 파일은 녹음 뒤 다시) 끝날 때까지 기다린다.
/// 돌려받은 손잡이를 쥐고 있는 동안은 새 파일을 시작하지 않는다 (녹음 시작이 끝날 때까지 쥔다)
pub fn make_way(app: &AppHandle) -> Result<Hold, String> {
    let Some(w) = app.try_state::<Watch>() else {
        return Ok(Hold(None));
    };
    let name = {
        let mut i = w.inner.lock();
        i.hold = true;
        i.job.as_mut().filter(|j| !j.ended).map(|j| {
            // 사람이 이미 멈춘 파일은 멈춘 대로 둔다 (끝나기만 기다린다)
            if !j.stopped {
                j.yielded = true;
            }
            j.name.clone()
        })
    };
    let hold = Hold(Some(app.clone()));
    let Some(name) = name else {
        return Ok(hold);
    };
    log::info!("녹음을 먼저 — 받아 적던 녹음 폴더 파일은 녹음을 마친 뒤 다시");
    let _ = app.emit("bd://watch", serde_json::json!({ "kind": "yield", "file": name }));
    let st = app.state::<AppState>();
    let t = Instant::now();
    loop {
        let ended = w.inner.lock().job.as_ref().is_none_or(|j| j.ended);
        let rec = *st.rec.lock();
        if ended && rec == RecState::Idle {
            return Ok(hold);
        }
        // 아직 시작하는 중(모델 불러오기)이면 시작된 뒤에 멈춘다
        if rec == RecState::Recording && st.file_mode.load(Ordering::SeqCst) && st.pipeline.lock().is_some() {
            crate::stop_recording_inner(app);
        }
        if t.elapsed() > YIELD_WAIT {
            return Err("받아 적던 녹음 폴더 파일을 멈추는 데 오래 걸리고 있어요. 잠시 뒤 다시 녹음을 시작해 주세요".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// [`make_way`] 손잡이 — 놓으면 다시 새 파일을 받아 적을 수 있다
pub struct Hold(Option<AppHandle>);

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(w) = self.0.as_ref().and_then(|a| a.try_state::<Watch>()) {
            w.inner.lock().hold = false;
            w.wake();
        }
    }
}

#[tauri::command]
pub fn watch_status(app: AppHandle) -> WatchStatus {
    app.state::<Watch>().status()
}

// ───────────────────────── 지켜보는 스레드 ─────────────────────────

pub fn spawn(app: AppHandle) {
    let Some(rx) = app.state::<Watch>().wake_rx.lock().take() else {
        return;
    };
    let _ = std::thread::Builder::new().name("bd-watch".into()).spawn(move || run(app, rx));
}

fn run(app: AppHandle, rx: Receiver<()>) {
    recover(&app);
    let started = Instant::now();
    let mut idle_since: Option<Instant> = None;
    loop {
        let on = app.state::<AppState>().settings.lock().watch_folder || job_running(&app);
        // 꺼져 있으면 설정을 저장할 때 깨운다
        match rx.recv_timeout(if on { wait(POLL) } else { Duration::from_secs(60) }) {
            Err(RecvTimeoutError::Disconnected) => return,
            _ => while rx.try_recv().is_ok() {},
        }
        let rec = *app.state::<AppState>().rec.lock();
        // 녹음 폴더 파일을 받아 적는 동안은 쉬는 중으로 친다 (그게 끝나면 바로 다음 파일)
        if rec == RecState::Idle || job_running(&app) {
            idle_since.get_or_insert_with(Instant::now);
        } else {
            idle_since = None;
        }
        let can_start = started.elapsed() >= wait(START_DELAY) && idle_since.is_some_and(|t| t.elapsed() >= wait(IDLE_GAP));
        tick(&app, can_start);
    }
}

/// 앱이 받아 적는 사이 꺼졌다 — 받아 적던 기록을 지우고 그 파일은 다시
fn recover(app: &AppHandle) {
    let w = app.state::<Watch>();
    let active = w.inner.lock().book.active.take();
    let Some(a) = active else {
        return;
    };
    if let Some(id) = a.doc.as_deref() {
        match crate::delete_session_now(app, id) {
            Ok(()) => log::info!("지난번에 받아 적다 만 녹음 폴더 파일의 기록을 지우고 다시 받아 적음"),
            Err(e) => log::warn!("받아 적다 만 기록을 지우지 못함: {e}"),
        }
    }
    let i = w.inner.lock();
    i.book.save(&w.path);
}

fn tick(app: &AppHandle, can_start: bool) {
    let w = app.state::<Watch>();
    let st = app.state::<AppState>();
    finish_job(app);
    let (on, dir_s) = {
        let s = st.settings.lock();
        (s.watch_folder, s.watch_dir.trim().to_string())
    };
    if !on || dir_s.is_empty() {
        let stop = {
            let mut i = w.inner.lock();
            i.on = false;
            i.problem = None;
            i.waiting = 0;
            i.settle.clear();
            if i.book.armed {
                i.book.armed = false;
                i.book.save(&w.path);
                log::info!("녹음 폴더 지켜보기 끔");
            }
            // 끄면 받아 적던 파일도 멈춘다 (받아 적은 데까지는 남긴다)
            match i.job.as_mut().filter(|j| !j.ended && !j.yielded) {
                Some(j) => {
                    j.stopped = true;
                    true
                }
                None => false,
            }
        };
        if stop {
            crate::stop_recording_inner(app);
        }
        return;
    }
    let dir = PathBuf::from(&dir_s);
    let found = match scan(&dir) {
        Ok(f) => f,
        Err(e) => {
            let mut i = w.inner.lock();
            i.on = true;
            i.problem = Some(if dir.exists() {
                format!("폴더를 읽지 못했어요 ({e})")
            } else {
                "폴더를 찾을 수 없어요 — 폴더가 옮겨졌거나 OneDrive 가 꺼져 있는지 확인해 주세요".into()
            });
            return;
        }
    };
    let now = Instant::now();
    // 다른 잠금(설정 · 녹음 상태)은 이 잠금을 쥐기 전에 본다 — 화면의 get_state 는 설정을 쥔 채 이 잠금을 잡는다
    let any_todo = {
        let i = w.inner.lock();
        found.iter().any(|f| i.book.wants(&f.key()))
    };
    let engine = if any_todo { crate::engine_problem(app).map(|e| format!("받아 적지 못하고 기다리는 중 — {e}")) } else { None };
    let busy = st.quit_after_stop.load(Ordering::SeqCst) || *st.rec.lock() != RecState::Idle;
    let mut i = w.inner.lock();
    i.on = true;
    if !i.book.armed || !same_dir(&i.book.dir, &dir_s) {
        let n = i.book.arm(&dir_s, &found);
        i.settle.clear();
        i.cooldown.clear();
        i.problem = None;
        i.waiting = 0;
        i.book.save(&w.path);
        log::info!("녹음 폴더 지켜보기 시작: 지금 있는 녹음 파일 {n}개는 건너뛰고 새 파일만 받아 적음");
        let _ = app.emit("bd://watch", serde_json::json!({ "kind": "armed", "skipped": n }));
        return;
    }
    // 받아 적는 중인 파일은 기다리는 파일로 세지 않는다
    let busy_key = i.job.as_ref().map(|j| j.key.clone());
    let todo: Vec<&Found> = found
        .iter()
        .filter(|f| i.book.wants(&f.key()) && busy_key.as_deref() != Some(f.key().as_str()))
        .collect();
    i.waiting = todo.len();
    let mut ready = i.settle.settled(&todo, now, wait(STABLE_FOR));
    ready.retain(|f| i.cooldown.get(&f.key()).is_none_or(|t| now >= *t));
    // 오래된 녹음부터
    ready.sort_by_key(|f| f.modified);
    i.problem = None;
    if ready.is_empty() {
        return;
    }
    // 설정 문제(모델 · API 키 없음)는 파일 탓이 아니다 — 실패로 세지 않고 고칠 때까지 기다린다
    i.problem = engine;
    if !can_start || busy || i.job.is_some() || i.hold || i.problem.is_some() {
        return;
    }
    let Some(f) = ready.into_iter().find(|f| writer_gone(&f.path)).cloned() else {
        return;
    };
    i.job = Some(Job { key: f.key(), name: f.name.clone(), ..Default::default() });
    w.busy.store(true, Ordering::SeqCst);
    drop(i);
    start_job(app, &f);
}

fn start_job(app: &AppHandle, f: &Found) {
    let w = app.state::<Watch>();
    let st = app.state::<AppState>();
    log::info!("녹음 폴더의 새 파일을 받아 적음: {} ({}KB)", f.name, f.size / 1024);
    let _ = app.emit("bd://watch", serde_json::json!({ "kind": "start", "file": f.name }));
    match crate::start_session(app, Some(&f.path), None) {
        Ok(()) => {
            let id = st.store.lock().current.as_ref().map(|d| d.id.clone());
            let mut i = w.inner.lock();
            let Some(j) = i.job.as_mut() else {
                return;
            };
            if j.doc.is_none() {
                j.doc = id;
            }
            let active = Active { key: j.key.clone(), doc: j.doc.clone() };
            i.book.active = Some(active);
            i.book.save(&w.path);
        }
        Err(e) => {
            w.busy.store(false, Ordering::SeqCst);
            let mut i = w.inner.lock();
            i.job = None;
            // 그 사이 사람이 녹음을 시작했다 — 파일 탓이 아니다
            if *st.rec.lock() != RecState::Idle {
                return;
            }
            let key = f.key();
            let tries = i.book.tries(&key) + 1;
            let state = if tries >= MAX_TRIES { FileState::Failed } else { FileState::Retry };
            i.book.set(&key, &f.name, state, None, tries);
            i.cooldown.insert(key, Instant::now() + RETRY_AFTER);
            i.book.save(&w.path);
            log::warn!("녹음 폴더 파일을 받아 적지 못함 ({tries}번째): {e}");
            let kind = if state == FileState::Failed { "failed" } else { "retry" };
            let _ = app.emit("bd://watch", serde_json::json!({ "kind": kind, "file": f.name, "message": e }));
        }
    }
}

/// 끝난 받아쓰기를 정리한다 — 기억에 남기고, 다시 할 것은 받아 적던 기록을 지우고, 다 했으면 요약
fn finish_job(app: &AppHandle) {
    let w = app.state::<Watch>();
    let job = {
        let mut i = w.inner.lock();
        match i.job.as_ref() {
            Some(j) if j.ended => i.job.take(),
            _ => None,
        }
    };
    let Some(job) = job else {
        return;
    };
    let saved = job
        .doc
        .as_deref()
        .is_some_and(|id| app.state::<AppState>().store.lock().load(id).is_ok());
    let failure = job.failure();
    let (tries, out) = {
        let i = w.inner.lock();
        let tries = i.book.tries(&job.key) + u32::from(failure.is_some() && !job.yielded && !job.stopped);
        (tries, outcome(job.yielded, job.stopped, failure.is_some(), saved, tries))
    };
    log::info!("녹음 폴더 파일 받아쓰기 끝: {out:?}");
    // 다시 할 파일은 받아 적던 기록을 지운다 (다음에 처음부터 받아 적으면 두 개가 되지 않게)
    if matches!(out, Outcome::Requeue | Outcome::Retry) {
        if let Some(id) = job.doc.as_deref().filter(|_| saved) {
            match crate::delete_session_now(app, id) {
                Ok(()) => {
                    let _ = app.emit("bd://watch", serde_json::json!({ "kind": "removed", "id": id }));
                }
                Err(e) => log::warn!("받아 적다 만 기록을 지우지 못함: {e}"),
            }
        }
    }
    let summarize = app.state::<AppState>().settings.lock().watch_summarize;
    {
        let mut i = w.inner.lock();
        i.book.active = None;
        match out {
            Outcome::Requeue => {}
            Outcome::Retry => {
                i.book.set(&job.key, &job.name, FileState::Retry, None, tries);
                i.cooldown.insert(job.key.clone(), Instant::now() + RETRY_AFTER);
            }
            Outcome::Failed => i.book.set(&job.key, &job.name, FileState::Failed, job.doc.clone().filter(|_| saved), tries),
            Outcome::Stopped => i.book.set(&job.key, &job.name, FileState::Stopped, job.doc.clone().filter(|_| saved), tries),
            Outcome::Done => i.book.set(&job.key, &job.name, FileState::Done, job.doc.clone(), tries),
            Outcome::Empty => i.book.set(&job.key, &job.name, FileState::Empty, None, tries),
        }
        i.book.save(&w.path);
    }
    let kept = job.doc.as_ref().filter(|_| saved);
    let ev = match out {
        Outcome::Requeue => serde_json::json!({ "kind": "requeued", "file": job.name }),
        Outcome::Retry => serde_json::json!({ "kind": "retry", "file": job.name, "message": failure }),
        Outcome::Failed => serde_json::json!({ "kind": "failed", "file": job.name, "message": failure, "id": kept }),
        Outcome::Stopped => serde_json::json!({ "kind": "stopped", "file": job.name, "id": kept }),
        Outcome::Empty => serde_json::json!({ "kind": "empty", "file": job.name }),
        Outcome::Done => serde_json::json!({ "kind": "done", "file": job.name, "id": job.doc, "summarize": summarize }),
    };
    let _ = app.emit("bd://watch", ev);
    if out == Outcome::Done && summarize {
        if let Some(id) = job.doc {
            summarize_later(app.clone(), id, job.name);
        }
    }
}

/// 받아 적은 기록을 요약한다 (설정 › 요약의 AI 로) — 따로 돌려 다음 파일을 기다리게 하지 않는다
fn summarize_later(app: AppHandle, id: String, name: String) {
    let _ = std::thread::Builder::new().name("bd-watch-sum".into()).spawn(move || {
        let token = SUMMARY_TOKEN_BASE + app.state::<Watch>().tokens.fetch_add(1, Ordering::SeqCst);
        let _ = app.emit("bd://watch", serde_json::json!({ "kind": "summarizing", "id": id, "file": name }));
        match tauri::async_runtime::block_on(crate::summarize_doc(&app, id.clone(), token, None, true)) {
            Ok(r) => {
                log::info!("녹음 폴더 파일 요약 끝");
                let _ = app.emit(
                    "bd://watch",
                    serde_json::json!({ "kind": "summarized", "id": id, "file": name, "text": r.text, "title": r.title, "template": r.template }),
                );
            }
            Err(e) if e == crate::SUMMARY_KEPT => {
                log::info!("녹음 폴더 파일 자동 요약은 넣지 않음 — 그 사이 요약이 생김");
                let _ = app.emit("bd://watch", serde_json::json!({ "kind": "summaryKept", "id": id, "file": name }));
            }
            Err(e) => {
                log::warn!("녹음 폴더 파일 요약 실패: {e}");
                let _ = app.emit("bd://watch", serde_json::json!({ "kind": "summaryFailed", "id": id, "file": name, "message": e }));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, size: u64, secs: u64) -> Found {
        Found {
            path: PathBuf::from(format!(r"C:\rec\{name}")),
            name: name.into(),
            size,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
        }
    }

    #[test]
    fn only_audio_files_that_are_not_temp() {
        assert!(is_audio_name("회의 녹음 1.m4a"));
        assert!(is_audio_name("REC_0001.MP3"));
        assert!(is_audio_name("통화.wav"));
        assert!(!is_audio_name("메모.txt"));
        assert!(!is_audio_name("회의.m4a.tmp"));
        assert!(!is_audio_name("~$회의.m4a"));
        assert!(!is_audio_name(".회의.m4a"));
        assert!(!is_audio_name("확장자없음"));
    }

    #[test]
    fn same_folder_ignores_case_and_trailing_slash() {
        assert!(same_dir(r"C:\Users\me\OneDrive\녹음", r"c:\users\me\onedrive\녹음\"));
        assert!(same_dir("D:/rec", r"D:\rec"));
        assert!(!same_dir(r"D:\rec", r"D:\rec2"));
        assert!(!same_dir("", r"D:\rec"));
    }

    #[test]
    fn arming_skips_what_is_already_there() {
        let mut b = Book::default();
        let old = [f("a.m4a", 100, 1), f("b.mp3", 200, 2)];
        assert_eq!(b.arm(r"D:\rec", &old), 2);
        assert!(b.armed);
        assert!(!b.wants(&old[0].key()));
        // 새로 생긴 파일 · 같은 이름이라도 크기가 다른 파일(다시 녹음함)은 받아 적는다
        assert!(b.wants(&f("c.m4a", 300, 3).key()));
        assert!(b.wants(&f("a.m4a", 150, 3).key()));
        // 이름은 대소문자를 가리지 않는다
        assert!(!b.wants(&f("A.M4A", 100, 1).key()));
        // 받아 적은 파일은 같은 폴더를 다시 켜도 그대로 기억한다
        b.set(&f("c.m4a", 300, 3).key(), "c.m4a", FileState::Done, Some("x".into()), 0);
        b.armed = false;
        assert_eq!(b.arm(r"d:\REC\", &[f("c.m4a", 300, 3), f("d.m4a", 1, 4)]), 1);
        assert_eq!(b.files[&f("c.m4a", 300, 3).key()].state, FileState::Done);
        // 다른 폴더면 처음부터
        assert_eq!(b.arm(r"E:\other", &[f("a.m4a", 100, 1)]), 1);
        assert_eq!(b.files.len(), 1);
    }

    #[test]
    fn retries_until_the_third_failure() {
        let mut b = Book::default();
        let k = f("a.m4a", 100, 1).key();
        b.set(&k, "a.m4a", FileState::Retry, None, 1);
        assert!(b.wants(&k));
        b.set(&k, "a.m4a", FileState::Retry, None, 2);
        assert!(b.wants(&k));
        b.set(&k, "a.m4a", FileState::Failed, None, 3);
        assert!(!b.wants(&k));
        for s in [FileState::Done, FileState::Empty, FileState::Stopped, FileState::Skip] {
            b.set(&k, "a.m4a", s, None, 0);
            assert!(!b.wants(&k), "{s:?}");
        }
    }

    #[test]
    fn waits_until_size_stops_changing() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut s = Settle::default();
        // 동기화 중: 크기가 계속 자란다
        let a1 = f("a.m4a", 100, 10);
        assert!(s.settled(&[&a1], at(0), STABLE_FOR).is_empty());
        let a2 = f("a.m4a", 500, 15);
        assert!(s.settled(&[&a2], at(5), STABLE_FOR).is_empty());
        assert!(s.settled(&[&a2], at(10), STABLE_FOR).is_empty());
        // 15초 넘게 그대로면 다 쓴 파일
        assert!(s.settled(&[&a2], at(19), STABLE_FOR).is_empty());
        assert_eq!(s.settled(&[&a2], at(20), STABLE_FOR), vec![&a2]);
        // 수정 시각만 바뀌어도 처음부터
        let a3 = f("a.m4a", 500, 40);
        assert!(s.settled(&[&a3], at(25), STABLE_FOR).is_empty());
        // 빈 파일은 아직 쓰기 전
        let e = f("e.m4a", 0, 1);
        assert!(s.settled(&[&e], at(30), STABLE_FOR).is_empty());
        assert!(s.settled(&[&e], at(60), STABLE_FOR).is_empty());
        // 사라진 파일은 잊는다
        s.settled(&[], at(61), STABLE_FOR);
        assert!(s.seen.is_empty());
    }

    #[test]
    fn what_to_do_after_a_file() {
        // 녹음에 비켜 줌 → 다시 (오류 · 멈춤보다 먼저)
        assert_eq!(outcome(true, false, true, true, 1), Outcome::Requeue);
        assert_eq!(outcome(false, true, false, true, 0), Outcome::Stopped);
        assert_eq!(outcome(false, false, true, true, 1), Outcome::Retry);
        assert_eq!(outcome(false, false, true, true, 2), Outcome::Retry);
        assert_eq!(outcome(false, false, true, true, 3), Outcome::Failed);
        assert_eq!(outcome(false, false, false, true, 0), Outcome::Done);
        assert_eq!(outcome(false, false, false, false, 0), Outcome::Empty);
    }

    #[test]
    fn many_failed_sentences_count_as_an_error() {
        assert!(!failed_too_many(30, 0));
        // 한두 문장 실패는 그대로 받아 적은 것으로
        assert!(!failed_too_many(30, 3));
        assert!(failed_too_many(30, 4));
        // 잘못된 API 키: 모든 문장이 실패
        assert!(failed_too_many(0, 5));
        let j = Job { ok: 0, failed: 5, fail_msg: Some("HTTP 401".into()), ..Default::default() };
        assert!(j.failure().is_some_and(|m| m.contains("401")));
        let j = Job { ok: 40, failed: 1, ..Default::default() };
        assert_eq!(j.failure(), None);
    }

    #[test]
    fn book_survives_a_round_trip() {
        let dir = std::env::temp_dir().join(format!("bd-watch-{}", std::process::id()));
        let path = dir.join("watch.json");
        let mut b = Book::default();
        b.arm(r"D:\rec", &[f("a.m4a", 100, 1)]);
        b.set("b.m4a|5", "b.m4a", FileState::Done, Some("2026-11-20_151000".into()), 0);
        b.active = Some(Active { key: "c.m4a|7".into(), doc: Some("2026-11-20_152000".into()) });
        b.save(&path);
        let r = Book::load(&path);
        assert!(r.armed);
        assert_eq!(r.files.len(), 2);
        assert_eq!(r.files["b.m4a|5"].state, FileState::Done);
        assert_eq!(r.active, b.active);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_counts_what_was_done() {
        let w = Watch::load(std::env::temp_dir().join("bd-watch-none").join("watch.json"));
        {
            let mut i = w.inner.lock();
            i.book.arm(r"D:\rec", &[f("a.m4a", 1, 1), f("b.m4a", 2, 1)]);
            i.book.set("c|1", "c.m4a", FileState::Done, Some("1".into()), 0);
            i.book.set("d|1", "d.m4a", FileState::Empty, None, 0);
            i.book.set("e|1", "e.m4a", FileState::Stopped, Some("2".into()), 0);
            i.book.set("f|1", "f.m4a", FileState::Failed, None, 3);
            i.job = Some(Job { name: "g.m4a".into(), ..Default::default() });
        }
        let s = w.status();
        assert_eq!((s.done, s.skipped, s.stopped, s.failed), (2, 2, 1, 1));
        assert_eq!(s.working.as_deref(), Some("g.m4a"));
        assert_eq!(s.last.map(|e| e.name), Some("c.m4a".into()));
    }
}
