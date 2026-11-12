//! 실시간 전사 파이프라인
//!
//! ```text
//!  [캡처 스레드] cpal 콜백 ─RawChunk─▶ [DSP 스레드] 리샘플·믹스·레벨·VAD 분할
//!                                            │ SpeechSegment
//!                                            ▼
//!                                   [디스패처] 화자 전환 분할 · 화자 배정 → Pending 이벤트
//!                                            │ Job
//!                                            ▼
//!                                   [ASR 워커 ×N] 인식 → 후처리 → Transcript 이벤트
//! ```

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};

use crate::asr::{self, AsrRequest, EngineConfig};
use crate::audio::{Capture, CaptureConfig, RawChunk, SourceKind};
use crate::resample::Resampler;
use crate::speaker::{self, DiarizationConfig, SpeakerEncoder, SpeakerTracker};
use crate::vad::{Segmenter, SegmenterConfig, SpeechSegment};
use crate::wav::WavRecorder;
use crate::{samples_to_ms, SAMPLE_RATE};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineConfig {
    #[serde(default)]
    pub capture: CaptureConfig,
    pub engine: EngineConfig,
    #[serde(default)]
    pub diarization: DiarizationConfig,
    /// 화자 임베딩 ONNX 모델 경로 (없으면 화자 분리 끔)
    #[serde(default)]
    pub speaker_model: Option<PathBuf>,
    #[serde(default)]
    pub segmenter: SegmenterConfig,
    /// "ko" 기본
    #[serde(default = "default_lang")]
    pub language: String,
    /// 용어집 · 문맥 프롬프트
    #[serde(default)]
    pub prompt: String,
    /// 지정하면 16 kHz 모노 WAV 로 녹음도 남긴다
    #[serde(default)]
    pub record_path: Option<PathBuf>,
}

fn default_lang() -> String {
    "ko".into()
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    #[serde(rename_all = "camelCase")]
    Started {
        mic: Option<String>,
        system: Option<String>,
        engine: String,
        diarization: bool,
    },
    #[serde(rename_all = "camelCase")]
    Level { rms: f32, speaking: bool, position_ms: u64 },
    #[serde(rename_all = "camelCase")]
    Pending {
        id: u64,
        start_ms: u64,
        end_ms: u64,
        speaker: Option<u32>,
    },
    #[serde(rename_all = "camelCase")]
    Transcript {
        id: u64,
        start_ms: u64,
        end_ms: u64,
        speaker: Option<u32>,
        text: String,
        /// 발화가 끝난 뒤 글자가 나오기까지 (대기열 포함)
        latency_ms: u64,
        /// 순수 인식 시간
        asr_ms: u64,
    },
    #[serde(rename_all = "camelCase")]
    Discarded { id: u64 },
    #[serde(rename_all = "camelCase")]
    Failed { id: u64, message: String },
    #[serde(rename_all = "camelCase")]
    Status { message: String },
    #[serde(rename_all = "camelCase")]
    Error { message: String },
    Stopped,
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

struct Job {
    id: u64,
    seg: SpeechSegment,
    speaker: Option<u32>,
    queued: Instant,
}

/// 실행 중인 녹음 세션
pub struct Pipeline {
    capture: Option<Capture>,
    stop: Arc<AtomicBool>,
    dsp: Option<JoinHandle<()>>,
    backend: Option<JoinHandle<()>>,
    sink: EventSink,
}

impl Pipeline {
    pub fn start(cfg: PipelineConfig, sink: EventSink) -> Result<Pipeline> {
        validate(&cfg)?;
        let segmenter = Segmenter::new(cfg.segmenter.clone())?;
        let recorder = match &cfg.record_path {
            Some(p) => Some(WavRecorder::create(p)?),
            None => None,
        };

        let (raw_tx, raw_rx) = crossbeam_channel::bounded::<RawChunk>(512);
        let capture = Capture::start(&cfg.capture, raw_tx)?;
        let (mic, system) = (capture.mic_name.clone(), capture.system_name.clone());

        let (seg_tx, backend, diar) = spawn_backend(&cfg, sink.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let dsp = {
            let sink = sink.clone();
            let stop = stop.clone();
            let source = cfg.capture.source;
            std::thread::Builder::new()
                .name("bd-dsp".into())
                .spawn(move || dsp_thread(raw_rx, source, segmenter, seg_tx, sink, recorder, stop))?
        };
        sink(Event::Started {
            mic,
            system,
            engine: cfg.engine.label(),
            diarization: diar,
        });
        Ok(Pipeline {
            capture: Some(capture),
            stop,
            dsp: Some(dsp),
            backend: Some(backend),
            sink,
        })
    }

    /// 녹음을 멈춘다. 남은 발화 인식이 끝나면 `Stopped` 이벤트가 온다. (즉시 반환)
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let capture = self.capture.take();
        let dsp = self.dsp.take();
        let backend = self.backend.take();
        let sink = self.sink.clone();
        let _ = std::thread::Builder::new().name("bd-stop".into()).spawn(move || {
            if let Some(c) = capture {
                c.stop();
            }
            if let Some(h) = dsp {
                let _ = h.join();
            }
            if let Some(h) = backend {
                let _ = h.join();
            }
            sink(Event::Stopped);
        });
    }
}

fn validate(cfg: &PipelineConfig) -> Result<()> {
    match &cfg.engine {
        EngineConfig::Local { model_path, .. } => {
            if !model_path.exists() {
                return Err(anyhow!(
                    "로컬 음성 인식 모델이 없습니다. 설정에서 모델을 내려받아 주세요.\n({})",
                    model_path.display()
                ));
            }
        }
        EngineConfig::Api { base_url, .. } => {
            if base_url.trim().is_empty() {
                return Err(anyhow!("API 주소를 입력해 주세요."));
            }
        }
    }
    Ok(())
}

/// 디스패처 + ASR 워커를 띄운다. 반환된 Sender 를 닫으면 남은 일을 끝내고 종료한다.
fn spawn_backend(cfg: &PipelineConfig, sink: EventSink) -> (Sender<SpeechSegment>, JoinHandle<()>, bool) {
    let (seg_tx, seg_rx) = crossbeam_channel::unbounded::<SpeechSegment>();
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Job>();

    let encoder = if cfg.diarization.enabled {
        match cfg.speaker_model.as_deref() {
            Some(p) if p.exists() => match SpeakerEncoder::new(p) {
                Ok(e) => Some(e),
                Err(e) => {
                    sink(Event::Status {
                        message: format!("화자 분리를 끕니다: {e}"),
                    });
                    None
                }
            },
            _ => {
                sink(Event::Status {
                    message: "화자 인식 모델이 없어 화자 분리 없이 진행합니다.".into(),
                });
                None
            }
        }
    } else {
        None
    };
    let diarization = encoder.is_some();

    let n = cfg.engine.concurrency();
    let mut workers = Vec::with_capacity(n);
    for i in 0..n {
        let rx = job_rx.clone();
        let sink = sink.clone();
        let engine = cfg.engine.clone();
        let lang = cfg.language.clone();
        let prompt = cfg.prompt.clone();
        workers.push(
            std::thread::Builder::new()
                .name(format!("bd-asr-{i}"))
                .spawn(move || asr_worker(i, rx, engine, lang, prompt, sink))
                .expect("spawn asr worker"),
        );
    }
    drop(job_rx);

    let dcfg = cfg.diarization.clone();
    let handle = std::thread::Builder::new()
        .name("bd-dispatch".into())
        .spawn(move || {
            dispatcher(seg_rx, job_tx, encoder, dcfg, sink);
            for w in workers {
                let _ = w.join();
            }
        })
        .expect("spawn dispatcher");
    (seg_tx, handle, diarization)
}

fn dispatcher(
    seg_rx: Receiver<SpeechSegment>,
    job_tx: Sender<Job>,
    mut encoder: Option<SpeakerEncoder>,
    cfg: DiarizationConfig,
    sink: EventSink,
) {
    let mut tracker = SpeakerTracker::new(cfg.clone());
    let mut next_id = 0u64;
    for seg in seg_rx {
        let parts = match encoder.as_mut() {
            Some(enc) if cfg.split_turns => match speaker::find_turns(enc, &seg, cfg.threshold) {
                Ok(cuts) => speaker::split_segment(seg, &cuts),
                Err(e) => {
                    log::warn!("화자 전환 감지 실패: {e}");
                    vec![seg]
                }
            },
            _ => vec![seg],
        };
        for part in parts {
            let speaker = encoder.as_mut().map(|enc| {
                let emb = enc.embed(&part.samples).unwrap_or_else(|e| {
                    log::warn!("화자 임베딩 실패: {e}");
                    None
                });
                tracker.assign(emb.as_deref(), part.duration_ms())
            });
            let id = next_id;
            next_id += 1;
            sink(Event::Pending {
                id,
                start_ms: samples_to_ms(part.start),
                end_ms: samples_to_ms(part.end),
                speaker,
            });
            if job_tx
                .send(Job {
                    id,
                    seg: part,
                    speaker,
                    queued: Instant::now(),
                })
                .is_err()
            {
                return;
            }
        }
    }
}

fn asr_worker(idx: usize, rx: Receiver<Job>, engine_cfg: EngineConfig, lang: String, prompt: String, sink: EventSink) {
    let is_local = matches!(engine_cfg, EngineConfig::Local { .. });
    if is_local && idx == 0 {
        sink(Event::Status {
            message: "음성 인식 모델을 불러오는 중…".into(),
        });
    }
    let t = Instant::now();
    let mut engine = match asr::create(&engine_cfg) {
        Ok(e) => {
            if is_local && idx == 0 {
                sink(Event::Status {
                    message: format!("모델 준비 완료 ({:.1}초)", t.elapsed().as_secs_f32()),
                });
            }
            Some(e)
        }
        Err(e) => {
            if idx == 0 {
                sink(Event::Error {
                    message: format!("음성 인식 엔진을 시작하지 못했습니다: {e}"),
                });
            }
            None
        }
    };
    for job in rx {
        let Some(engine) = engine.as_mut() else {
            sink(Event::Failed {
                id: job.id,
                message: "엔진 초기화 실패".into(),
            });
            continue;
        };
        let t0 = Instant::now();
        let req = AsrRequest {
            samples: &job.seg.samples,
            language: &lang,
            prompt: &prompt,
        };
        match engine.transcribe(&req) {
            Ok(raw) => {
                let text = crate::text::clean(&raw);
                if text.is_empty() {
                    sink(Event::Discarded { id: job.id });
                } else {
                    sink(Event::Transcript {
                        id: job.id,
                        start_ms: samples_to_ms(job.seg.start),
                        end_ms: samples_to_ms(job.seg.end),
                        speaker: job.speaker,
                        text,
                        latency_ms: job.queued.elapsed().as_millis() as u64,
                        asr_ms: t0.elapsed().as_millis() as u64,
                    });
                }
            }
            Err(e) => sink(Event::Failed {
                id: job.id,
                message: e.to_string(),
            }),
        }
    }
}

fn dsp_thread(
    rx: Receiver<RawChunk>,
    source: SourceKind,
    mut segmenter: Segmenter,
    seg_tx: Sender<SpeechSegment>,
    sink: EventSink,
    mut recorder: Option<WavRecorder>,
    stop: Arc<AtomicBool>,
) {
    const LEAD: usize = SAMPLE_RATE as usize / 5; // 200 ms
    let both = source == SourceKind::Both;
    let mut resamplers: [Option<Resampler>; 2] = [None, None];
    let mut queues: [VecDeque<f32>; 2] = [VecDeque::new(), VecDeque::new()];
    let started = Instant::now();
    let mut produced: u64 = 0;
    let mut last_data = Instant::now();
    let mut level_sum = 0.0f64;
    let mut level_n = 0usize;
    let mut last_level = Instant::now();
    let mut segs = Vec::new();
    let mut tmp = Vec::with_capacity(4096);
    let mut mixed = Vec::with_capacity(4096);

    let mut ingest = |c: RawChunk, resamplers: &mut [Option<Resampler>; 2], queues: &mut [VecDeque<f32>; 2]| {
        let i = (c.source as usize).min(1);
        let r = resamplers[i].get_or_insert_with(|| Resampler::new(c.rate, SAMPLE_RATE));
        if r.in_rate() != c.rate {
            *r = Resampler::new(c.rate, SAMPLE_RATE);
        }
        tmp.clear();
        r.process(&c.samples, &mut tmp);
        queues[i].extend(tmp.iter().copied());
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(c) => {
                ingest(c, &mut resamplers, &mut queues);
                last_data = Instant::now();
                while let Ok(c) = rx.try_recv() {
                    ingest(c, &mut resamplers, &mut queues);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        mixed.clear();
        if both {
            let (a, b) = (queues[0].len(), queues[1].len());
            // 한쪽이 조용해서 데이터가 안 오면(루프백은 무음일 때 패킷이 없다) 기다리지 않고 0 으로 채운다
            let n = if a.max(b) > LEAD { a.max(b) - LEAD / 2 } else { a.min(b) };
            for _ in 0..n {
                let x = queues[0].pop_front().unwrap_or(0.0) + queues[1].pop_front().unwrap_or(0.0);
                mixed.push(x.clamp(-1.0, 1.0));
            }
        } else {
            let i = if source == SourceKind::System { 1 } else { 0 };
            mixed.extend(queues[i].drain(..));
        }

        // 장치가 아무것도 안 보내는 동안에도 시간은 흐르게 (발화 종료 판정을 위해)
        if mixed.is_empty() && last_data.elapsed() > Duration::from_millis(300) && !stop.load(Ordering::Relaxed) {
            let expected = (started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
            if expected > produced + 4800 {
                let fill = ((expected - produced - 1600) as usize).min(SAMPLE_RATE as usize);
                mixed.resize(fill, 0.0);
            }
        }
        if mixed.is_empty() {
            continue;
        }
        produced += mixed.len() as u64;
        if let Some(r) = recorder.as_mut() {
            r.write(&mixed);
        }
        for &s in &mixed {
            level_sum += (s * s) as f64;
        }
        level_n += mixed.len();

        if let Err(e) = segmenter.push(&mixed, &mut segs) {
            sink(Event::Error {
                message: format!("VAD 오류: {e}"),
            });
        }
        for s in segs.drain(..) {
            let _ = seg_tx.send(s);
        }

        if last_level.elapsed() >= Duration::from_millis(70) && level_n > 0 {
            sink(Event::Level {
                rms: (level_sum / level_n as f64).sqrt() as f32,
                speaking: segmenter.is_speaking(),
                position_ms: samples_to_ms(produced),
            });
            level_sum = 0.0;
            level_n = 0;
            last_level = Instant::now();
        }
    }

    // 큐에 남은 200 ms 미만 자투리는 버리고, 진행 중이던 발화만 마무리한다
    segmenter.flush(&mut segs);
    for s in segs.drain(..) {
        let _ = seg_tx.send(s);
    }
    if let Some(r) = recorder.take() {
        if let Err(e) = r.finish() {
            log::warn!("녹음 파일 저장 실패: {e}");
        }
    }
}

/// WAV 파일 전사 (테스트·파일 불러오기용). 끝날 때까지 블록한다.
pub fn transcribe_file(path: &Path, cfg: &PipelineConfig, sink: EventSink) -> Result<()> {
    validate(cfg)?;
    let samples = crate::wav::read_16k_mono(path)?;
    let mut segmenter = Segmenter::new(cfg.segmenter.clone())?;
    let (seg_tx, backend, diar) = spawn_backend(cfg, sink.clone());
    sink(Event::Started {
        mic: None,
        system: Some(path.display().to_string()),
        engine: cfg.engine.label(),
        diarization: diar,
    });
    let mut out = Vec::new();
    for chunk in samples.chunks(SAMPLE_RATE as usize / 10) {
        segmenter.push(chunk, &mut out)?;
        for s in out.drain(..) {
            let _ = seg_tx.send(s);
        }
    }
    segmenter.flush(&mut out);
    for s in out.drain(..) {
        let _ = seg_tx.send(s);
    }
    drop(seg_tx);
    let _ = backend.join();
    sink(Event::Stopped);
    Ok(())
}
