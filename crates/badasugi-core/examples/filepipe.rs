//! 파일 파이프라인(Pipeline::start_file) 점검
//!
//! ```text
//! cargo run --release --example filepipe -- <오디오 파일> --model <ggml.bin> [옵션]
//!   --speaker <model.onnx>     화자 임베딩 모델 (화자 분리 켬)
//!   --stop-after <초>          이 시간 뒤 stop() 호출 (Stopped 가 정확히 한 번 오는지 확인)
//!   --pause <시작초> <길이초>   일시정지 → 다시 시작 (이미 넘긴 발화만 마저 인식하고 멈추는지 확인)
//!   --quiet                    인식 결과 줄은 출력하지 않음
//!   --decode-only [--ref <wav>] [--dump <wav>]
//!                              인식 없이 디코딩만 (길이·속도, 기준 WAV 와 상관계수, 16 kHz 결과 저장)
//! ```
//! Stopped 를 받은 뒤에도 3초 더 이벤트를 지켜보고, Stopped 가 한 번이 아니면 실패로 끝난다.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use badasugi_core::asr::EngineConfig;
use badasugi_core::pipeline::{Event, Pipeline, PipelineConfig};
use badasugi_core::speaker::DiarizationConfig;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = PathBuf::from(args.first().expect("오디오 파일 경로를 주세요"));
    let (mut model, mut speaker, mut stop_after, mut pause, mut quiet) = (None, None, None, None, false);
    let (mut decode_only, mut reference, mut dump) = (false, None, None);
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                model = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--speaker" => {
                speaker = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--stop-after" => {
                stop_after = Some(Duration::from_secs_f32(args[i + 1].parse()?));
                i += 2;
            }
            "--pause" => {
                let at = Duration::from_secs_f32(args[i + 1].parse()?);
                pause = Some((at, at + Duration::from_secs_f32(args[i + 2].parse()?)));
                i += 3;
            }
            "--quiet" => {
                quiet = true;
                i += 1;
            }
            "--decode-only" => {
                decode_only = true;
                i += 1;
            }
            "--ref" => {
                reference = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--dump" => {
                dump = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            other => anyhow::bail!("알 수 없는 옵션: {other}"),
        }
    }
    if decode_only {
        return decode(&file, reference, dump);
    }
    let cfg = PipelineConfig {
        capture: Default::default(),
        engine: EngineConfig::Local {
            model_path: model.expect("--model 이 필요합니다"),
            threads: 0,
            beam_size: 1,
        },
        diarization: DiarizationConfig {
            enabled: speaker.is_some(),
            ..Default::default()
        },
        speaker_model: speaker,
        segmenter: Default::default(),
        language: "ko".into(),
        prompt: String::new(),
        record_path: None,
    };

    let (tx, rx) = crossbeam_channel::unbounded::<Event>();
    let sink = Arc::new(move |e: Event| {
        let _ = tx.send(e);
    });
    let t0 = Instant::now();
    let mut pipeline = match Pipeline::start_file(&file, cfg, sink) {
        Ok(p) => Some(p),
        Err(e) => {
            println!("start_file 실패 ({:.0} ms): {e}", t0.elapsed().as_secs_f64() * 1000.0);
            return Ok(());
        }
    };
    println!("start_file 반환 ({:.0} ms)", t0.elapsed().as_secs_f64() * 1000.0);

    let secs = || t0.elapsed().as_secs_f32();
    let mut stopped = 0;
    let mut first_stopped: Option<Instant> = None;
    let mut stop_called: Option<Instant> = None;
    let (mut transcripts, mut after_stopped, mut progress_n) = (0, 0, 0);
    // 디스패처가 넘긴 발화(Pending) 와 끝난 발화(Transcript·Discarded·Failed): 차이 = 인식 대기열
    let (mut pendings, mut completed) = (0, 0);
    let mut last_done = 0u64;
    let mut paused_state = false;
    // 일시정지 중: 새 발화(Pending) 수, 마지막 활동(Pending·Transcript·Progress) 시점
    let (mut pending_in_pause, mut pause_last_activity) = (0, Duration::ZERO);
    let mut pause_since: Option<Instant> = None;
    let mut progress_backwards = 0;
    let mut warnings = Vec::new();

    loop {
        if let Some(t) = first_stopped {
            if t.elapsed() > Duration::from_secs(3) {
                break;
            }
        }
        if let (Some(after), Some(p)) = (stop_after, pipeline.as_ref()) {
            if t0.elapsed() >= after && stop_called.is_none() {
                println!(
                    "[{:>6.2}s] stop() 호출 (일시정지={}, 인식 대기·진행 중 발화 {}개, 진행 {last_done} ms)",
                    secs(),
                    p.is_paused(),
                    pendings - completed
                );
                let t = Instant::now();
                pipeline.take().unwrap().stop();
                println!("          stop() 반환까지 {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
                stop_called = Some(Instant::now());
            }
        }
        if let (Some((from, to)), Some(p)) = (pause, pipeline.as_ref()) {
            let now = t0.elapsed();
            let want = now >= from && now < to;
            if want != paused_state {
                p.set_paused(want);
                paused_state = want;
                assert_eq!(p.is_paused(), want);
                println!("[{:>6.2}s] {} (done {last_done} ms)", secs(), if want { "일시정지" } else { "다시 시작" });
                if !want {
                    println!(
                        "          일시정지 중 새 발화 {pending_in_pause}개 · 마지막 활동 +{:.2}s",
                        pause_last_activity.as_secs_f32()
                    );
                }
                pause_since = want.then(Instant::now);
            }
        }
        let Ok(e) = rx.recv_timeout(Duration::from_millis(50)) else {
            continue;
        };
        if first_stopped.is_some() && !matches!(e, Event::Stopped) {
            after_stopped += 1;
        }
        if let Some(t) = pause_since {
            if matches!(e, Event::Pending { .. } | Event::Transcript { .. } | Event::Progress { .. }) {
                pause_last_activity = pause_last_activity.max(t.elapsed());
            }
            pending_in_pause += matches!(e, Event::Pending { .. }) as usize;
        }
        match e {
            Event::Started { mic, system, engine, diarization } => {
                println!("[{:>6.2}s] 시작: mic={mic:?} system={system:?} {engine} 화자분리={diarization}", secs())
            }
            Event::Progress { done_ms, total_ms } => {
                progress_n += 1;
                if done_ms < last_done {
                    progress_backwards += 1;
                }
                last_done = done_ms;
                println!("[{:>6.2}s] 진행 {done_ms}/{total_ms} ms", secs());
            }
            Event::Transcript { id, start_ms, end_ms, speaker, text, asr_ms, .. } => {
                transcripts += 1;
                completed += 1;
                if !quiet {
                    println!(
                        "[{:>6.2}s] #{id} {:.1}~{:.1}s 화자{:?} ({asr_ms}ms) {text}",
                        secs(),
                        start_ms as f32 / 1000.0,
                        end_ms as f32 / 1000.0,
                        speaker.map(|s| s + 1)
                    );
                }
            }
            Event::Pending { .. } => pendings += 1,
            Event::Discarded { .. } => completed += 1,
            Event::Failed { id, message } => {
                completed += 1;
                println!("[{:>6.2}s] #{id} 실패: {message}", secs());
            }
            Event::Stopped => {
                stopped += 1;
                println!("[{:>6.2}s] Stopped ({}번째)", secs(), stopped);
                if let Some(s) = stop_called {
                    println!("          stop() → Stopped {:.2}s", s.elapsed().as_secs_f32());
                }
                first_stopped.get_or_insert_with(Instant::now);
            }
            Event::Warning { message } => {
                warnings.push(message.clone());
                println!("[{:>6.2}s] 경고: {message}", secs())
            }
            other => println!("[{:>6.2}s] {other:?}", secs()),
        }
    }
    if let Some(p) = pipeline.take() {
        let t = Instant::now();
        drop(p);
        println!("Stopped 뒤 Pipeline drop: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "\n요약: Stopped {stopped}번 · 인식 {transcripts}개 · 진행 이벤트 {progress_n}개 (뒤로 간 것 {progress_backwards}) · Stopped 이후 이벤트 {after_stopped}개 · 경고 {}개 · 전체 {:.1}s",
        warnings.len(),
        secs()
    );
    if stopped != 1 {
        anyhow::bail!("Stopped 가 정확히 한 번이 아님: {stopped}");
    }
    Ok(())
}

/// AudioFile 로 끝까지 디코딩해 길이·속도를 보고, 기준 WAV 가 있으면 지연을 맞춰 상관계수를 잰다
fn decode(file: &std::path::Path, reference: Option<PathBuf>, dump: Option<PathBuf>) -> anyhow::Result<()> {
    use badasugi_core::decode::AudioFile;
    let t = Instant::now();
    let mut f = AudioFile::open(file)?;
    let opened = t.elapsed();
    let mut out = Vec::new();
    let mut reads = 0;
    while f.read(&mut out)? {
        reads += 1;
    }
    let rms = (out.iter().map(|v| v * v).sum::<f32>() / out.len().max(1) as f32).sqrt();
    println!(
        "{}: open {:.1} ms · 전체 {:.1} ms · read {reads}번 · {} 샘플 = {:.3}s (duration_ms {:?}) · RMS {rms:.4}",
        file.display(),
        opened.as_secs_f64() * 1000.0,
        t.elapsed().as_secs_f64() * 1000.0,
        out.len(),
        out.len() as f32 / 16000.0,
        f.duration_ms()
    );
    if let Some(d) = dump {
        let mut rec = badasugi_core::wav::WavRecorder::create(&d)?;
        rec.write(&out);
        rec.finish()?;
    }
    if let Some(r) = reference {
        let want = badasugi_core::wav::read_16k_mono(&r)?;
        let corr = |lag: usize, limit: usize| {
            let n = want.len().min(out.len().saturating_sub(lag)).min(limit);
            let (a, b) = (&want[..n], &out[lag..lag + n]);
            let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            dot / (na * nb).max(1e-9)
        };
        // 인코더 지연(수십 ms)을 감안해 앞 10초로 0~200 ms 사이 최적 지연을 찾고, 전체 상관계수를 잰다
        let best_lag = (0..3200usize)
            .map(|lag| (lag, corr(lag, 160_000)))
            .max_by(|x, y| x.1.total_cmp(&y.1))
            .map_or(0, |(lag, _)| lag);
        let best = corr(best_lag, usize::MAX);
        println!(
            "  기준 {} 샘플 대비 길이 차 {:+} · 지연 {:.1} ms · 상관계수 {best:.4}",
            want.len(),
            out.len() as i64 - want.len() as i64,
            best_lag as f32 / 16.0
        );
    }
    Ok(())
}
