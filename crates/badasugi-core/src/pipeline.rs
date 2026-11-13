//! 실시간 · 파일 전사 파이프라인
//!
//! ```text
//!  [캡처 스레드] cpal 콜백 ─RawChunk─▶ [DSP 스레드] 리샘플·믹스·레벨·VAD 분할
//!  [파일 스레드] 디코딩 → 16 kHz 모노 ─────────▶ VAD 분할 (인식이 밀리면 쉬어 간다)
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
use crate::audio::{Capture, CaptureConfig, Notify, RawChunk, SourceKind, SRC_MIC};
use crate::decode::AudioFile;
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
    /// 사용자가 꼭 봐야 하는 문제 (마이크 무음 · 장치 끊김/재연결)
    #[serde(rename_all = "camelCase")]
    Warning { message: String },
    #[serde(rename_all = "camelCase")]
    Error { message: String },
    /// 파일 전사 진행: 인식까지 끝난 위치 / 전체 길이 (모르면 0). 앞으로만 움직이고, 다 끝나면 done = total.
    #[serde(rename_all = "camelCase")]
    Progress { done_ms: u64, total_ms: u64 },
    Stopped,
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

struct Job {
    id: u64,
    seg: SpeechSegment,
    speaker: Option<u32>,
    queued: Instant,
    /// 화자 전환으로 나뉘기 전 원래 발화의 시작 위치 (진행률 추적용)
    origin: u64,
}

/// 파일 모드 진행률용: 아직 인식이 끝나지 않은 발화들 (원래 발화 시작 위치 → 남은 조각 수)
#[derive(Default)]
struct Inflight(parking_lot::Mutex<std::collections::BTreeMap<u64, usize>>);

impl Inflight {
    fn add(&self, start: u64, n: usize) {
        *self.0.lock().entry(start).or_insert(0) += n;
    }

    fn done(&self, start: u64) {
        let mut m = self.0.lock();
        if let Some(c) = m.get_mut(&start) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                m.remove(&start);
            }
        }
    }

    fn earliest(&self) -> Option<u64> {
        self.0.lock().keys().next().copied()
    }
}

/// 파이프라인 스레드들이 함께 보는 상태
#[derive(Default)]
struct Flags {
    stop: AtomicBool,
    paused: AtomicBool,
    /// `Stopped` 를 이미 보냈는지 (정확히 한 번만 보내기 위해)
    stopped: AtomicBool,
}

impl Flags {
    fn emit_stopped(&self, sink: &EventSink) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            sink(Event::Stopped);
        }
    }
}

/// 실행 중인 녹음 · 파일 전사 세션
pub struct Pipeline {
    capture: Option<Capture>,
    flags: Arc<Flags>,
    /// 실시간: DSP 스레드 / 파일: 공급 스레드
    worker: Option<JoinHandle<()>>,
    /// 실시간 모드에서만 (파일 모드는 공급 스레드가 직접 기다린다)
    backend: Option<JoinHandle<()>>,
    sink: EventSink,
    file_mode: bool,
}

impl Pipeline {
    /// 실시간 녹음 전사를 시작한다.
    pub fn start(cfg: PipelineConfig, sink: EventSink) -> Result<Pipeline> {
        validate(&cfg)?;
        let segmenter = Segmenter::new(cfg.segmenter.clone())?;
        let recorder = match &cfg.record_path {
            Some(p) => Some(WavRecorder::create(p)?),
            None => None,
        };

        let (raw_tx, raw_rx) = crossbeam_channel::bounded::<RawChunk>(512);
        let notify: Notify = {
            let sink = sink.clone();
            Arc::new(move |message: String| sink(Event::Warning { message }))
        };
        let capture = Capture::start(&cfg.capture, raw_tx, notify)?;

        let (encoder, note) = load_encoder(&cfg);
        sink(Event::Started {
            mic: capture.mic_name.clone(),
            system: capture.system_name.clone(),
            engine: cfg.engine.label(),
            diarization: encoder.is_some(),
        });
        if let Some(message) = note {
            sink(Event::Status { message });
        }
        let backend = spawn_backend(&cfg, encoder, sink.clone(), None);
        let flags = Arc::new(Flags::default());
        let dsp = {
            let sink = sink.clone();
            let flags = flags.clone();
            let source = cfg.capture.source;
            let seg_tx = backend.seg_tx;
            std::thread::Builder::new()
                .name("bd-dsp".into())
                .spawn(move || dsp_thread(raw_rx, source, segmenter, seg_tx, sink, recorder, flags))?
        };
        Ok(Pipeline {
            capture: Some(capture),
            flags,
            worker: Some(dsp),
            backend: Some(backend.handle),
            sink,
            file_mode: false,
        })
    }

    /// 오디오 파일 전사를 시작한다. 형식 문제는 바로 에러로 돌려주고, 나머지는 백그라운드에서 진행한다.
    ///
    /// 파일을 다 읽고 인식까지 끝나면 `Stopped` 가 온다. `cfg.capture` · `cfg.record_path` 는 쓰지 않는다.
    pub fn start_file(path: &Path, cfg: PipelineConfig, sink: EventSink) -> Result<Pipeline> {
        validate(&cfg)?;
        let file = AudioFile::open(path)?;
        let segmenter = Segmenter::new(cfg.segmenter.clone())?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        let (encoder, note) = load_encoder(&cfg);
        sink(Event::Started {
            mic: None,
            system: Some(name),
            engine: cfg.engine.label(),
            diarization: encoder.is_some(),
        });
        if let Some(message) = note {
            sink(Event::Status { message });
        }
        let inflight = Arc::new(Inflight::default());
        let backend = spawn_backend(&cfg, encoder, sink.clone(), Some(inflight.clone()));
        // 인식이 밀린 만큼만 앞서 디코딩한다 (결과가 차례로 나오고 메모리도 일정)
        let max_backlog = 2 * cfg.engine.concurrency();
        let flags = Arc::new(Flags::default());
        let worker = {
            let sink = sink.clone();
            let flags = flags.clone();
            std::thread::Builder::new()
                .name("bd-file".into())
                .spawn(move || file_thread(file, segmenter, backend, inflight, max_backlog, sink, flags))?
        };
        Ok(Pipeline {
            capture: None,
            flags,
            worker: Some(worker),
            backend: None,
            sink,
            file_mode: true,
        })
    }

    /// 일시정지 / 다시 시작. 실시간 모드는 그동안 무음으로 채워 시간·녹음 파일이 실제 시각과 맞는다.
    pub fn set_paused(&self, paused: bool) {
        self.flags.paused.store(paused, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.flags.paused.load(Ordering::SeqCst)
    }

    /// 멈춘다. 남은 발화 인식이 끝나면 `Stopped` 이벤트가 온다. (즉시 반환)
    pub fn stop(mut self) {
        self.flags.stop.store(true, Ordering::SeqCst);
        if self.file_mode {
            // 공급 스레드가 곧바로 멈추고, 이미 넘긴 발화 인식이 끝나면 Stopped 를 보낸다
            return;
        }
        let capture = self.capture.take();
        let dsp = self.worker.take();
        let backend = self.backend.take();
        let sink = self.sink.clone();
        let flags = self.flags.clone();
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
            flags.emit_stopped(&sink);
        });
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // stop() 없이 버려져도 파일 공급 스레드가 끝까지 돌지 않게 (막지 않음)
        self.flags.stop.store(true, Ordering::SeqCst);
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

/// 화자 임베딩 모델을 불러온다 → (인코더, 사용자에게 알릴 상태 메시지)
fn load_encoder(cfg: &PipelineConfig) -> (Option<SpeakerEncoder>, Option<String>) {
    if !cfg.diarization.enabled {
        return (None, None);
    }
    match cfg.speaker_model.as_deref() {
        Some(p) if p.exists() => match SpeakerEncoder::new(p) {
            Ok(e) => (Some(e), None),
            Err(e) => (None, Some(format!("화자 분리를 끕니다: {e}"))),
        },
        _ => (None, Some("화자 인식 모델이 없어 화자 분리 없이 진행합니다.".into())),
    }
}

/// 디스패처 + ASR 워커. `seg_tx` 를 닫으면 남은 일을 끝내고 `handle` 이 끝난다.
struct Backend {
    seg_tx: Sender<SpeechSegment>,
    /// 대기 중인 Job 수를 보기 위한 수신 측 복제 (보내는 쪽이 아니라 종료에는 영향 없음)
    job_rx: Receiver<Job>,
    handle: JoinHandle<()>,
}

impl Backend {
    /// 아직 인식을 시작하지 않은 발화 수
    fn backlog(&self) -> usize {
        self.seg_tx.len() + self.job_rx.len()
    }
}

/// `inflight` 를 주면 발화마다 인식이 끝났는지 기록한다 (파일 모드 진행률)
fn spawn_backend(
    cfg: &PipelineConfig,
    encoder: Option<SpeakerEncoder>,
    sink: EventSink,
    inflight: Option<Arc<Inflight>>,
) -> Backend {
    let (seg_tx, seg_rx) = crossbeam_channel::unbounded::<SpeechSegment>();
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Job>();

    let n = cfg.engine.concurrency();
    let mut workers = Vec::with_capacity(n);
    for i in 0..n {
        let rx = job_rx.clone();
        let sink = sink.clone();
        let engine = cfg.engine.clone();
        let lang = cfg.language.clone();
        let prompt = cfg.prompt.clone();
        let inflight = inflight.clone();
        workers.push(
            std::thread::Builder::new()
                .name(format!("bd-asr-{i}"))
                .spawn(move || asr_worker(i, rx, engine, lang, prompt, sink, inflight))
                .expect("spawn asr worker"),
        );
    }

    let dcfg = cfg.diarization.clone();
    let handle = std::thread::Builder::new()
        .name("bd-dispatch".into())
        .spawn(move || {
            dispatcher(seg_rx, job_tx, encoder, dcfg, sink, inflight);
            for w in workers {
                let _ = w.join();
            }
        })
        .expect("spawn dispatcher");
    Backend { seg_tx, job_rx, handle }
}

fn dispatcher(
    seg_rx: Receiver<SpeechSegment>,
    job_tx: Sender<Job>,
    mut encoder: Option<SpeakerEncoder>,
    cfg: DiarizationConfig,
    sink: EventSink,
    inflight: Option<Arc<Inflight>>,
) {
    let mut tracker = SpeakerTracker::new(cfg.clone());
    let mut next_id = 0u64;
    for seg in seg_rx {
        let origin = seg.start;
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
        if let Some(t) = &inflight {
            // 원래 발화 1개 → 조각 n개 (늘린 뒤 줄여서 중간에 0 이 되지 않게)
            t.add(origin, parts.len());
            t.done(origin);
        }
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
                    origin,
                })
                .is_err()
            {
                return;
            }
        }
    }
}

struct InflightDone<'a>(&'a Inflight, u64);

impl Drop for InflightDone<'_> {
    fn drop(&mut self) {
        self.0.done(self.1);
    }
}

fn asr_worker(
    idx: usize,
    rx: Receiver<Job>,
    engine_cfg: EngineConfig,
    lang: String,
    prompt: String,
    sink: EventSink,
    inflight: Option<Arc<Inflight>>,
) {
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
    // API 가 연달아 실패하면(인터넷 끊김·키 오류) 20초 동안은 부르지 않고 바로 실패 처리한다.
    // 한 번에 30초 넘게 걸리는 재시도가 쌓여 정지·종료가 몇 시간씩 걸리지 않게. (녹음 파일은 남는다)
    let mut fails = 0u32;
    let mut skip_until: Option<Instant> = None;
    for job in rx {
        // 결과가 어떻든 이 조각 처리가 끝나면 진행률에 반영
        let _done = inflight.as_deref().map(|t| InflightDone(t, job.origin));
        if skip_until.is_some_and(|t| Instant::now() < t) {
            sink(Event::Failed {
                id: job.id,
                message: "연결 문제로 잠시 건너뜀 (녹음 파일에는 남아요)".into(),
            });
            continue;
        }
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
        let result = engine.transcribe(&req);
        if result.is_ok() {
            fails = 0;
            skip_until = None;
        } else if !is_local {
            fails += 1;
            if fails >= 3 {
                skip_until = Some(Instant::now() + Duration::from_secs(20));
            }
        }
        match result {
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

const MIC_SILENT_MSG: &str =
    "마이크에서 소리가 들어오지 않아요. 마이크 음소거나 Windows 설정 › 개인 정보 › 마이크 접근을 확인해 주세요.";

/// 마이크가 실제로 소리를 보내는지 점검한다.
/// Windows 개인 정보 설정으로 막혔거나 하드웨어 음소거면 데이터가 안 오거나 디지털 0 만 온다.
/// (조용히 있어도 실제 마이크는 잡음이 1e-4 보다 훨씬 크다)
struct MicCheck {
    /// 일시정지를 뺀 녹음 시간
    active: Duration,
    last_tick: Instant,
    /// 받은 마이크 오디오 길이 (초)
    secs: f64,
    peak: f32,
    done: bool,
}

impl MicCheck {
    const AFTER: Duration = Duration::from_secs(8);
    const MIN_SECS: f64 = 1.0;
    const MIN_PEAK: f32 = 1e-4;

    fn new() -> Self {
        Self {
            active: Duration::ZERO,
            last_tick: Instant::now(),
            secs: 0.0,
            peak: 0.0,
            done: false,
        }
    }

    fn feed(&mut self, c: &RawChunk, paused: bool) {
        if self.done || paused || c.source != SRC_MIC {
            return;
        }
        self.secs += c.samples.len() as f64 / c.rate.max(1) as f64;
        self.peak = c.samples.iter().fold(self.peak, |m, s| m.max(s.abs()));
    }

    /// 녹음 시간이 충분히 쌓이면 한 번 판정한다 → 문제가 있으면 true
    fn tick(&mut self, paused: bool) -> bool {
        let now = Instant::now();
        if !paused {
            self.active += now - self.last_tick;
        }
        self.last_tick = now;
        if self.done || self.active < Self::AFTER {
            return false;
        }
        self.done = true;
        self.secs < Self::MIN_SECS || self.peak < Self::MIN_PEAK
    }
}

fn dsp_thread(
    rx: Receiver<RawChunk>,
    source: SourceKind,
    mut segmenter: Segmenter,
    seg_tx: Sender<SpeechSegment>,
    sink: EventSink,
    mut recorder: Option<WavRecorder>,
    flags: Arc<Flags>,
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
    let mut mic_check = source.uses_mic().then(MicCheck::new);

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
        let paused = flags.paused.load(Ordering::Relaxed);
        match rx.recv_timeout(Duration::from_millis(40)) {
            Ok(c) => {
                if let Some(m) = mic_check.as_mut() {
                    m.feed(&c, paused);
                }
                ingest(c, &mut resamplers, &mut queues);
                last_data = Instant::now();
                while let Ok(c) = rx.try_recv() {
                    if let Some(m) = mic_check.as_mut() {
                        m.feed(&c, paused);
                    }
                    ingest(c, &mut resamplers, &mut queues);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if mic_check.as_mut().is_some_and(|m| m.tick(paused)) {
            sink(Event::Warning {
                message: MIC_SILENT_MSG.into(),
            });
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
        if mixed.is_empty() && last_data.elapsed() > Duration::from_millis(300) && !flags.stop.load(Ordering::Relaxed)
        {
            let expected = (started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
            if expected > produced + 4800 {
                let fill = ((expected - produced - 1600) as usize).min(SAMPLE_RATE as usize);
                mixed.resize(fill, 0.0);
            }
        }
        if mixed.is_empty() {
            continue;
        }
        // 일시정지: 같은 길이의 무음으로 바꾼다 (타임스탬프·녹음 파일은 실제 시각과 맞고, 하던 발화는 VAD 가 자연스럽게 끝냄)
        if paused {
            mixed.fill(0.0);
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
                rms: if paused { 0.0 } else { (level_sum / level_n as f64).sqrt() as f32 },
                speaking: !paused && segmenter.is_speaking(),
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

/// 파일 모드 공급 스레드: 디코딩 → VAD 분할 → 백엔드. 끝나면(다 읽었거나 stop) 남은 인식을 기다려 `Stopped`.
fn file_thread(
    mut file: AudioFile,
    mut segmenter: Segmenter,
    backend: Backend,
    inflight: Arc<Inflight>,
    max_backlog: usize,
    sink: EventSink,
    flags: Arc<Flags>,
) {
    const IDLE: Duration = Duration::from_millis(30);
    // 어떻게 끝나든(패닉 포함) Stopped 는 한 번 보낸다
    let stopped = StoppedGuard { flags, sink: sink.clone() };
    let flags = &stopped.flags;
    let mut progress = ProgressReporter::new(sink.clone(), file.duration_ms().unwrap_or(0));
    progress.report(0, true);
    let mut fed: u64 = 0;
    let mut buf = Vec::with_capacity(SAMPLE_RATE as usize);
    let mut segs = Vec::new();
    let mut finished = false;

    while !flags.stop.load(Ordering::Relaxed) {
        if backend.handle.is_finished() {
            // 백엔드가 먼저 죽었다: 더 넘겨도 처리할 곳이 없다
            break;
        }
        if flags.paused.load(Ordering::Relaxed) || backend.backlog() >= max_backlog {
            // 기다리는 동안에도 인식이 끝난 만큼 진행률을 따라잡는다
            progress.report(recognized_ms(&segmenter, fed, &inflight), false);
            std::thread::sleep(IDLE);
            continue;
        }
        buf.clear();
        match file.read(&mut buf) {
            Ok(true) => {}
            Ok(false) => {
                finished = true;
                break;
            }
            Err(e) => {
                sink(Event::Error { message: e.to_string() });
                break;
            }
        }
        if let Err(e) = segmenter.push(&buf, &mut segs) {
            sink(Event::Error {
                message: format!("VAD 오류: {e}"),
            });
            break;
        }
        fed += buf.len() as u64;
        for s in segs.drain(..) {
            inflight.add(s.start, 1);
            let _ = backend.seg_tx.send(s);
        }
        progress.report(recognized_ms(&segmenter, fed, &inflight), false);
    }

    // 진행 중이던 발화까지 넘기고, 이미 넘긴 발화 인식이 끝나길 기다린다
    segmenter.flush(&mut segs);
    for s in segs.drain(..) {
        inflight.add(s.start, 1);
        let _ = backend.seg_tx.send(s);
    }
    let Backend { seg_tx, job_rx, handle } = backend;
    drop(seg_tx);
    drop(job_rx);
    while !handle.is_finished() {
        progress.report(recognized_ms(&segmenter, fed, &inflight), false);
        std::thread::sleep(IDLE);
    }
    let _ = handle.join();
    let last = if finished {
        // 다 읽었으면 100%
        samples_to_ms(fed).max(progress.total_ms)
    } else {
        recognized_ms(&segmenter, fed, &inflight)
    };
    progress.report(last, true);
    drop(stopped);
}

/// 인식까지 끝난 위치 (ms): 아직 인식 중이거나 VAD 가 붙잡고 있는 발화 중 가장 앞선 것의 시작 전까지
fn recognized_ms(segmenter: &Segmenter, fed: u64, inflight: &Inflight) -> u64 {
    let settled = segmenter.settled().min(fed);
    samples_to_ms(inflight.earliest().map_or(settled, |s| s.min(settled)))
}

/// 진행률 이벤트: 값이 바뀌었고 마지막으로 보낸 지 300 ms 가 지났을 때만 (force 면 바로)
struct ProgressReporter {
    sink: EventSink,
    total_ms: u64,
    last_ms: Option<u64>,
    last_at: Instant,
}

impl ProgressReporter {
    const EVERY: Duration = Duration::from_millis(300);

    fn new(sink: EventSink, total_ms: u64) -> Self {
        Self {
            sink,
            total_ms,
            last_ms: None,
            last_at: Instant::now(),
        }
    }

    fn report(&mut self, done_ms: u64, force: bool) {
        // 디코더·리샘플러 반올림으로 길이보다 1~2 ms 넘칠 수 있다
        let done_ms = if self.total_ms > 0 { done_ms.min(self.total_ms) } else { done_ms };
        if self.last_ms == Some(done_ms) || (!force && self.last_at.elapsed() < Self::EVERY) {
            return;
        }
        (self.sink)(Event::Progress {
            done_ms,
            total_ms: self.total_ms,
        });
        self.last_ms = Some(done_ms);
        self.last_at = Instant::now();
    }
}

struct StoppedGuard {
    flags: Arc<Flags>,
    sink: EventSink,
}

impl Drop for StoppedGuard {
    fn drop(&mut self) {
        self.flags.emit_stopped(&self.sink);
    }
}

/// 오디오 파일 전사 (wav · mp3 · m4a · flac · ogg). 끝날 때까지 블록한다.
pub fn transcribe_file(path: &Path, cfg: &PipelineConfig, sink: EventSink) -> Result<()> {
    let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
    let inner: EventSink = Arc::new(move |e: Event| {
        let last = matches!(e, Event::Stopped);
        sink(e);
        if last {
            let _ = done_tx.try_send(());
        }
    });
    let pipeline = Pipeline::start_file(path, cfg.clone(), inner)?;
    loop {
        match done_rx.recv_timeout(Duration::from_millis(200)) {
            Err(RecvTimeoutError::Timeout) if pipeline.worker.as_ref().is_some_and(|h| !h.is_finished()) => {}
            _ => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_events_json_shape() {
        let p = serde_json::to_string(&Event::Progress { done_ms: 1500, total_ms: 60000 }).unwrap();
        assert_eq!(p, r#"{"type":"progress","doneMs":1500,"totalMs":60000}"#);
        let w = serde_json::to_string(&Event::Warning { message: "경고".into() }).unwrap();
        assert_eq!(w, r#"{"type":"warning","message":"경고"}"#);
    }

    #[test]
    fn inflight_tracks_earliest_unfinished() {
        let t = Inflight::default();
        t.add(100, 1);
        t.add(500, 1);
        // 디스패처: 100 에서 시작한 발화가 두 조각으로 나뉨
        t.add(100, 2);
        t.done(100);
        assert_eq!(t.earliest(), Some(100));
        t.done(100);
        assert_eq!(t.earliest(), Some(100));
        t.done(100);
        assert_eq!(t.earliest(), Some(500));
        t.done(500);
        assert_eq!(t.earliest(), None);
    }
}
