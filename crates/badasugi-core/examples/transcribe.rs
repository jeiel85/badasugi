//! 파일 전사 테스트 도구
//!
//! ```text
//! cargo run --release --example transcribe -- <파일.wav> [옵션]
//!   --model <ggml.bin>          로컬 whisper 모델
//!   --api <base_url> <model>    OpenAI 호환 API (키는 BADASUGI_API_KEY 환경변수)
//!   --speaker <model.onnx>      화자 임베딩 모델 (없으면 화자 분리 끔)
//!   --threshold <0.45>          화자 구분 기준
//!   --prompt <텍스트>           용어집
//!   --beam <n>                  beam search 크기
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use badasugi_core::asr::EngineConfig;
use badasugi_core::pipeline::{transcribe_file, Event, PipelineConfig};
use badasugi_core::speaker::DiarizationConfig;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    if std::env::var("BADASUGI_NO_BOOST").is_err() {
        badasugi_core::perf::disable_power_throttling();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = PathBuf::from(args.first().expect("WAV 경로를 주세요"));
    let mut engine = None;
    let mut speaker = None;
    let mut diar = DiarizationConfig::default();
    let mut prompt = String::new();
    let mut beam = 1;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                engine = Some(("local", args[i + 1].clone(), String::new()));
                i += 2;
            }
            "--api" => {
                engine = Some(("api", args[i + 1].clone(), args[i + 2].clone()));
                i += 3;
            }
            "--speaker" => {
                speaker = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--threshold" => {
                diar.threshold = args[i + 1].parse()?;
                i += 2;
            }
            "--no-split" => {
                diar.split_turns = false;
                i += 1;
            }
            "--prompt" => {
                prompt = args[i + 1].clone();
                i += 2;
            }
            "--beam" => {
                beam = args[i + 1].parse()?;
                i += 2;
            }
            other => anyhow::bail!("알 수 없는 옵션: {other}"),
        }
    }
    let engine = match engine.expect("--model 또는 --api 가 필요합니다") {
        ("local", path, _) => EngineConfig::Local {
            model_path: path.into(),
            threads: 0,
            beam_size: beam,
        },
        (_, base, model) => EngineConfig::Api {
            base_url: base,
            api_key: std::env::var("BADASUGI_API_KEY").unwrap_or_default(),
            model,
        },
    };
    diar.enabled = speaker.is_some();
    let cfg = PipelineConfig {
        capture: Default::default(),
        engine,
        diarization: diar,
        speaker_model: speaker,
        segmenter: Default::default(),
        language: "ko".into(),
        prompt,
        record_path: None,
    };

    let t0 = Instant::now();
    let audio_ms = Arc::new(parking_lot::Mutex::new(0u64));
    let am = audio_ms.clone();
    let total_asr = Arc::new(parking_lot::Mutex::new(0u64));
    let ta = total_asr.clone();
    let sink = Arc::new(move |e: Event| match e {
        Event::Transcript {
            start_ms,
            end_ms,
            speaker,
            text,
            asr_ms,
            ..
        } => {
            *am.lock() += end_ms - start_ms;
            *ta.lock() += asr_ms;
            let spk = speaker.map(|s| format!("화자{}", s + 1)).unwrap_or_else(|| "-".into());
            println!(
                "[{:>6.1}s~{:>6.1}s] {spk:<5} ({:>5}ms) {text}",
                start_ms as f32 / 1000.0,
                end_ms as f32 / 1000.0,
                asr_ms
            );
        }
        Event::Failed { id, message } => eprintln!("#{id} 실패: {message}"),
        Event::Discarded { id } => eprintln!("#{id} (버림)"),
        Event::Pending { id, start_ms, end_ms, speaker } if std::env::var("BD_TRACE").is_ok() => {
            eprintln!("  pending #{id} {start_ms}~{end_ms} spk={speaker:?}")
        }
        Event::Stopped => eprintln!("[종료]"),
        Event::Status { message } | Event::Error { message } => eprintln!("[상태] {message}"),
        Event::Started { engine, diarization, .. } => eprintln!("[시작] {engine} / 화자분리 {diarization}"),
        _ => {}
    });
    transcribe_file(&file, &cfg, sink)?;
    let wall = t0.elapsed().as_secs_f32();
    let speech = *audio_ms.lock() as f32 / 1000.0;
    // 주의: 한 문장 안에서 같은 Mutex 를 두 번 잠그면 교착된다 (parking_lot 은 재진입 불가)
    let asr = *total_asr.lock() as f32 / 1000.0;
    eprintln!(
        "\n발화 {speech:.1}초 · 인식 시간 합계 {asr:.1}초 · 실시간 배율 {:.2}x · 전체 {wall:.1}초",
        speech / asr.max(0.001)
    );
    Ok(())
}
