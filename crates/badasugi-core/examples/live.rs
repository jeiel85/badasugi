//! 실제 장치 캡처 점검: live <mic|system|both> <초> <ggml.bin>
use std::sync::Arc;

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
    let cfg = PipelineConfig {
        capture: CaptureConfig { source, mic_device: None },
        engine: EngineConfig::Local { model_path: a[3].clone().into(), threads: 0, beam_size: 1 },
        diarization: Default::default(),
        speaker_model: None,
        segmenter: Default::default(),
        language: "ko".into(),
        prompt: String::new(),
        record_path: None,
    };
    let (tx, rx) = crossbeam_channel::unbounded();
    let sink = Arc::new(move |e: Event| {
        let _ = tx.send(e);
    });
    let p = Pipeline::start(cfg, sink)?;
    let t0 = std::time::Instant::now();
    let mut levels = 0;
    let mut max_rms = 0f32;
    while t0.elapsed().as_secs() < secs {
        if let Ok(e) = rx.recv_timeout(std::time::Duration::from_millis(100)) {
            match e {
                Event::Level { rms, .. } => {
                    levels += 1;
                    max_rms = max_rms.max(rms);
                }
                Event::Started { mic, system, .. } => println!("시작: mic={mic:?} system={system:?}"),
                other => println!("{other:?}"),
            }
        }
    }
    p.stop();
    while let Ok(e) = rx.recv_timeout(std::time::Duration::from_secs(20)) {
        if matches!(e, Event::Stopped) {
            println!("정지 완료");
            break;
        }
    }
    println!("레벨 이벤트 {levels}개 (초당 {:.1}), 최대 RMS {max_rms:.4}", levels as f32 / secs as f32);
    Ok(())
}
