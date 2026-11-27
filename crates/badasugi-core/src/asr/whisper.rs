//! 로컬 whisper.cpp 엔진

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

use super::{AbortCheck, AsrRequest, Transcriber};
use crate::SAMPLE_RATE;

/// 불러온 모델 (경로, GPU 로 열었는지, 모델)
static CACHE: Mutex<Option<(PathBuf, bool, Arc<WhisperContext>)>> = Mutex::new(None);
/// 지금 불러온 모델이 도는 장치 — "GPU · Intel(R) Graphics (Vulkan0)" / "CPU"
static DEVICE: Mutex<Option<String>> = Mutex::new(None);

/// 마지막으로 불러온 모델이 도는 장치 (진단 · 화면 표시용)
pub fn device() -> Option<String> {
    DEVICE.lock().clone()
}

fn context_params(gpu: bool) -> WhisperContextParameters<'static> {
    let mut params = WhisperContextParameters::default();
    // flash attention 은 끈다 (BADASUGI_FLASH_ATTN=1 로 켤 수 있음).
    // ggml 의 flash attention 은 KV 를 256 단위로 채워 계산하는데, 인코더 창을 줄여 256 의 배수가 아니게 되면
    // 채운 자리가 결과를 흐려 잡음 섞인 음성(m4a·실제 마이크)에서 엉뚱한 영어·반복이 나왔다.
    // 이 CPU 에서는 줄인 창 기준으로 끄는 쪽이 오히려 더 빨랐다 (384 프레임: 8.5초 → 5.0초).
    params.flash_attn(std::env::var("BADASUGI_FLASH_ATTN").map(|v| v == "1").unwrap_or(false));
    params.use_gpu(gpu);
    params.gpu_device(0);
    params
}

/// 모델을 메모리에 올린다 (같은 모델이면 재사용). GPU 빌드면 시작할 때 장치를 확인해 GPU 로 열고,
/// GPU 가 없거나 꺼 두었거나 열지 못하면 CPU 로 연다.
pub fn load(path: &Path) -> Result<Arc<WhisperContext>> {
    let gpu = super::gpu::should_try();
    let mut cache = CACHE.lock();
    if let Some((p, g, ctx)) = cache.as_ref() {
        if p == path && *g == gpu {
            return Ok(ctx.clone());
        }
    }
    *cache = None;
    if !path.exists() {
        return Err(anyhow!("모델 파일이 없습니다: {}", path.display()));
    }
    if std::env::var("BADASUGI_WHISPER_DEBUG").is_err() {
        whisper_rs::install_logging_hooks();
    }
    if gpu {
        match super::gpu::find() {
            Some(dev) => match WhisperContext::new_with_params(path, context_params(true)) {
                Ok(ctx) => {
                    log::info!("음성 인식 장치: GPU · {dev}");
                    *DEVICE.lock() = Some(format!("GPU · {dev}"));
                    let ctx = Arc::new(ctx);
                    *cache = Some((path.to_path_buf(), true, ctx.clone()));
                    return Ok(ctx);
                }
                Err(e) => {
                    log::warn!("GPU({dev})로 모델을 열지 못해 CPU 로 다시 엽니다: {e}");
                    super::gpu::mark_broken();
                }
            },
            None => {
                // 이번 실행 동안 다시 찾지 않는다 — 다음에 열 때도 CPU 모델을 그대로 다시 쓴다
                super::gpu::mark_no_device();
                if super::gpu::loader_missing() {
                    log::info!("Vulkan 이 없는 PC — CPU 로 받아 적습니다");
                } else {
                    log::info!("GPU 를 찾지 못해 CPU 로 받아 적습니다");
                }
            }
        }
    }
    let ctx = WhisperContext::new_with_params(path, context_params(false)).map_err(|e| anyhow!("whisper 모델 로드 실패: {e}"))?;
    if super::gpu::BUILT_WITH_GPU {
        log::info!("음성 인식 장치: CPU");
    }
    *DEVICE.lock() = Some("CPU".into());
    let ctx = Arc::new(ctx);
    *cache = Some((path.to_path_buf(), false, ctx.clone()));
    Ok(ctx)
}

/// 메모리에서 내린다 (트레이 대기 중 가볍게)
pub fn unload() {
    *CACHE.lock() = None;
}

/// 이 모델을 들고 있으면 내린다 (녹음 뒤 다시 받아 적기가 끝난 뒤 — 그 사이 다른 모델을 열었으면 그대로 둔다)
pub fn unload_if(path: &Path) {
    let mut cache = CACHE.lock();
    if cache.as_ref().is_some_and(|(p, _, _)| p == path) {
        *cache = None;
    }
}

pub struct LocalWhisper {
    ctx: Arc<WhisperContext>,
    state: WhisperState,
    threads: i32,
    beam: u32,
    path: PathBuf,
    /// 이 상태가 GPU 위에서 도는지 — 인식이 GPU 에서 실패하면 CPU 로 다시 열어 한 번 더
    on_gpu: bool,
    /// 용어집 낱말로 만든 프롬프트 (낱말, 프롬프트) — 녹음 동안 같은 용어집이라 한 번만 센다
    glossary: Option<(Vec<String>, String)>,
}

/// 로컬 엔진 프롬프트의 최대 토큰 수 = whisper.cpp 가 쓰는 한도 (`n_text_ctx / 2 - 1`, 넘치면 앞쪽을 버린다).
/// 용어집은 앞에서부터 쉼표로 늘어놓고("A, B, C.") 넘치는 뒤쪽 용어를 뺀다 — 앞에 둔 용어가 중요한 용어.
/// 실제 회의 2건(2026-11-23 · 11-25, gpt-transcribe 기록과 견줌)에서 고른 형식이다:
/// - 설명 문구("회의 녹취록입니다. 주요 용어: …")를 붙이면, 용어집이 짧을 때 소리가 불분명한 곳에 그 문구를
///   받아 적었다 (용어 20개: turbo 310문장 중 134문장, large-v3 26문장). 용어집이 길면 whisper.cpp 가 넘치는
///   앞쪽(설명 문구 · 앞쪽 용어)을 버려서 드러나지 않았을 뿐이다. 빼도 정확도는 그대로였다.
/// - 띄어쓰기로 늘어놓으면("A B C.") 결과의 쉼표 · 마침표가 줄어 large-v3 가 나빠졌다
///   (글자 오류율 +4.1점, 문장부호로 끝나는 문장 73% → 39%).
/// - 용어를 20개(91토큰)로 줄이면 turbo 는 조금 나아졌지만(11-23 −1.6점) large-v3 가 나빠져(+2.7점) 한도까지 넣는다.
pub const LOCAL_PROMPT_TOKENS: usize = 223;

/// 이 모델의 토크나이저로 센 토큰 수 (whisper.cpp 가 `initial_prompt` 를 자르는 기준과 같다)
fn token_count(ctx: &WhisperContext, text: &str) -> usize {
    // whisper-rs 는 토큰이 `max_tokens` 를 넘으면(-n) 길이를 잘못 잡으므로, 넘칠 수 없게 바이트 수만큼 잡는다
    // (토큰 하나는 1바이트 이상)
    ctx.tokenize(text, text.len() + 1).map(|t| t.len()).unwrap_or(usize::MAX)
}

/// 용어를 앞에서부터 쉼표로 이어 `budget` 토큰 안쪽까지 ("A, B, C.") — (프롬프트, 넣은 용어 수).
/// 넘치는 용어가 나오면 거기서 멈춘다 (앞에 둔 용어가 중요한 용어).
fn fit_terms(terms: &[String], budget: usize, count: impl Fn(&str) -> usize) -> (String, usize) {
    let joined = |kept: &[&str]| format!("{}.", kept.join(", "));
    let mut kept: Vec<&str> = Vec::new();
    for t in terms.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
        kept.push(t);
        if count(&joined(&kept)) > budget {
            kept.pop();
            break;
        }
    }
    if kept.is_empty() {
        (String::new(), 0)
    } else {
        (joined(&kept), kept.len())
    }
}

/// 캐시에 있는 이 모델이 GPU 로 열렸는지
fn is_gpu_ctx(path: &Path) -> bool {
    CACHE.lock().as_ref().is_some_and(|(p, g, _)| p == path && *g)
}

pub fn default_threads() -> usize {
    if let Some(n) = std::env::var("BADASUGI_THREADS").ok().and_then(|v| v.parse().ok()) {
        return n;
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8)
}

impl LocalWhisper {
    pub fn new(path: &Path, threads: usize, beam: u32) -> Result<Self> {
        let ctx = load(path)?;
        let state = match ctx.create_state() {
            Ok(s) => s,
            // GPU 에 상태(작업 메모리)를 만들지 못했으면 CPU 로
            Err(e) if is_gpu_ctx(path) => {
                log::warn!("GPU 에 whisper 상태를 만들지 못해 CPU 로 다시 엽니다: {e}");
                super::gpu::mark_broken();
                return Self::new(path, threads, beam);
            }
            Err(e) => return Err(anyhow!("whisper 상태 생성 실패: {e}")),
        };
        let threads = if threads == 0 { default_threads() } else { threads };
        let mut me = Self {
            on_gpu: is_gpu_ctx(path),
            ctx,
            state,
            threads: threads as i32,
            beam,
            path: path.to_path_buf(),
            glossary: None,
        };
        if me.on_gpu {
            me.warm_up();
        }
        Ok(me)
    }

    /// GPU(Vulkan)는 처음 계산할 때 연산 파이프라인을 만드느라 몇 초 걸린다 (이 PC 내장 그래픽: 첫 발화 6초 · 그 뒤 1.2초).
    /// 모델을 열 때 1초 무음으로 한 번 돌려 두어 회의 첫마디가 늦지 않게 한다. 여기서 실패하면 CPU 로.
    fn warm_up(&mut self) {
        let t = std::time::Instant::now();
        let silence = vec![0.0f32; SAMPLE_RATE as usize];
        let req = AsrRequest {
            samples: &silence,
            language: "ko",
            prompt: "",
            keywords: &[],
            context: "",
            preview: None,
            abort: None,
        };
        let threads = self.threads;
        let make = || {
            let mut p = params_template(threads, 1, &req, "");
            p.set_audio_ctx(ctx_min());
            p
        };
        match self.full(make, &silence, &|| false) {
            Ok(()) => log::info!("GPU 준비 ({}ms)", t.elapsed().as_millis()),
            Err(e) => log::warn!("GPU 준비 실패: {e}"),
        }
    }

    /// GPU 에서 인식이 실패했다 → GPU 를 접고 CPU 로 다시 연다 (이번 실행 동안)
    fn fall_back_to_cpu(&mut self, e: &dyn std::fmt::Display) -> Result<()> {
        log::warn!("GPU 에서 인식이 실패해 CPU 로 다시 엽니다: {e}");
        super::gpu::mark_broken();
        unload();
        let ctx = load(&self.path)?;
        self.state = ctx.create_state().map_err(|e| anyhow!("whisper 상태 생성 실패: {e}"))?;
        self.ctx = ctx;
        self.on_gpu = false;
        Ok(())
    }

    /// 한 번 푼다. GPU 에서 실패하면 CPU 로 다시 열어 한 번 더 (`make` 는 매번 새 매개변수).
    /// `aborted`: 일부러 그만둔 것인지 (말하는 동안 미리 보기 — whisper.cpp 는 중단도 실패(-6)로 돌려준다).
    /// 그만둔 것이면 GPU 탓이 아니므로 CPU 로 바꾸지 않는다.
    fn full<'p>(&mut self, make: impl Fn() -> FullParams<'p, 'p>, samples: &[f32], aborted: &dyn Fn() -> bool) -> Result<()> {
        match self.state.full(make(), samples) {
            Ok(_) => Ok(()),
            Err(e) if aborted() => Err(anyhow!("그만둠: {e}")),
            Err(e) if self.on_gpu => {
                self.fall_back_to_cpu(&e)?;
                self.state.full(make(), samples).map(|_| ()).map_err(|e| anyhow!("whisper 추론 실패: {e}"))
            }
            Err(e) => Err(anyhow!("whisper 추론 실패: {e}")),
        }
    }
}

fn env_i32(name: &str, default: i32) -> i32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// 줄인 인코더 창의 최소 크기 (프레임, 50 = 1초)
fn ctx_min() -> i32 {
    env_i32("BADASUGI_CTX_MIN", 384).clamp(128, 1500)
}

/// 발화 길이에 더하는 여유 (프레임)
fn ctx_margin() -> i32 {
    env_i32("BADASUGI_CTX_MARGIN", 100).clamp(0, 1500)
}

fn params_template<'a>(threads: i32, beam: u32, req: &'a AsrRequest<'a>, prompt: &'a str) -> FullParams<'a, 'a> {
    let strategy = if beam >= 2 {
        SamplingStrategy::BeamSearch {
            beam_size: beam as i32,
            patience: -1.0,
        }
    } else {
        SamplingStrategy::Greedy { best_of: 1 }
    };
    let mut params = FullParams::new(strategy);
    let lang = if req.language == "auto" { None } else { Some(req.language) };
    params.set_language(lang);
    params.set_n_threads(threads);
    params.set_translate(false);
    params.set_no_context(true);
    params.set_no_timestamps(true);
    params.set_single_segment(false);
    params.set_print_special(false);
    let dbg = std::env::var("BADASUGI_WHISPER_DEBUG").is_ok();
    params.set_print_progress(dbg);
    params.set_print_realtime(dbg);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_temperature(0.0);
    params.set_temperature_inc(0.2);
    params.set_no_speech_thold(0.6);
    if !prompt.is_empty() {
        params.set_initial_prompt(prompt);
    }
    params
}

/// whisper.cpp 가 계산 중에 부르는 중단 확인 — `user_data` 는 `&&AbortCheck` (부르는 쪽이 `full` 이 끝날 때까지 잡고 있다)
unsafe extern "C" fn abort_trampoline(user_data: *mut std::ffi::c_void) -> bool {
    if user_data.is_null() {
        return false;
    }
    let check = unsafe { &*(user_data as *const &AbortCheck<'_>) };
    check()
}

impl LocalWhisper {
    /// 이 엔진에 보낼 프롬프트. 용어집 낱말이 있으면 앞에서부터 [`LOCAL_PROMPT_TOKENS`] 안쪽으로 쉼표 나열,
    /// 없으면 받은 프롬프트 그대로 ([`AsrRequest::full_prompt`]). 앞 문맥이 있으면 끝에 붙이되,
    /// 합쳐서 whisper.cpp 한도를 넘으면(앞쪽 용어가 잘리므로) 앞 문맥을 뺀다.
    fn prompt_for(&mut self, req: &AsrRequest) -> String {
        if req.keywords.is_empty() {
            return req.full_prompt();
        }
        let terms = match &self.glossary {
            Some((k, p)) if k.as_slice() == req.keywords => p.clone(),
            _ => {
                let ctx = &self.ctx;
                let (p, n) = fit_terms(req.keywords, LOCAL_PROMPT_TOKENS, |s| token_count(ctx, s));
                log::info!(
                    "로컬 용어집 프롬프트: 용어 {}개 중 앞 {n}개 ({}토큰)",
                    req.keywords.len(),
                    token_count(ctx, &p)
                );
                self.glossary = Some((req.keywords.to_vec(), p.clone()));
                p
            }
        };
        let context = req.context.trim();
        if context.is_empty() {
            return terms;
        }
        if terms.is_empty() {
            // whisper.cpp 가 넘치는 앞쪽을 버린다 — 앞 문맥은 뒤(바로 앞 말)가 남는다
            return context.to_string();
        }
        let full = format!("{terms} {context}");
        if token_count(&self.ctx, &full) <= LOCAL_PROMPT_TOKENS {
            full
        } else {
            terms
        }
    }

    /// 말하는 동안 미리 보기: 빠르게 한 번만 (greedy · 다시 풀기 없음 · 반복 정리만). 확정 인식할 발화가 기다리면 그만둔다.
    /// (인코더 계산 중에는 멈추지 않고, 인코더가 끝난 뒤 · 낱말을 하나씩 풀 때마다 확인한다)
    fn preview(&mut self, req: &AsrRequest, samples: &[f32], audio_ctx: i32, prompt: &str, abort: &AbortCheck<'_>) -> Result<String> {
        let holder: &AbortCheck<'_> = abort;
        let threads = self.threads;
        let make = || {
            let mut params = params_template(threads, 1, req, prompt);
            if audio_ctx < 1500 {
                params.set_audio_ctx(audio_ctx);
            }
            params.set_temperature_inc(0.0);
            let secs = req.samples.len() as f32 / SAMPLE_RATE as f32;
            params.set_max_tokens((secs * 30.0) as i32 + 20);
            // SAFETY: `holder` 는 아래 `full` 이 돌아올 때까지 살아 있고, whisper.cpp 는 `full` 안에서만 콜백을 부른다
            unsafe {
                params.set_abort_callback(Some(abort_trampoline));
                params.set_abort_callback_user_data(&holder as *const &AbortCheck<'_> as *mut std::ffi::c_void);
            }
            params
        };
        self.full(make, samples, abort).map_err(|e| anyhow!("미리 보기 그만둠: {e}"))?;
        if abort() {
            anyhow::bail!("미리 보기 그만둠");
        }
        Ok(crate::text::clean(&self.collect()))
    }

    fn collect(&self) -> String {
        let mut text = String::new();
        for seg in self.state.as_iter() {
            if seg.no_speech_probability() > 0.8 {
                continue;
            }
            if let Ok(t) = seg.to_str_lossy() {
                text.push_str(&t);
            }
        }
        text
    }
}

impl Transcriber for LocalWhisper {
    fn transcribe(&mut self, req: &AsrRequest) -> Result<String> {
        // whisper.cpp 는 1초 미만 입력을 거부하므로 무음으로 채운다
        let min = SAMPLE_RATE as usize + SAMPLE_RATE as usize / 10;
        let padded;
        let samples = if req.samples.len() < min {
            let mut v = req.samples.to_vec();
            v.resize(min, 0.0);
            padded = v;
            &padded[..]
        } else {
            req.samples
        };

        // 인코더는 원래 항상 30초 창(1500 프레임)을 계산한다. 실제 길이 + 무음 여유분으로 줄이면
        // 짧은 발화에서 3~5배 빨라진다. 여유분(2초)과 최소 창(약 7.7초)이 모자라면
        // 모델이 문장 끝을 못 보고 같은 말을 반복하므로 실험으로 정한 값을 지킨다.
        let frames = (samples.len() as f64 / SAMPLE_RATE as f64 * 50.0).ceil() as i32;
        let audio_ctx = (frames + ctx_margin()).clamp(ctx_min(), 1500);
        let reduced = audio_ctx < 1500 && std::env::var("BADASUGI_FULL_CTX").is_err();

        let prompt = self.prompt_for(req);
        if let Some(abort) = req.preview {
            return self.preview(req, samples, audio_ctx, &prompt, abort);
        }
        let (threads, beam) = (self.threads, self.beam);
        // 그만두기 (녹음 뒤 다시 받아 적기): 계산 중에 확인해 바로 CPU · GPU 를 내준다
        let stop: Option<&AbortCheck<'_>> = req.abort;
        let aborted = || stop.is_some_and(|f| f());
        let make = || {
            let mut params = params_template(threads, beam, req, &prompt);
            if reduced {
                params.set_audio_ctx(audio_ctx);
                // whisper 자체의 온도 올려 다시 풀기(최대 5번)는 끈다: 짧은 발화가 같은 말을 되풀이하면 이게 돌아
                // 1초짜리 "안녕하세요"에 10초 넘게 걸리고 엉뚱한 말이 섞였다. 이상하면 아래에서 직접 정리·재인식한다.
                params.set_temperature_inc(0.0);
                // 말 길이에 비해 터무니없이 긴 출력(반복 루프)은 일찍 끊는다 (빠른 말 · 바이트 토큰까지 넉넉히)
                let secs = req.samples.len() as f32 / SAMPLE_RATE as f32;
                params.set_max_tokens((secs * 30.0) as i32 + 20);
            }
            if let Some(holder) = &stop {
                set_abort(&mut params, holder);
            }
            params
        };
        self.full(make, samples, &aborted)?;
        if aborted() {
            anyhow::bail!("그만둠");
        }
        let mut text = self.collect();

        // 줄인 창에서 반복 루프에 빠졌거나, 한국어인데 엉뚱한 영어가 나왔으면 전체 창으로 한 번 더 (느리지만 드묾)
        // (AAC(m4a) 녹음에서 특히 잘 생긴다)
        let bad = |t: &str| crate::text::is_degenerate(t) || crate::text::is_wrong_script(t, req.language);
        if reduced && bad(&text) {
            // 짧은 발화("안녕하세요")는 줄인 창에서 같은 문장을 두 번 내놓는 일이 잦다. 겹친 문장만 빼면 맞는 결과라,
            // 다시 인식(이 CPU 에서 15초 이상)하지 않는다 → 회의 첫마디가 바로 뜬다.
            // 긴 발화의 반복은 뒷말을 빠뜨렸을 수 있으므로 그대로 다시 인식한다.
            if req.samples.len() <= SAMPLE_RATE as usize * 4 {
                let tidy = crate::text::clean(&text);
                if !tidy.is_empty() && !bad(&tidy) {
                    log::debug!("짧은 발화 반복 → 겹친 문장만 정리: {text} → {tidy}");
                    return Ok(tidy);
                }
            }
        }
        if reduced && bad(&text) {
            // 바로 30초 창으로 가면 이 CPU 에서 50초가 넘게 걸려 뒤 발화가 밀리므로, 창을 두 배로 늘려 한 번 더
            let retry = (audio_ctx * 2).min(1500);
            log::debug!("반복·언어 이상 감지 → 창 {audio_ctx} → {retry} 로 재인식: {text}");
            let make = || {
                let mut params = params_template(threads, beam, req, &prompt);
                if retry < 1500 {
                    params.set_audio_ctx(retry);
                }
                if let Some(holder) = &stop {
                    set_abort(&mut params, holder);
                }
                params
            };
            self.full(make, samples, &aborted)?;
            if aborted() {
                anyhow::bail!("그만둠");
            }
            text = self.collect();
        }
        Ok(text)
    }
}

/// 계산 중에 그만둘지 묻는 콜백을 단다 (인코더가 끝난 뒤 · 낱말을 하나씩 풀 때마다 확인한다)
fn set_abort(params: &mut FullParams<'_, '_>, holder: &&AbortCheck<'_>) {
    // SAFETY: `holder` 는 부르는 쪽 지역 변수를 가리키고 `full` 이 돌아올 때까지 살아 있다.
    // whisper.cpp 는 `full` 안에서만 콜백을 부른다.
    unsafe {
        params.set_abort_callback(Some(abort_trampoline));
        params.set_abort_callback_user_data(holder as *const &AbortCheck<'_> as *mut std::ffi::c_void);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(s: &str) -> Vec<String> {
        s.split(',').map(|t| t.trim().to_string()).collect()
    }

    #[test]
    fn fit_terms_keeps_front_terms_within_budget() {
        let t = terms("OKPOS, 웨이브포스, OKPOS Helper, PM2.0");
        // 글자 수로 센다고 치면: "OKPOS, 웨이브포스." = 13
        let count = |s: &str| s.chars().count();
        assert_eq!(fit_terms(&t, 13, count), ("OKPOS, 웨이브포스.".to_string(), 2));
        assert_eq!(fit_terms(&t, 100, count), ("OKPOS, 웨이브포스, OKPOS Helper, PM2.0.".to_string(), 4));
        // 넘치는 용어가 나오면 뒤의 짧은 용어도 넣지 않는다 (앞에 둔 순서를 지킨다)
        let t = terms("OKPOS, 아주 긴 용어 이름입니다, ASP");
        assert_eq!(fit_terms(&t, 13, count), ("OKPOS.".to_string(), 1));
        // 빈 낱말은 건너뛰고, 첫 용어부터 넘치면 비운다
        assert_eq!(fit_terms(&terms(" , OKPOS"), 100, count), ("OKPOS.".to_string(), 1));
        assert_eq!(fit_terms(&terms("OKPOS"), 3, count), (String::new(), 0));
        assert_eq!(fit_terms(&[], 100, count), (String::new(), 0));
    }

    /// 실제 모델 토크나이저로 센 토큰 수 — `BADASUGI_TEST_MODEL=<ggml 모델 경로>` 일 때만
    #[test]
    fn token_count_matches_whisper_tokenizer() {
        let Ok(path) = std::env::var("BADASUGI_TEST_MODEL") else {
            return;
        };
        let ctx = WhisperContext::new_with_params(&path, context_params(false)).expect("모델");
        assert_eq!(token_count(&ctx, ""), 0);
        assert_eq!(token_count(&ctx, "OKPOS"), 2);
        // 한도를 넘는 용어집 — 앞에서부터 한도 안쪽까지 넣고, 하나 더 넣으면 넘친다
        let t: Vec<String> = (1..=100).map(|i| format!("웨이브포스{i}")).collect();
        let (p, n) = fit_terms(&t, LOCAL_PROMPT_TOKENS, |s| token_count(&ctx, s));
        assert!(n > 0 && n < t.len(), "{n}");
        assert!(p.starts_with("웨이브포스1, 웨이브포스2, "), "{p}");
        assert!(token_count(&ctx, &p) <= LOCAL_PROMPT_TOKENS);
        let more = format!("{}, {}.", p.trim_end_matches('.'), t[n]);
        assert!(token_count(&ctx, &more) > LOCAL_PROMPT_TOKENS);
    }
}
