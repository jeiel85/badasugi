//! 자동 점검 모드 (`BADASUGI_SMOKE=1`) — 릴리스 전에 실제 앱으로 녹음 · 일시정지 · 정지 · 저장 ·
//! 파일 받아쓰기를 돌려 보고 결과를 검사한다. (`scripts/smoke.ps1` 이 실행)
//!
//! - 사용 중인 앱 · 설정 · 회의 기록을 건드리지 않는다: `BADASUGI_HOME` 임시 폴더에 설정 · 기록 · 로그를 두고,
//!   같은 앱 하나만 실행 규칙 · 트레이 · 단축키 · 업데이트는 끈다. 모델은 설치된 것을 읽기만 한다.
//! - 마이크 대신 `BADASUGI_TEST_WAV` 를 실시간 속도로 흘려 넣는다 (소리를 내거나 방 안 소리를 녹음하지 않음)
//! - `BADASUGI_SMOKE_EXPECT` (| 로 구분) 의 낱말이 받아 적혀야 통과
//! - 결과: `<BADASUGI_HOME>/smoke-report.json`, 종료 코드 0 = 통과 / 1 = 실패

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
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

/// 화면이 녹음 파일을 읽어 본 결과 (`smoke_probe` 로 돌아온다)
#[derive(Deserialize)]
pub struct Probe {
    ok: bool,
    /// 초. 화면이 길이를 모르면 -1
    duration: f64,
    detail: String,
}

static PROBE: Mutex<Option<Probe>> = Mutex::new(None);

/// 화면이 녹음 파일을 읽어 본 결과를 알려 준다 (점검 모드에서만 받는다)
#[tauri::command]
pub fn smoke_probe(result: Probe) {
    if enabled() {
        *PROBE.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    }
}

/// 화면(WebView2)이 기록의 녹음 파일을 다시 듣기와 같은 길(session_audio → asset 프로토콜 → <audio>)로
/// 실제로 열어 길이(초)를 알아내는지 본다.
fn probe_playback(app: &AppHandle, id: &str) -> Result<f64, String> {
    let w = app.get_webview_window("main").ok_or("창이 없어요")?;
    *PROBE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let js = format!(
        r#"(async () => {{
  const T = window.__TAURI__;
  let r;
  try {{
    const p = await T.core.invoke('session_audio', {{ id: {id} }});
    if (!p) throw new Error('녹음 파일 경로가 없어요');
    const a = new Audio();
    a.preload = 'metadata';
    const d = await new Promise((ok, no) => {{
      const to = setTimeout(() => no(new Error('10초 안에 열지 못했어요')), 10000);
      a.onloadedmetadata = () => {{ clearTimeout(to); ok(a.duration); }};
      a.onerror = () => {{ clearTimeout(to); no(new Error('재생 오류 ' + (a.error ? a.error.code + ' ' + a.error.message : ''))); }};
      a.src = T.core.convertFileSrc(p) + '?v=' + Date.now();
    }});
    a.removeAttribute('src');
    const name = p.slice(Math.max(p.lastIndexOf('/'), p.lastIndexOf(String.fromCharCode(92))) + 1);
    r = {{ ok: true, duration: Number.isFinite(d) ? d : -1, detail: name }};
  }} catch (e) {{
    r = {{ ok: false, duration: 0, detail: String((e && e.message) || e) }};
  }}
  await T.core.invoke('smoke_probe', {{ result: r }});
}})();"#,
        id = serde_json::to_string(id).unwrap_or_default()
    );
    w.eval(&js).map_err(|e| e.to_string())?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(15) {
        let got = PROBE.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(p) = got {
            return if p.ok && p.duration >= 0.0 {
                Ok(p.duration)
            } else if p.ok {
                Err(format!("길이를 알 수 없어요 ({})", p.detail))
            } else {
                Err(p.detail)
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("화면이 응답하지 않아요".into())
}

/// 화면의 PDF 저장(app.js `exportPdf` — 저장 메뉴의 PDF 와 같은 함수)으로 기록을 PDF 로 내고 파일 내용을 돌려준다
fn probe_pdf(app: &AppHandle, id: &str, path: &Path) -> Result<Vec<u8>, String> {
    let w = app.get_webview_window("main").ok_or("창이 없어요")?;
    *PROBE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let js = format!(
        r#"(async () => {{
  let r;
  try {{
    await exportPdf({{ id: {id}, title: '점검 회의' }}, {path});
    const left = document.body.classList.contains('printing') || document.getElementById('print-doc').innerHTML !== '';
    r = left ? {{ ok: false, duration: 0, detail: '인쇄용 칸이 비워지지 않았어요' }} : {{ ok: true, duration: 0, detail: '' }};
  }} catch (e) {{
    r = {{ ok: false, duration: 0, detail: String((e && e.message) || e) }};
  }}
  await window.__TAURI__.core.invoke('smoke_probe', {{ result: r }});
}})();"#,
        id = serde_json::to_string(id).unwrap_or_default(),
        path = serde_json::to_string(&path.to_string_lossy()).unwrap_or_default()
    );
    w.eval(&js).map_err(|e| e.to_string())?;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(60) {
        let got = PROBE.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(p) = got {
            return if p.ok { std::fs::read(path).map_err(|e| format!("PDF 파일을 읽을 수 없어요: {e}")) } else { Err(p.detail) };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("화면이 응답하지 않아요".into())
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

/// 녹음 파일을 끝까지 디코딩 (16 kHz 모노)
fn read_all(p: &Path) -> anyhow::Result<Vec<f32>> {
    let mut f = badasugi_core::decode::AudioFile::open(p)?;
    let mut out = Vec::new();
    while f.read(&mut out)? {}
    Ok(out)
}

/// 말로 입력: 단축키를 누른 채 말하고 떼면 받아 적어 넣을 글이 나온다 (점검 모드는 넣지 않고 남긴다).
/// 회의 녹음이 마이크를 쓰는 동안은 쉰다
fn dictate_check(app: &AppHandle, c: &mut Checks, expect_first: Option<&str>) {
    use crate::dictate::{self, Dictate, View};
    let d = app.state::<Dictate>();
    *d.smoke_out.lock() = None;
    // 테스트 음성을 마이크 대신 — 처음 5초 남짓을 누른 채 말한 것처럼
    dictate::press(app);
    std::thread::sleep(Duration::from_millis(5500));
    dictate::release(app);
    let got = wait_for(Duration::from_secs(120), || d.smoke_out.lock().is_some());
    let text = d.smoke_out.lock().clone().unwrap_or_default();
    let has = expect_first.is_none_or(|w| text.contains(w));
    c.add(
        "말로 입력: 누른 채 말하고 떼면 글",
        got && !text.trim().is_empty() && has,
        if got { format!("{}자{}", text.chars().count(), expect_first.map(|w| if has { format!(" · '{w}' 있음") } else { format!(" · '{w}' 없음") }).unwrap_or_default()) } else { "2분 안에 글이 나오지 않았어요".into() },
    );
    // 마이크를 함께 쓰지 않는다: 톡 눌러 듣는 중에 회의 녹음을 시작하면 말로 입력을 멈추고 들은 말은 넣지 않는다
    *d.smoke_out.lock() = None;
    dictate::press(app);
    std::thread::sleep(Duration::from_millis(150));
    dictate::release(app);
    std::thread::sleep(Duration::from_millis(1500));
    if crate::start_recording_inner(app).is_ok() {
        let stopped = wait_for(Duration::from_secs(60), || matches!(d.view(), View::Error { ref message } if message.contains("멈췄어요")));
        c.add(
            "말로 입력: 듣는 중 회의 녹음을 시작하면 멈추기",
            stopped && d.smoke_out.lock().is_none(),
            format!("{:?}", d.view()),
        );
        // 회의 녹음 중에는 쉰다
        std::thread::sleep(Duration::from_secs(1));
        dictate::press(app);
        let busy = wait_for(Duration::from_secs(5), || matches!(d.view(), View::Error { ref message } if message.contains("쉬어요")));
        dictate::release(app);
        c.add("말로 입력: 회의 녹음 중에는 쉬기", busy, format!("{:?}", d.view()));
        crate::stop_recording_inner(app);
        wait_idle(app, Duration::from_secs(300));
    }
}

/// Outlook 이 지금 떠 있나 (작업 관리자의 OUTLOOK.EXE)
fn outlook_running() -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq OUTLOOK.EXE", "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("outlook.exe"))
        .unwrap_or(false)
}

/// 일정에서 가져오기: 꺼져 있는 Outlook 을 띄우지 않는다 (Outlook 이 켜져 있으면 읽기만 본다)
fn calendar_check(app: &AppHandle, c: &mut Checks) {
    let before = outlook_running();
    let st = app.state::<crate::calendar::Calendar>().run(crate::calendar::read_now);
    std::thread::sleep(Duration::from_secs(3));
    let after = outlook_running();
    c.add(
        "일정: Outlook 을 띄우지 않음",
        before || !after,
        format!("{st:?} · Outlook 실행 {before} → {after}"),
    );
}

/// 조건이 맞을 때까지 기다린다 (시간 안에 맞으면 true)
fn wait_for(timeout: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < timeout {
        if ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    ok()
}

/// 녹음 폴더 지켜보기: 켤 때 있던 파일은 건너뛰고, 새 파일은 받아 적고, 받아 적는 중에 녹음을 시작하면 녹음이 먼저 —
/// 그 파일은 녹음 뒤 처음부터 다시 받아 적어 기록이 하나만 남는다. (요약은 AI 없이 끈다)
fn watch_check(app: &AppHandle, c: &mut Checks, src: &Path, sessions: &Path) {
    use crate::watch::{self, Watch};
    let Some(home) = home() else {
        return;
    };
    let dir = home.join("watch");
    let _ = std::fs::create_dir_all(&dir);
    // 테스트 음성 앞 12초만 — 받아쓰기를 세 번 하므로 점검이 너무 길어지지 않게 (받아 적는 중에 녹음을 시작할 틈은 남긴다)
    let clip = match badasugi_core::wav::read_16k_mono(src)
        .and_then(|s| badasugi_core::wav::encode_pcm16(&s[..s.len().min(badasugi_core::SAMPLE_RATE as usize * 12)]))
    {
        Ok(b) => b,
        Err(e) => {
            c.add("녹음 폴더: 준비", false, format!("테스트 음성을 자르지 못함: {e}"));
            return;
        }
    };
    let name = |n: &str| format!("{n}.wav");
    let copy = |n: &str| std::fs::write(dir.join(name(n)), &clip).is_ok();
    copy("켜기 전 녹음");
    {
        let st = app.state::<AppState>();
        let mut s = st.settings.lock();
        s.watch_folder = true;
        s.watch_dir = dir.display().to_string();
        s.watch_summarize = false;
    }
    watch::spawn(app.clone());
    watch::wake(app);
    let status = || app.state::<Watch>().status();
    let armed = wait_for(Duration::from_secs(10), || {
        let s = status();
        s.on && s.skipped == 1
    });
    if !c.add("녹음 폴더: 켤 때 있던 파일 건너뛰기", armed, format!("{:?}", status())) {
        return;
    }
    let count = |n: &str| {
        let n = name(n);
        SessionStore::new(sessions.to_path_buf()).all_docs().iter().filter(|d| d.source_file.as_deref() == Some(n.as_str())).count()
    };
    let t = Instant::now();
    copy("새 녹음 1");
    let done = wait_for(Duration::from_secs(400), || status().done >= 1);
    let (one, before) = (count("새 녹음 1"), count("켜기 전 녹음"));
    c.add(
        "녹음 폴더: 새 파일 받아 적기",
        done && one == 1 && before == 0,
        format!("{:.0}초 걸림 · 새 파일 기록 {one}개 · 켜기 전 파일 기록 {before}개", t.elapsed().as_secs_f64()),
    );
    // 다음 파일을 받아 적기 시작하면 바로 녹음을 시작한다 (단축키를 누른 것처럼)
    copy("새 녹음 2");
    let st = app.state::<AppState>();
    let started = wait_for(Duration::from_secs(120), || {
        watch::job_running(app) && *st.rec.lock() == RecState::Recording && st.pipeline.lock().is_some()
    });
    if !c.add("녹음 폴더: 다음 파일 받아쓰기 시작", started, format!("{:?}", status().working)) {
        return;
    }
    std::thread::sleep(Duration::from_secs(1));
    let was_running = watch::job_running(app);
    let t = Instant::now();
    let r = crate::toggle_recording_inner(app);
    let live = r.is_ok() && *st.rec.lock() == RecState::Recording && !st.file_mode.load(std::sync::atomic::Ordering::SeqCst);
    c.add(
        "녹음 폴더: 받아 적는 중 녹음 단축키 → 녹음이 먼저",
        live && was_running,
        match &r {
            Err(e) => format!("녹음을 시작하지 못함: {e}"),
            Ok(()) if !was_running => "받아쓰기가 먼저 끝나 비켜 주기를 확인하지 못했어요".into(),
            Ok(()) => format!("{:.1}초 만에 녹음 시작", t.elapsed().as_secs_f64()),
        },
    );
    std::thread::sleep(Duration::from_secs(3));
    crate::stop_recording_inner(app);
    if !wait_idle(app, Duration::from_secs(300)) {
        c.add("녹음 폴더: 녹음 뒤 다시 받아 적기", false, "녹음이 끝나지 않았어요");
        return;
    }
    let t = Instant::now();
    let redone = wait_for(Duration::from_secs(400), || status().done >= 2);
    let two = count("새 녹음 2");
    c.add(
        "녹음 폴더: 녹음 뒤 처음부터 다시 (기록 하나)",
        redone && two == 1,
        format!("{:.0}초 걸림 · 기록 {two}개", t.elapsed().as_secs_f64()),
    );
    st.settings.lock().watch_folder = false;
    watch::wake(app);
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

    // ── 0-2. 회의 시작 알림 창: 포커스를 빼앗지 않고 화면 안(시계 위)에 떴다가, 닫으면 없어진다 ──
    match crate::meeting::smoke_check(app) {
        Ok(d) => c.add("회의 알림 창", true, d),
        Err(e) => c.add("회의 알림 창", false, e),
    };

    // ── 1. 실시간 녹음: 시작 → 일시정지 → 이어서 → 정지 → 저장 ──
    let t0 = Instant::now();
    if !c.add("녹음 시작", crate::start_recording_inner(app).is_ok(), "테스트 음성을 마이크 대신") {
        return;
    }
    std::thread::sleep(Duration::from_secs(4));
    // 일시정지는 절전 감지 경로로: 녹음 중 PC 가 2분 잠들었다 깬 것처럼 (power.rs 감시 스레드가 알아챈 경우)
    crate::on_power(app, crate::power::PowerEvent::Resume { slept_secs: Some(120) });
    let paused = {
        let st = app.state::<AppState>();
        let p = st.pipeline.lock().as_ref().is_some_and(|p| p.is_paused());
        p && st.sleep_paused.load(std::sync::atomic::Ordering::SeqCst)
    };
    c.add("절전 감지 → 일시정지", paused, "4초 지점, 2분 잠들었다 깬 것처럼");
    std::thread::sleep(Duration::from_secs(2));
    c.add("이어서 녹음", crate::set_paused_inner(app, false).is_ok(), "6초 지점");
    // 녹음 중 다시 듣기: 자라는 녹음 파일을 화면이 그때까지의 길이로 읽어야 한다 (1초마다 디스크에 내보냄)
    std::thread::sleep(Duration::from_secs(3));
    let live_id = app.state::<AppState>().store.lock().current.as_ref().map(|d| d.id.clone());
    if let Some(id) = live_id {
        let at = t0.elapsed().as_secs_f64();
        match probe_playback(app, &id) {
            Ok(d) => c.add(
                "녹음 중 다시 듣기",
                d > 1.0 && (d - at).abs() < 3.0,
                format!("{d:.1}초까지 들림 (녹음 {at:.1}초째)"),
            ),
            Err(e) => c.add("녹음 중 다시 듣기", false, e),
        };
    } else {
        c.add("녹음 중 다시 듣기", false, "녹음 중인 기록이 없어요");
    }
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
    // GPU 배포본: 어느 장치로 받아 적었는지 — 점검 스크립트가 GPU 를 가린 판에서는 CPU 로 돌아갔어야 한다
    let dev = badasugi_core::asr::local_device();
    let want = std::env::var("BADASUGI_SMOKE_EXPECT_DEVICE").unwrap_or_default();
    // GPU 로 열었다가 인식 중에 실패해 CPU 로 바꿨으면 실패로 본다 (GPU 가 없는 PC 는 처음부터 CPU 라 해당 없음)
    let broken = badasugi_core::asr::gpu::is_broken();
    let dev_ok = !broken
        && match (want.as_str(), dev.as_deref()) {
            ("cpu", Some(d)) => d == "CPU",
            ("gpu", Some(d)) => d.starts_with("GPU"),
            (_, d) => d.is_some(),
        };
    c.add(
        "음성 인식 장치",
        dev_ok,
        format!(
            "{}{}{}",
            dev.as_deref().unwrap_or("모름"),
            if want.is_empty() { String::new() } else { format!(" (기대: {want})") },
            if broken { " — GPU 에서 실패해 CPU 로 바뀜 (로그 확인)" } else { "" }
        ),
    );
    let Some(live) = latest(&sessions, None) else {
        c.add("녹음 기록 저장", false, "기록 파일이 없어요");
        return;
    };
    c.add("녹음 기록 저장", sessions.join(format!("{}.md", live.id)).is_file(), format!("{}.json · .md", live.id));
    // 일시정지 구간(4~6초)의 말은 빠질 수 있으니 그 밖의 낱말만 기대한다 — 점검 스크립트가 고른다
    check_doc(c, "녹음", &live, &expect);
    match live.audio_file.as_deref().map(|f| sessions.join(f)) {
        Some(p) => match read_all(&p) {
            Ok(s) => {
                let secs = s.len() as f64 / badasugi_core::SAMPLE_RATE as f64;
                let kb = std::fs::metadata(&p).map(|m| m.len() / 1024).unwrap_or(0);
                // Opus 24 kbps ≈ 3 KB/s (WAV 면 31 KB/s)
                let small = (kb as f64) < secs.max(1.0) * 6.0;
                c.add(
                    "녹음 파일(Opus)",
                    p.extension().is_some_and(|e| e == "opus") && small && (secs - rec_secs).abs() < 4.0,
                    format!("{secs:.1}초 (녹음 {rec_secs:.1}초) · {kb} KB"),
                );
            }
            Err(e) => {
                c.add("녹음 파일(Opus)", false, format!("읽을 수 없어요: {e}"));
            }
        },
        None => {
            c.add("녹음 파일(Opus)", false, "기록에 녹음 파일이 없어요");
        }
    }
    match probe_playback(app, &live.id) {
        Ok(d) => c.add(
            "다시 듣기",
            (d - rec_secs).abs() < 4.0,
            format!("화면에서 {d:.1}초로 열림 (녹음 {rec_secs:.1}초)"),
        ),
        Err(e) => c.add("다시 듣기", false, e),
    };

    // ── 1-1. 회의록 내보내기: Word(.docx) 는 앱이 직접, PDF 는 화면과 같은 길(인쇄용 칸 → PrintToPdf)로 ──
    let out = home().unwrap_or_else(std::env::temp_dir);
    let word = out.join("smoke-export.docx");
    let saved = std::fs::write(&word, crate::export::docx(&live, crate::session::TrMode::Orig)).map(|_| std::fs::read(&word).unwrap_or_default());
    match saved {
        Ok(b) => c.add("Word 내보내기", b.starts_with(b"PK\x03\x04") && b.len() > 2000, format!("{} KB", b.len() / 1024)),
        Err(e) => c.add("Word 내보내기", false, e.to_string()),
    };
    let pdf = out.join("smoke-export.pdf");
    match probe_pdf(app, &live.id, &pdf) {
        Ok(b) => c.add("PDF 내보내기", b.starts_with(b"%PDF") && b.len() > 5000, format!("{} KB", b.len() / 1024)),
        Err(e) => c.add("PDF 내보내기", false, e),
    };

    // ── 1-2. 회의 후 화자 다시 정리 → 이름 붙이기 → 목소리 기억 ──
    const VOICE_NAME: &str = "점검 화자";
    let speaker_ready = {
        let st = app.state::<AppState>();
        badasugi_core::models::SPEAKER_MODEL.is_ready(&st.paths.models) && st.settings.lock().diarization.enabled
    };
    if speaker_ready {
        let voices = crate::voices::read_session_voices(&sessions, &live.id);
        let secs: f32 = voices.iter().map(|v| v.dur_ms as f32 / 1000.0).sum();
        c.add(
            "화자 다시 정리",
            !voices.is_empty(),
            format!("문장 {}개 목소리 · {secs:.0}초 ({}.voices)", voices.len(), live.id),
        );
        // 화면에서 화자 이름을 바꾼 것처럼 저장한다
        let mut doc = live.clone();
        match doc.utterances.iter().find_map(|u| u.speaker) {
            Some(spk) => {
                doc.speakers.insert(spk, VOICE_NAME.into());
                let saved = app.state::<AppState>().store.lock().save_doc(doc);
                crate::sync_session_voices(app, &live.id);
                let remembered = app.state::<crate::voices::VoiceBook>().list().into_iter().find(|v| v.name == VOICE_NAME);
                c.add(
                    "목소리 기억",
                    saved.is_ok() && remembered.is_some(),
                    match (&saved, &remembered) {
                        (Err(e), _) => format!("이름 저장 실패: {e}"),
                        (_, Some(v)) => format!("'{VOICE_NAME}' {:.0}초 기억함", v.secs),
                        _ => "기억한 목소리가 없어요".into(),
                    },
                );
            }
            None => {
                c.add("목소리 기억", false, "화자가 정해진 문장이 없어요");
            }
        }
    }

    // ── 2. 파일 받아쓰기 ──
    let wav_for_watch = wav.clone();
    let file = std::env::var_os("BADASUGI_SMOKE_FILE").map(PathBuf::from).unwrap_or(wav);
    let t1 = Instant::now();
    if !c.add("파일 받아쓰기 시작", crate::start_session(app, Some(&file), None).is_ok(), file.display().to_string()) {
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
            if speaker_ready {
                // 같은 목소리(같은 테스트 음성)이므로 기억해 둔 이름이 저절로 붙어야 한다
                let names: Vec<&str> = doc.speakers.values().map(String::as_str).collect();
                c.add(
                    "목소리로 이름 붙이기",
                    names.contains(&VOICE_NAME),
                    if names.is_empty() { "이름 없음".to_string() } else { names.join(", ") },
                );
            }
        }
        None => {
            c.add("파일 기록 저장", false, "기록 파일이 없어요");
        }
    }

    // ── 2-2. 목소리 잊기: 잊은 뒤 예전 회의를 고쳐 저장해도 다시 기억되면 안 된다 ──
    if speaker_ready {
        crate::voices::strip_name(&sessions, VOICE_NAME, None);
        app.state::<crate::voices::VoiceBook>().forget(VOICE_NAME);
        // (기록 잠금을 한 문장 안에서 두 번 잡지 않게 나눠서)
        let loaded = app.state::<AppState>().store.lock().load(&live.id);
        let edited = loaded.and_then(|mut d| {
            d.title.push_str(" (제목 고침)");
            app.state::<AppState>().store.lock().save_doc(d)
        });
        crate::sync_session_voices(app, &live.id);
        let back = app.state::<crate::voices::VoiceBook>().list().iter().any(|v| v.name == VOICE_NAME);
        c.add(
            "목소리 잊기",
            edited.is_ok() && !back,
            if back { "잊은 목소리가 기록을 고치자 다시 기억됐어요".to_string() } else { "잊은 뒤 기록을 고쳐도 다시 기억하지 않음".to_string() },
        );
    }

    // ── 2-3. 녹음 폴더 지켜보기 ──
    watch_check(app, c, &wav_for_watch, &sessions);

    // ── 2-4. 말로 입력 ──
    dictate_check(app, c, expect.first().map(String::as_str));

    // ── 2-5. 일정(Outlook): 꺼져 있으면 띄우지 않고 건너뛴다 ──
    calendar_check(app, c);

    // ── 3. 진단 로그가 남는지 ──
    let logs = crate::logging::log_dir();
    let has_log = std::fs::read_dir(&logs)
        .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().starts_with("badasugi-")))
        .unwrap_or(false);
    c.add("진단 로그", has_log, logs.display().to_string());
}
