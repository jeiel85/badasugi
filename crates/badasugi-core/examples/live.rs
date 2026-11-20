//! 실제 장치 캡처 점검: live <mic|system|both> <초> <ggml.bin> [일시정지 시작초 일시정지 길이초]
//! (BADASUGI_TEST_WAV=<wav> 를 주면 장치 대신 파일을 실시간 속도로 흘려 넣는다)
//! (BADASUGI_PREVIEW=1 이면 말하는 동안 미리 보기를 켜고, 미리 보기 · 확정 문장 지연을 출력한다)
use std::sync::Arc;
use std::time::{Duration, Instant};

use badasugi_core::asr::EngineConfig;
use badasugi_core::audio::{CaptureConfig, SourceKind};
use badasugi_core::pipeline::{Event, Pipeline, PipelineConfig};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let source = match a[1].as_str() {
        "mic" => SourceKind::Mic,
        "both" => SourceKind::Both,
        _ => SourceKind::System,
    };
    let secs: u64 = a[2].parse()?;
    let pause = match (a.get(4), a.get(5)) {
        (Some(at), Some(len)) => {
            let at = Duration::from_secs_f32(at.parse()?);
            Some((at, at + Duration::from_secs_f32(len.parse()?)))
        }
        _ => None,
    };
    let cfg = PipelineConfig {
        capture: CaptureConfig { source, mic_device: None },
        engine: EngineConfig::Local { model_path: a[3].clone().into(), threads: 0, beam_size: 1 },
        diarization: Default::default(),
        speaker_model: None,
        segmenter: Default::default(),
        language: "ko".into(),
        prompt: String::new(),
        keywords: Vec::new(),
        record_path: None,
        known_voices: Vec::new(),
        preview: std::env::var("BADASUGI_PREVIEW").is_ok_and(|v| v == "1"),
    };
    let preview_on = cfg.preview;
    let (tx, rx) = crossbeam_channel::unbounded();
    let sink = Arc::new(move |e: Event| {
        let _ = tx.send(e);
    });
    let p = Pipeline::start(cfg, sink)?;
    let t0 = Instant::now();
    let mut levels = 0;
    let mut lat: Vec<u64> = Vec::new();
    let mut previews = 0;
    let mut max_rms = 0f32;
    let mut paused = false;
    // 일시정지 0.7초 뒤부터의 레벨: 최대 rms, speaking=true 개수 (둘 다 0 이어야 함)
    let (mut pause_levels, mut pause_max_rms, mut pause_speaking) = (0, 0f32, 0);
    while t0.elapsed().as_secs() < secs {
        if let Some((from, to)) = pause {
            let want = t0.elapsed() >= from && t0.elapsed() < to;
            if want != paused {
                p.set_paused(want);
                paused = want;
                println!("[{:>5.1}s] {}", t0.elapsed().as_secs_f32(), if want { "일시정지" } else { "다시 시작" });
            }
        }
        if let Ok(e) = rx.recv_timeout(Duration::from_millis(100)) {
            match e {
                Event::Level { rms, speaking, .. } => {
                    levels += 1;
                    max_rms = max_rms.max(rms);
                    if let Some((from, to)) = pause {
                        let now = t0.elapsed();
                        if now >= from + Duration::from_millis(700) && now < to {
                            pause_levels += 1;
                            pause_max_rms = pause_max_rms.max(rms);
                            pause_speaking += speaking as u32;
                        }
                    }
                }
                Event::Started { mic, system, .. } => println!("시작: mic={mic:?} system={system:?}"),
                Event::Transcript { start_ms, end_ms, text, latency_ms, asr_ms, .. } => {
                    lat.push(latency_ms);
                    println!(
                        "[{:>5.1}s] {:.1}~{:.1}s (지연 {latency_ms}ms · 인식 {asr_ms}ms) {text}",
                        t0.elapsed().as_secs_f32(),
                        start_ms as f32 / 1000.0,
                        end_ms as f32 / 1000.0
                    )
                }
                Event::Preview { start_ms, end_ms, text } => {
                    previews += 1;
                    // 미리 보기가 화면에 뜬 때 · 그 말의 끝(녹음 시각) 차이 = 미리 보기가 늦은 정도
                    println!(
                        "[{:>5.1}s]   미리 {:.1}~{:.1}s: {text}",
                        t0.elapsed().as_secs_f32(),
                        start_ms as f32 / 1000.0,
                        end_ms as f32 / 1000.0
                    )
                }
                Event::Pending { .. } => {}
                other => println!("[{:>5.1}s] {other:?}", t0.elapsed().as_secs_f32()),
            }
        }
    }
    p.stop();
    let mut stopped = 0;
    while let Ok(e) = rx.recv_timeout(Duration::from_secs(if stopped == 0 { 60 } else { 2 })) {
        match e {
            Event::Stopped => {
                stopped += 1;
                println!("정지 완료");
            }
            Event::Transcript { start_ms, end_ms, text, latency_ms, .. } => {
                lat.push(latency_ms);
                println!("{:.1}~{:.1}s (지연 {latency_ms}ms) {text}", start_ms as f32 / 1000.0, end_ms as f32 / 1000.0)
            }
            Event::Status { message } => println!("{message}"),
            _ => {}
        }
    }
    if !lat.is_empty() {
        let mut l = lat.clone();
        l.sort_unstable();
        println!(
            "미리 보기 {} · 미리 보기 {previews}번 · 확정 문장 {}개 지연: 평균 {}ms · 중간 {}ms · 최대 {}ms",
            if preview_on { "켬" } else { "끔" },
            l.len(),
            l.iter().sum::<u64>() / l.len() as u64,
            l[l.len() / 2],
            l[l.len() - 1]
        );
    }
    println!("레벨 이벤트 {levels}개 (초당 {:.1}), 최대 RMS {max_rms:.4}, Stopped {stopped}번", levels as f32 / secs as f32);
    if pause.is_some() {
        println!("일시정지 중 레벨 {pause_levels}개: 최대 RMS {pause_max_rms:.4}, speaking {pause_speaking}개");
    }
    Ok(())
}
