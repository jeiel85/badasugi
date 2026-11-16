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
use crate::diarize::{Diarizer, KnownVoice, SpeakerHints, UtteranceVoice};
use crate::resample::Resampler;
use crate::speaker::{DiarizationConfig, SpeakerEncoder};
use crate::vad::{Segmenter, SegmenterConfig, SpeechSegment};
use crate::ogg_opus::OpusRecorder;
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
    /// 지정하면 녹음도 남긴다: 확장자 `.wav` 면 16 kHz 모노 PCM16, 그 밖(`.opus`)은 Ogg Opus 24 kbps (WAV 의 약 1/10)
    #[serde(default)]
    pub record_path: Option<PathBuf>,
    /// 이름을 아는 목소리 — 녹음 중 화자 목소리가 닮으면 그 이름을 알려 준다 ([`Event::SpeakerNamed`])
    #[serde(default)]
    pub known_voices: Vec<KnownVoice>,
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
    /// 인식 실패. `retry` 면 들고 있다가 녹음이 끝나면 한 번 더 받아 적는다 (그때 `Transcript` 나 다시 `Failed` 가 온다)
    #[serde(rename_all = "camelCase")]
    Failed { id: u64, message: String, retry: bool },
    /// API 로 실제로 보낸 인식 요청 (사용량 집계용 — 화면에는 보내지 않아도 된다)
    #[serde(rename_all = "camelCase")]
    ApiUsage { audio_ms: u64, ok: bool },
    #[serde(rename_all = "camelCase")]
    Status { message: String },
    /// 사용자가 꼭 봐야 하는 문제 (마이크 무음 · 장치 끊김/재연결)
    #[serde(rename_all = "camelCase")]
    Warning { message: String },
    #[serde(rename_all = "camelCase")]
    Error { message: String },
    /// 등록된 목소리로 화자를 알아봄 (화자마다 한 번)
    #[serde(rename_all = "camelCase")]
    SpeakerNamed { speaker: u32, name: String },
    /// 모든 발화 인식이 끝난 뒤(`Stopped` 바로 전) 회의 전체를 보고 다시 정리한 화자
    #[serde(rename_all = "camelCase")]
    SpeakersRefined {
        /// [발화 id, 실시간 화자, 다시 정리한 화자] — 모든 조각. 다시 정리한 화자는 사람이 합친 화자를 지킨다.
        labels: Vec<[u64; 3]>,
        /// 믿을 만한 조각의 목소리 (문장별) — 이름을 기억해 두었다가 다음 회의에서 알아보는 데 쓴다
        utterances: Vec<UtteranceVoice>,
    },
    /// 파일 전사 진행: 인식까지 끝난 위치 / 전체 길이 (모르면 0). 앞으로만 움직이고, 다 끝나면 done = total.
    #[serde(rename_all = "camelCase")]
    Progress { done_ms: u64, total_ms: u64 },
    Stopped,
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// 사람이 녹음 중에 합친 화자 · 이름 붙인 화자. 회의 후 다시 정리가 그 결정을 되돌리지 않게 알려 둔다.
pub type SharedHints = Arc<parking_lot::Mutex<SpeakerHints>>;

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
    hints: SharedHints,
}

impl Pipeline {
    /// 사람이 합친 화자 · 이름 붙인 화자를 알려 주는 손잡이 — 녹음이 끝나 정리하는 동안에도 고칠 수 있게 복제해 둘 수 있다
    pub fn speaker_hints(&self) -> SharedHints {
        self.hints.clone()
    }

    /// 실시간 녹음 전사를 시작한다.
    pub fn start(cfg: PipelineConfig, sink: EventSink) -> Result<Pipeline> {
        validate(&cfg)?;
        let segmenter = Segmenter::new(cfg.segmenter.clone())?;
        let recorder = match &cfg.record_path {
            Some(p) => Some(Recorder::create(p)?),
            None => None,
        };

        let (raw_tx, raw_rx) = crossbeam_channel::bounded::<RawChunk>(512);
        let notify: Notify = {
            let sink = sink.clone();
            Arc::new(move |message: String| sink(Event::Warning { message }))
        };
        let capture = Capture::start(&cfg.capture, raw_tx, notify)?;

        let (diarizer, note) = load_diarizer(&cfg);
        sink(Event::Started {
            mic: capture.mic_name.clone(),
            system: capture.system_name.clone(),
            engine: cfg.engine.label(),
            diarization: diarizer.is_some(),
        });
        if let Some(message) = note {
            sink(Event::Status { message });
        }
        let hints = SharedHints::default();
        let backend = spawn_backend(&cfg, diarizer, sink.clone(), None, hints.clone(), None);
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
            hints,
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

        let (diarizer, note) = load_diarizer(&cfg);
        sink(Event::Started {
            mic: None,
            system: Some(name),
            engine: cfg.engine.label(),
            diarization: diarizer.is_some(),
        });
        if let Some(message) = note {
            sink(Event::Status { message });
        }
        let inflight = Arc::new(Inflight::default());
        let hints = SharedHints::default();
        let flags = Arc::new(Flags::default());
        let backend = spawn_backend(
            &cfg,
            diarizer,
            sink.clone(),
            Some(inflight.clone()),
            hints.clone(),
            Some(flags.clone()),
        );
        // 인식이 밀린 만큼만 앞서 디코딩한다 (결과가 차례로 나오고 메모리도 일정)
        let max_backlog = 2 * cfg.engine.concurrency();
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
            hints,
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

/// 화자 임베딩 모델을 불러와 화자 구분을 준비한다 → (화자 구분, 사용자에게 알릴 상태 메시지)
fn load_diarizer(cfg: &PipelineConfig) -> (Option<Diarizer>, Option<String>) {
    if !cfg.diarization.enabled {
        return (None, None);
    }
    match cfg.speaker_model.as_deref() {
        Some(p) if p.exists() => match SpeakerEncoder::new(p) {
            Ok(e) => (
                Some(Diarizer::new(e, cfg.diarization.clone(), cfg.known_voices.clone())),
                None,
            ),
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
    diarizer: Option<Diarizer>,
    sink: EventSink,
    inflight: Option<Arc<Inflight>>,
    hints: SharedHints,
    file_flags: Option<Arc<Flags>>,
) -> Backend {
    let (seg_tx, seg_rx) = crossbeam_channel::unbounded::<SpeechSegment>();
    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Job>();

    let ctx = Arc::new(WorkerCtx {
        lang: cfg.language.clone(),
        prompt: cfg.prompt.clone(),
        sink: sink.clone(),
        inflight: inflight.clone(),
        recorded: cfg.record_path.is_some(),
        file_flags,
        waiting_noted: Default::default(),
        retry_noted: AtomicBool::new(false),
        gave_up: AtomicBool::new(false),
        offline: AtomicBool::new(false),
    });
    let n = cfg.engine.concurrency();
    let mut workers = Vec::with_capacity(n);
    for i in 0..n {
        let rx = job_rx.clone();
        let engine = cfg.engine.clone();
        let ctx = ctx.clone();
        workers.push(
            std::thread::Builder::new()
                .name(format!("bd-asr-{i}"))
                .spawn(move || asr_worker(i, rx, engine, ctx))
                .expect("spawn asr worker"),
        );
    }

    let handle = std::thread::Builder::new()
        .name("bd-dispatch".into())
        .spawn(move || {
            let diarizer = dispatcher(seg_rx, job_tx, diarizer, sink.clone(), inflight);
            for w in workers {
                let _ = w.join();
            }
            // 모든 발화 인식이 끝난 뒤, 회의 전체를 보고 화자를 다시 정리한다 (Stopped 보다 먼저)
            if let Some(d) = diarizer.filter(|d| !d.is_empty()) {
                let t = Instant::now();
                let hints = hints.lock().clone();
                let r = d.finish(&hints);
                log::info!(
                    "화자 다시 정리: 조각 {}개 중 {}개 옮김 · 화자 {}명 ({}ms)",
                    r.labels.len(),
                    r.moved(),
                    r.voices.len(),
                    t.elapsed().as_millis()
                );
                sink(Event::SpeakersRefined {
                    labels: r.labels.iter().map(|&(id, a, b)| [id, a as u64, b as u64]).collect(),
                    utterances: r.utterances,
                });
            }
        })
        .expect("spawn dispatcher");
    Backend { seg_tx, job_rx, handle }
}

/// 발화를 화자별 조각으로 나눠 ASR 워커에 넘긴다. 입력이 닫히면 화자 구분 상태를 돌려준다 (다시 정리용).
fn dispatcher(
    seg_rx: Receiver<SpeechSegment>,
    job_tx: Sender<Job>,
    mut diarizer: Option<Diarizer>,
    sink: EventSink,
    inflight: Option<Arc<Inflight>>,
) -> Option<Diarizer> {
    let mut next_id = 0u64;
    for seg in seg_rx {
        let origin = seg.start;
        let parts = match diarizer.as_mut() {
            Some(d) => d.split(seg),
            None => vec![seg],
        };
        if let Some(t) = &inflight {
            // 원래 발화 1개 → 조각 n개 (늘린 뒤 줄여서 중간에 0 이 되지 않게)
            t.add(origin, parts.len());
            t.done(origin);
        }
        for part in parts {
            let id = next_id;
            next_id += 1;
            let speaker = diarizer.as_mut().map(|d| d.assign(id, &part));
            sink(Event::Pending {
                id,
                start_ms: samples_to_ms(part.start),
                end_ms: samples_to_ms(part.end),
                speaker,
            });
            if let (Some(d), Some(spk)) = (diarizer.as_mut(), speaker) {
                if let Some(name) = d.recognize(spk) {
                    log::info!("화자 {spk} 를 등록된 목소리로 알아봄");
                    sink(Event::SpeakerNamed { speaker: spk, name });
                }
            }
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
                return diarizer;
            }
        }
    }
    diarizer
}

struct InflightDone<'a>(&'a Inflight, u64);

impl Drop for InflightDone<'_> {
    fn drop(&mut self) {
        self.0.done(self.1);
    }
}

/// ASR 워커들이 함께 보는 설정 · 상태
struct WorkerCtx {
    lang: String,
    prompt: String,
    sink: EventSink,
    inflight: Option<Arc<Inflight>>,
    /// 실시간 녹음이 파일로도 남는지 (실패 안내 문구용)
    recorded: bool,
    /// 파일 모드: 연결 문제면 건너뛰지 않고 기다렸다가 다시 보내고, 오래 안 되면 받아쓰기를 멈춘다
    file_flags: Option<Arc<Flags>>,
    /// 마지막으로 "다시 보낼게요" 를 알린 때 (워커 여럿이 같은 안내를 겹쳐 띄우지 않게)
    waiting_noted: parking_lot::Mutex<Option<Instant>>,
    /// "받아 적지 못한 발화를 다시…" 를 알렸는지
    retry_noted: AtomicBool,
    /// 파일 모드에서 연결이 계속 안 돼 멈췄는지 — 남은 발화는 기다리지 않고 바로 실패
    gave_up: AtomicBool,
    /// 파일 모드에서 마지막 요청이 연결 문제로 실패했는지 — 그 상태로 멈추면 남은 발화를 하나씩 기다리지 않는다
    offline: AtomicBool,
}

impl WorkerCtx {
    fn emit(&self, e: Event) {
        (self.sink)(e)
    }

    fn fail(&self, id: u64, message: String, retry: bool) {
        self.emit(Event::Failed { id, message, retry });
    }

    /// 다시 받아 적지 못하고 버릴 때 덧붙이는 말
    fn kept_note(&self) -> &'static str {
        if self.recorded {
            " (녹음 파일에는 남아요)"
        } else {
            ""
        }
    }

    fn should_note_waiting(&self) -> bool {
        let mut last = self.waiting_noted.lock();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }
}

fn asr_worker(idx: usize, rx: Receiver<Job>, engine_cfg: EngineConfig, ctx: Arc<WorkerCtx>) {
    let is_local = matches!(engine_cfg, EngineConfig::Local { .. });
    if is_local && idx == 0 {
        ctx.emit(Event::Status {
            message: "음성 인식 모델을 불러오는 중…".into(),
        });
    }
    let t = Instant::now();
    let engine = match asr::create(&engine_cfg) {
        Ok(e) => {
            if is_local && idx == 0 {
                ctx.emit(Event::Status {
                    message: format!("모델 준비 완료 ({:.1}초)", t.elapsed().as_secs_f32()),
                });
            }
            Some(e)
        }
        Err(e) => {
            if idx == 0 {
                ctx.emit(Event::Error {
                    message: format!("음성 인식 엔진을 시작하지 못했습니다: {e}"),
                });
            }
            None
        }
    };
    run_worker(rx, engine, !is_local, &ctx);
}

/// 실시간 녹음에서 연결 문제로 받아 적지 못한 발화를 녹음이 끝날 때까지 들고 있는 양 (워커마다, 16 kHz 샘플) — 5분
const LATER_MAX_SAMPLES: usize = 5 * 60 * SAMPLE_RATE as usize;

/// 녹음이 끝나면 다시 받아 적을 발화
#[derive(Default)]
struct Later {
    jobs: Vec<Job>,
    samples: usize,
}

impl Later {
    /// 넣을 자리가 없으면 돌려준다
    fn keep(&mut self, job: Job) -> std::result::Result<(), Job> {
        let n = job.seg.samples.len();
        if self.samples + n > LATER_MAX_SAMPLES {
            return Err(job);
        }
        self.samples += n;
        self.jobs.push(job);
        Ok(())
    }
}

fn run_worker(rx: Receiver<Job>, mut engine: Option<Box<dyn asr::Transcriber>>, is_api: bool, ctx: &WorkerCtx) {
    // API 가 연달아 실패하면(인터넷 끊김 등) 20초 동안은 부르지 않는다 — 한 번에 수십 초 걸리는 재시도가 쌓여
    // 정지·종료가 몇 시간씩 걸리지 않게. 그동안 들어온 발화는 들고 있다가 녹음이 끝나면 다시 받아 적는다.
    let mut fails = 0u32;
    let mut skip_until: Option<Instant> = None;
    let mut later = Later::default();
    let defer = |later: &mut Later, job: Job, reason: String| {
        let id = job.id;
        match later.keep(job) {
            Ok(()) => ctx.fail(id, format!("{reason} — 녹음을 멈추면 다시 받아 적어요"), true),
            Err(_) => ctx.fail(id, format!("{reason}{}", ctx.kept_note()), false),
        }
    };
    for job in rx {
        // 결과가 어떻든 이 조각 처리가 끝나면 진행률에 반영
        let _done = ctx.inflight.as_deref().map(|t| InflightDone(t, job.origin));
        let Some(engine) = engine.as_deref_mut() else {
            ctx.fail(job.id, "엔진 초기화 실패".into(), false);
            continue;
        };
        if let Some(flags) = &ctx.file_flags {
            if is_api {
                transcribe_patiently(engine, &job, flags, ctx);
            } else {
                let t0 = Instant::now();
                match recognize(engine, &job, ctx, false) {
                    Ok(raw) => emit_text(ctx, &job, raw, t0),
                    Err(e) => ctx.fail(job.id, e.to_string(), false),
                }
            }
            continue;
        }
        if skip_until.is_some_and(|t| Instant::now() < t) {
            defer(&mut later, job, "연결 문제로 잠시 미뤄 둠".into());
            continue;
        }
        let t0 = Instant::now();
        match recognize(engine, &job, ctx, is_api) {
            Ok(raw) => {
                fails = 0;
                skip_until = None;
                emit_text(ctx, &job, raw, t0);
            }
            // 연결 문제 · 요청 한도 · 서비스 장애: 들고 있다가 녹음이 끝나면 다시
            Err(e) if is_api && asr::retryable(&e) => {
                fails += 1;
                if fails >= 3 {
                    skip_until = Some(Instant::now() + Duration::from_secs(20));
                }
                defer(&mut later, job, e.to_string());
            }
            Err(e) => ctx.fail(job.id, e.to_string(), false),
        }
    }
    if let Some(engine) = engine.as_deref_mut() {
        retry_later(later, engine, ctx);
    }
}

/// 녹음을 멈춘 뒤: 미뤄 둔 발화를 한 번 더 받아 적는다. 그래도 안 되면 나머지는 바로 실패로 (정지가 길어지지 않게)
fn retry_later(later: Later, engine: &mut dyn asr::Transcriber, ctx: &WorkerCtx) {
    if later.jobs.is_empty() {
        return;
    }
    if !ctx.retry_noted.swap(true, Ordering::SeqCst) {
        ctx.emit(Event::Status {
            message: "받아 적지 못한 발화를 다시 받아 적는 중…".into(),
        });
    }
    let mut gave_up: Option<String> = None;
    for job in later.jobs {
        if let Some(m) = &gave_up {
            ctx.fail(job.id, m.clone(), false);
            continue;
        }
        let t0 = Instant::now();
        match recognize(engine, &job, ctx, true) {
            Ok(raw) => emit_text(ctx, &job, raw, t0),
            Err(e) => {
                let m = format!("{e}{}", ctx.kept_note());
                ctx.fail(job.id, m.clone(), false);
                gave_up = Some(m);
            }
        }
    }
}

/// 파일 모드에서 기다리는 단위 — 5초, 10초, 20초, 30초, 30초… 쉬었다 다시 보내고, 3분 넘게 안 되면 멈춘다
#[cfg(not(test))]
const FILE_WAIT_UNIT: Duration = Duration::from_secs(5);
#[cfg(test)]
const FILE_WAIT_UNIT: Duration = Duration::from_millis(5);

/// 파일 모드: 연결 문제면 건너뛰지 않고 기다렸다가 같은 발화를 다시 보낸다 (그동안 공급도 함께 쉰다).
/// 오래 안 되면 파일 받아쓰기를 멈춘다 — 받아 적은 데까지는 기록에 남는다.
fn transcribe_patiently(engine: &mut dyn asr::Transcriber, job: &Job, flags: &Flags, ctx: &WorkerCtx) {
    let started = Instant::now();
    let mut attempt = 0usize;
    loop {
        let offline_stop = flags.stop.load(Ordering::SeqCst) && ctx.offline.load(Ordering::SeqCst);
        if ctx.gave_up.load(Ordering::SeqCst) || offline_stop {
            ctx.fail(job.id, "연결 문제로 받아쓰기를 멈춤".into(), false);
            return;
        }
        let t0 = Instant::now();
        let e = match recognize(engine, job, ctx, true) {
            Ok(raw) => {
                ctx.offline.store(false, Ordering::SeqCst);
                return emit_text(ctx, job, raw, t0);
            }
            Err(e) => e,
        };
        if asr::retryable(&e) {
            ctx.offline.store(true, Ordering::SeqCst);
        }
        if !asr::retryable(&e) || flags.stop.load(Ordering::SeqCst) {
            return ctx.fail(job.id, e.to_string(), false);
        }
        if started.elapsed() >= FILE_WAIT_UNIT * 36 {
            if !ctx.gave_up.swap(true, Ordering::SeqCst) {
                flags.stop.store(true, Ordering::SeqCst);
                ctx.emit(Event::Error {
                    message: format!(
                        "연결이 계속 안 돼 파일 받아쓰기를 멈췄어요. 받아 적은 데까지는 기록에 남아요. ({e})"
                    ),
                });
            }
            return ctx.fail(job.id, e.to_string(), false);
        }
        let wait = FILE_WAIT_UNIT * [1, 2, 4, 6][attempt.min(3)];
        // 요청 한도에 걸렸으면 공급자가 알려 준 만큼 (1분까지)
        let wait = e
            .downcast_ref::<asr::openai::ApiError>()
            .and_then(asr::openai::ApiError::retry_after)
            .map_or(wait, |d| d.clamp(wait, FILE_WAIT_UNIT * 12));
        if ctx.should_note_waiting() {
            ctx.emit(Event::Warning {
                message: format!("{e} — {}초 뒤 다시 보내요", wait.as_secs().max(1)),
            });
        }
        // 기다리는 동안 멈추면 바로 그만둔다
        let until = Instant::now() + wait;
        while Instant::now() < until && !flags.stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(50).min(wait));
        }
        attempt += 1;
    }
}

/// 한 번 인식한다. API 면 실제로 보낸 요청을 사용량으로 알린다.
fn recognize(engine: &mut dyn asr::Transcriber, job: &Job, ctx: &WorkerCtx, is_api: bool) -> Result<String> {
    let r = engine.transcribe(&AsrRequest {
        samples: &job.seg.samples,
        language: &ctx.lang,
        prompt: &ctx.prompt,
    });
    if is_api {
        ctx.emit(Event::ApiUsage {
            audio_ms: samples_to_ms(job.seg.samples.len() as u64),
            ok: r.is_ok(),
        });
    }
    r
}

fn emit_text(ctx: &WorkerCtx, job: &Job, raw: String, t0: Instant) {
    let text = crate::text::clean(&raw);
    if text.is_empty() {
        ctx.emit(Event::Discarded { id: job.id });
    } else {
        ctx.emit(Event::Transcript {
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

const MIC_SILENT_MSG: &str =
    "마이크에서 소리가 거의 들어오지 않아요. 마이크 음소거 · 입력 음량, 또는 설정 › 녹음 › 입력의 마이크를 확인해 주세요. (Windows 설정 › 개인 정보 › 마이크 접근도 확인)";

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
    /// 제곱합 · 샘플 수 (평균 소리 크기 RMS 용)
    sum_sq: f64,
    count: u64,
    done: bool,
}

impl MicCheck {
    const AFTER: Duration = Duration::from_secs(8);
    const MIN_SECS: f64 = 1.0;
    const MIN_PEAK: f32 = 1e-4;
    /// 평균 소리 크기 하한 (약 -70 dBFS). 조용한 방이라도 실제 마이크 잡음은 이보다 훨씬 크다(보통 -60~-45).
    /// 음소거 · 입력 음량 0 · 꽂혀 있지 않은 마이크는 미세한 잡음만 들어와 이보다 작다 (실사용 사례: -88 dBFS)
    const MIN_RMS: f64 = 3e-4;

    fn new() -> Self {
        Self {
            active: Duration::ZERO,
            last_tick: Instant::now(),
            secs: 0.0,
            peak: 0.0,
            sum_sq: 0.0,
            count: 0,
            done: false,
        }
    }

    fn feed(&mut self, c: &RawChunk, paused: bool) {
        if self.done || paused || c.source != SRC_MIC {
            return;
        }
        self.secs += c.samples.len() as f64 / c.rate.max(1) as f64;
        self.peak = c.samples.iter().fold(self.peak, |m, s| m.max(s.abs()));
        self.sum_sq += c.samples.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>();
        self.count += c.samples.len() as u64;
    }

    fn silent(&self) -> bool {
        let rms = if self.count > 0 { (self.sum_sq / self.count as f64).sqrt() } else { 0.0 };
        self.secs < Self::MIN_SECS || self.peak < Self::MIN_PEAK || rms < Self::MIN_RMS
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
        self.silent()
    }
}

/// 녹음 파일 쓰기 (확장자로 형식을 고른다)
enum Recorder {
    Wav(WavRecorder),
    Opus(OpusRecorder),
}

impl Recorder {
    fn create(path: &Path) -> Result<Self> {
        let wav = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav"));
        Ok(if wav {
            Recorder::Wav(WavRecorder::create(path)?)
        } else {
            Recorder::Opus(OpusRecorder::create(path)?)
        })
    }

    fn write(&mut self, samples: &[f32]) {
        match self {
            Recorder::Wav(r) => r.write(samples),
            Recorder::Opus(r) => r.write(samples),
        }
    }

    fn finish(self) -> Result<()> {
        match self {
            Recorder::Wav(r) => r.finish(),
            Recorder::Opus(r) => r.finish(),
        }
    }
}

fn dsp_thread(
    rx: Receiver<RawChunk>,
    source: SourceKind,
    mut segmenter: Segmenter,
    seg_tx: Sender<SpeechSegment>,
    sink: EventSink,
    mut recorder: Option<Recorder>,
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

    /// 정해 둔 순서대로 실패하다가 그다음부터는 성공하는 가짜 엔진
    struct Scripted(std::collections::VecDeque<anyhow::Error>, Arc<parking_lot::Mutex<u32>>);

    impl asr::Transcriber for Scripted {
        fn transcribe(&mut self, _req: &AsrRequest) -> Result<String> {
            *self.1.lock() += 1;
            match self.0.pop_front() {
                Some(e) => Err(e),
                None => Ok("결제 모듈은 금요일까지 배포할게요".into()),
            }
        }
    }

    fn net() -> anyhow::Error {
        asr::openai::ApiError::Network("서버에 연결할 수 없어요".into()).into()
    }

    fn status(code: u16) -> anyhow::Error {
        asr::openai::ApiError::Status { status: code, retry_after: None, message: "x".into() }.into()
    }

    fn job(id: u64) -> Job {
        Job {
            id,
            seg: SpeechSegment { start: id * 32_000, end: id * 32_000 + 16_000, samples: vec![0.0; 16_000], probs: vec![] },
            speaker: Some(0),
            queued: Instant::now(),
            origin: id * 32_000,
        }
    }

    /// 일을 차례로 넣고 입력을 닫은 뒤 워커를 끝까지 돌려 이벤트를 모은다
    fn run(errors: Vec<anyhow::Error>, jobs: Vec<Job>, file_flags: Option<Arc<Flags>>) -> (Vec<Event>, u32) {
        let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: EventSink = {
            let events = events.clone();
            Arc::new(move |e| events.lock().push(e))
        };
        let ctx = WorkerCtx {
            lang: "ko".into(),
            prompt: String::new(),
            sink,
            inflight: None,
            recorded: false,
            file_flags,
            waiting_noted: Default::default(),
            retry_noted: AtomicBool::new(false),
            gave_up: AtomicBool::new(false),
            offline: AtomicBool::new(false),
        };
        let calls = Arc::new(parking_lot::Mutex::new(0));
        let (tx, rx) = crossbeam_channel::unbounded();
        for j in jobs {
            tx.send(j).unwrap();
        }
        drop(tx);
        run_worker(rx, Some(Box::new(Scripted(errors.into(), calls.clone()))), true, &ctx);
        let out = std::mem::take(&mut *events.lock());
        let n = *calls.lock();
        (out, n)
    }

    fn texts(events: &[Event]) -> Vec<u64> {
        events.iter().filter_map(|e| match e { Event::Transcript { id, .. } => Some(*id), _ => None }).collect()
    }

    #[test]
    fn live_failures_are_kept_and_retried_after_stop() {
        // 연결 문제로 발화 0 이 실패 → 들고 있다가 녹음이 끝나면 다시 받아 적는다
        let (ev, calls) = run(vec![net()], vec![job(0), job(1)], None);
        assert!(ev.iter().any(|e| matches!(e, Event::Failed { id: 0, retry: true, .. })));
        assert_eq!(texts(&ev), vec![1, 0], "다시 받아 적은 발화도 문장으로 나온다");
        assert_eq!(calls, 3);
        // 실제로 보낸 요청마다 사용량 (실패 1 + 성공 2)
        let usage: Vec<bool> = ev.iter().filter_map(|e| match e { Event::ApiUsage { ok, .. } => Some(*ok), _ => None }).collect();
        assert_eq!(usage, vec![false, true, true]);
    }

    #[test]
    fn wrong_key_is_not_retried() {
        // 키 오류는 다시 보내도 같다 — 미루지 않고, "연결 문제" 로 건너뛰지도 않는다
        let (ev, calls) = run((0..4).map(|_| status(401)).collect(), (0..4).map(job).collect(), None);
        let failed: Vec<(u64, bool)> = ev.iter().filter_map(|e| match e { Event::Failed { id, retry, .. } => Some((*id, *retry)), _ => None }).collect();
        assert_eq!(failed, vec![(0, false), (1, false), (2, false), (3, false)]);
        assert!(texts(&ev).is_empty());
        assert_eq!(calls, 4);
    }

    #[test]
    fn file_mode_stop_while_offline_does_not_wait_job_by_job() {
        let flags = Arc::new(Flags::default());
        flags.stop.store(true, Ordering::SeqCst);
        // 멈춘 상태에서 첫 발화가 연결 문제로 실패 → 남은 발화는 보내지 않고 바로 실패
        let (ev, calls) = run((0..10).map(|_| net()).collect(), (0..4).map(job).collect(), Some(flags));
        assert_eq!(calls, 1);
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Failed { .. })).count(), 4);
    }

    #[test]
    fn retry_after_stop_gives_up_quickly_when_still_offline() {
        // 연달아 3번 실패 → 남은 발화는 보내지 않고 미뤄 둠 → 녹음 뒤 다시 보낸 첫 발화도 실패하면 나머지는 바로 실패
        let (ev, calls) = run((0..4).map(|_| net()).collect(), (0..6).map(job).collect(), None);
        assert_eq!(calls, 4, "실시간 3번 + 녹음 뒤 1번");
        let failed: Vec<(u64, bool)> = ev.iter().filter_map(|e| match e { Event::Failed { id, retry, .. } => Some((*id, *retry)), _ => None }).collect();
        assert_eq!(failed.iter().filter(|(_, r)| *r).count(), 6, "모두 한 번은 미뤄 둠");
        assert_eq!(failed.iter().filter(|(_, r)| !*r).count(), 6, "그리고 모두 최종 실패");
        assert!(texts(&ev).is_empty());
        let st = ev.iter().filter(|e| matches!(e, Event::Status { .. })).count();
        assert_eq!(st, 1, "다시 받아 적는다는 안내는 한 번");
    }

    #[test]
    fn file_mode_waits_instead_of_skipping() {
        let flags = Arc::new(Flags::default());
        let (ev, calls) = run(vec![net(), net(), status(503)], vec![job(0), job(1)], Some(flags.clone()));
        assert_eq!(texts(&ev), vec![0, 1], "기다렸다가 같은 발화를 다시 보내 순서대로 받아 적음");
        assert_eq!(calls, 5);
        assert!(!ev.iter().any(|e| matches!(e, Event::Failed { .. })));
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Warning { .. })).count(), 1, "기다린다는 안내는 한 번");
        assert!(!flags.stop.load(Ordering::SeqCst));
    }

    #[test]
    fn file_mode_stops_when_offline_for_too_long() {
        let flags = Arc::new(Flags::default());
        let (ev, _) = run((0..200).map(|_| net()).collect(), vec![job(0), job(1)], Some(flags.clone()));
        assert!(flags.stop.load(Ordering::SeqCst), "파일 받아쓰기를 멈춘다");
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Error { .. })).count(), 1);
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Failed { .. })).count(), 2);
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

#[cfg(test)]
mod mic_check_tests {
    use super::*;

    fn chunk(level: f32, secs: f32) -> RawChunk {
        let n = (16_000.0 * secs) as usize;
        // 간단한 잡음 흉내: 부호가 번갈아 바뀌는 일정한 크기
        RawChunk {
            source: SRC_MIC,
            rate: 16_000,
            samples: (0..n).map(|i| if i % 2 == 0 { level } else { -level }).collect(),
        }
    }

    fn judged(level: f32) -> bool {
        let mut m = MicCheck::new();
        m.feed(&chunk(level, 8.0), false);
        m.silent()
    }

    #[test]
    fn warns_when_the_mic_is_nearly_silent() {
        assert!(judged(0.0), "디지털 0 (개인 정보 차단)");
        assert!(judged(4e-5), "실사용 사례: 음소거 · 음량 0 (-88 dBFS)");
    }

    #[test]
    fn quiet_room_noise_is_not_a_problem() {
        assert!(!judged(1e-3), "조용한 방의 실제 마이크 (-60 dBFS)");
        assert!(!judged(0.05), "말소리");
    }
}
