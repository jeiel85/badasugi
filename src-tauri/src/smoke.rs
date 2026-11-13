//! 자동 점검 모드 (`BADASUGI_SMOKE=1`) — 릴리스 전에 실제 앱으로 녹음 · 일시정지 · 정지 · 저장 ·
//! 파일 받아쓰기를 돌려 보고 결과를 검사한다. (`scripts/smoke.ps1` 이 실행)
//!
//! - 사용 중인 앱 · 설정 · 회의 기록을 건드리지 않는다: `BADASUGI_HOME` 임시 폴더에 설정 · 기록 · 로그를 두고,
//!   같은 앱 하나만 실행 규칙 · 트레이 · 단축키 · 업데이트는 끈다. 모델은 설치된 것을 읽기만 한다.
//! - 마이크 대신 `BADASUGI_TEST_WAV` 를 실시간 속도로 흘려 넣는다 (소리를 내거나 방 안 소리를 녹음하지 않음)
//! - `BADASUGI_SMOKE_EXPECT` (| 로 구분) 의 낱말이 받아 적혀야 통과
//! - 결과: `<BADASUGI_HOME>/smoke-report.json`, 종료 코드 0 = 통과 / 1 = 실패

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::session::{SessionDoc, SessionStore};
use crate::{AppState, RecState};

pub fn enabled() -> bool {
    std::env::var_os("BADASUGI_SMOKE").is_some()
}

/// 점검용 임시 폴더 (점검 모드일 때만)
pub fn home() -> Option<PathBuf> {
    enabled().then(|| std::env::var_os("BADASUGI_HOME").map(PathBuf::from)).flatten()
}

#[derive(Serialize)]
struct Check {
    name: String,
    ok: bool,
    detail: String,
}

struct Checks(Vec<Check>);

impl Checks {
    fn add(&mut self, name: &str, ok: bool, detail: impl Into<String>) -> bool {
        let detail = detail.into();
        log::info!("[점검] {} {name} — {detail}", if ok { "통과" } else { "실패" });
        self.0.push(Check {
            name: name.into(),
            ok,
            detail,
        });
        ok
    }
}

pub fn spawn(app: AppHandle) {
    let _ = std::thread::Builder::new().name("bd-smoke".into()).spawn(move || {
        let mut c = Checks(Vec::new());
        run(&app, &mut c);
        let ok = !c.0.is_empty() && c.0.iter().all(|x| x.ok);
        let report = serde_json::json!({
            "ok": ok,
            "version": app.package_info().version.to_string(),
            "checks": c.0,
        });
        if let Some(home) = home() {
            let _ = std::fs::write(home.join("smoke-report.json"), serde_json::to_vec_pretty(&report).unwrap_or_default());
        }
        log::info!("[점검] 끝: {}", if ok { "모두 통과" } else { "실패 있음" });
        log::logger().flush();
        app.exit(if ok { 0 } else { 1 });
    });
}

fn wait_idle(app: &AppHandle, timeout: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if *app.state::<AppState>().rec.lock() == RecState::Idle {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// 방금 저장된 기록 (id 가 가장 큰 것)
fn latest(dir: &Path, skip: Option<&str>) -> Option<SessionDoc> {
    let store = SessionStore::new(dir.to_path_buf());
    let meta = store.list().into_iter().find(|m| Some(m.id.as_str()) != skip)?;
    store.load(&meta.id).ok()
}

fn check_doc(c: &mut Checks, label: &str, doc: &SessionDoc, expect: &[String]) {
    let n = doc.utterances.len();
    c.add(&format!("{label}: 받아 적힘"), n > 0, format!("발화 {n}개"));
    let sorted = doc.utterances.windows(2).all(|w| w[0].start_ms <= w[1].start_ms);
    c.add(&format!("{label}: 시간 순서"), sorted, if sorted { "정렬됨" } else { "순서가 뒤섞임" });
    let all: String = doc.utterances.iter().map(|u| u.text.as_str()).collect::<Vec<_>>().join(" ");
    let missing: Vec<&str> = expect.iter().map(String::as_str).filter(|w| !all.contains(w)).collect();
    c.add(
        &format!("{label}: 인식 내용"),
        missing.is_empty(),
        if missing.is_empty() {
            format!("기대한 낱말 {}개 모두 있음", expect.len())
        } else {
            format!("빠진 낱말: {}", missing.join(", "))
        },
    );
}

fn run(app: &AppHandle, c: &mut Checks) {
    let expect: Vec<String> = std::env::var("BADASUGI_SMOKE_EXPECT")
        .unwrap_or_default()
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let Some(wav) = std::env::var_os("BADASUGI_TEST_WAV").map(PathBuf::from) else {
        c.add("준비", false, "BADASUGI_TEST_WAV 가 없어요");
        return;
    };
    let wav_secs = match badasugi_core::wav::read_16k_mono(&wav) {
        Ok(s) => s.len() as f64 / badasugi_core::SAMPLE_RATE as f64,
        Err(e) => {
            c.add("준비", false, format!("테스트 음성을 읽을 수 없어요: {e}"));
            return;
        }
    };
    let sessions = app.state::<AppState>().paths.sessions.clone();

    // ── 0. 화면: 스크립트 오류가 있으면 화면이 비고 이 신호가 오지 않는다 ──
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(30) && !app.state::<AppState>().ui_ready.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let ready = app.state::<AppState>().ui_ready.load(std::sync::atomic::Ordering::SeqCst);
    c.add(
        "화면 표시",
        ready,
        if ready {
            format!("{:.1}초 만에 준비됨", t.elapsed().as_secs_f64())
        } else {
            "30초 안에 화면이 준비되지 않았어요 (화면 스크립트 오류 가능성)".into()
        },
    );
    std::thread::sleep(Duration::from_secs(1));

    // ── 1. 실시간 녹음: 시작 → 일시정지 → 이어서 → 정지 → 저장 ──
    let t0 = Instant::now();
    if !c.add("녹음 시작", crate::start_recording_inner(app).is_ok(), "테스트 음성을 마이크 대신") {
        return;
    }
    std::thread::sleep(Duration::from_secs(4));
    c.add("일시정지", crate::set_paused_inner(app, true).is_ok(), "4초 지점");
    std::thread::sleep(Duration::from_secs(2));
    c.add("이어서 녹음", crate::set_paused_inner(app, false).is_ok(), "6초 지점");
    let rest = (wav_secs + 1.5 - t0.elapsed().as_secs_f64()).max(0.0);
    std::thread::sleep(Duration::from_secs_f64(rest));
    let rec_secs = t0.elapsed().as_secs_f64();
    crate::stop_recording_inner(app);
    let t_stop = Instant::now();
    if !c.add(
        "정지 · 정리",
        wait_idle(app, Duration::from_secs(300)),
        format!("남은 인식까지 {:.0}초", t_stop.elapsed().as_secs_f64()),
    ) {
        return;
    }
    let Some(live) = latest(&sessions, None) else {
        c.add("녹음 기록 저장", false, "기록 파일이 없어요");
        return;
    };
    c.add("녹음 기록 저장", sessions.join(format!("{}.md", live.id)).is_file(), format!("{}.json · .md", live.id));
    // 일시정지 구간(4~6초)의 말은 빠질 수 있으니 그 밖의 낱말만 기대한다 — 점검 스크립트가 고른다
    check_doc(c, "녹음", &live, &expect);
    match live.audio_file.as_deref().map(|f| sessions.join(f)) {
        Some(p) => match badasugi_core::wav::read_16k_mono(&p) {
            Ok(s) => {
                let secs = s.len() as f64 / badasugi_core::SAMPLE_RATE as f64;
                c.add(
                    "녹음 파일(WAV)",
                    (secs - rec_secs).abs() < 4.0,
                    format!("{secs:.1}초 (녹음 {rec_secs:.1}초)"),
                );
            }
            Err(e) => {
                c.add("녹음 파일(WAV)", false, format!("읽을 수 없어요: {e}"));
            }
        },
        None => {
            c.add("녹음 파일(WAV)", false, "기록에 녹음 파일이 없어요");
        }
    }

    // ── 2. 파일 받아쓰기 ──
    let file = std::env::var_os("BADASUGI_SMOKE_FILE").map(PathBuf::from).unwrap_or(wav);
    let t1 = Instant::now();
    if !c.add("파일 받아쓰기 시작", crate::start_session(app, Some(&file)).is_ok(), file.display().to_string()) {
        return;
    }
    if !c.add(
        "파일 받아쓰기 끝",
        wait_idle(app, Duration::from_secs(400)),
        format!("{:.0}초 걸림", t1.elapsed().as_secs_f64()),
    ) {
        return;
    }
    match latest(&sessions, Some(&live.id)) {
        Some(doc) => {
            c.add("파일 기록 저장", doc.source_file.is_some(), doc.source_file.clone().unwrap_or_default());
            check_doc(c, "파일", &doc, &expect);
        }
        None => {
            c.add("파일 기록 저장", false, "기록 파일이 없어요");
        }
    }

    // ── 3. 진단 로그가 남는지 ──
    let logs = crate::logging::log_dir();
    let has_log = std::fs::read_dir(&logs)
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("badasugi-")))
        .unwrap_or(false);
    c.add("진단 로그", has_log, logs.display().to_string());
}
