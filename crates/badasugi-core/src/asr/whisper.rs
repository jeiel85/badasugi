//! 로컬 whisper.cpp 엔진

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

use super::{AsrRequest, Transcriber};
use crate::SAMPLE_RATE;

static CACHE: Mutex<Option<(PathBuf, Arc<WhisperContext>)>> = Mutex::new(None);

/// 모델을 메모리에 올린다 (같은 모델이면 재사용)
pub fn load(path: &Path) -> Result<Arc<WhisperContext>> {
    let mut cache = CACHE.lock();
    if let Some((p, ctx)) = cache.as_ref() {
        if p == path {
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
    let mut params = WhisperContextParameters::default();
    // flash attention 은 끈다 (BADASUGI_FLASH_ATTN=1 로 켤 수 있음).
    // ggml 의 flash attention 은 KV 를 256 단위로 채워 계산하는데, 인코더 창을 줄여 256 의 배수가 아니게 되면
    // 채운 자리가 결과를 흐려 잡음 섞인 음성(m4a·실제 마이크)에서 엉뚱한 영어·반복이 나왔다.
    // 이 CPU 에서는 줄인 창 기준으로 끄는 쪽이 오히려 더 빨랐다 (384 프레임: 8.5초 → 5.0초).
    params.flash_attn(std::env::var("BADASUGI_FLASH_ATTN").map(|v| v == "1").unwrap_or(false));
    let ctx = WhisperContext::new_with_params(path, params).map_err(|e| anyhow!("whisper 모델 로드 실패: {e}"))?;
    let ctx = Arc::new(ctx);
    *cache = Some((path.to_path_buf(), ctx.clone()));
    Ok(ctx)
}

/// 메모리에서 내린다 (트레이 대기 중 가볍게)
pub fn unload() {
    *CACHE.lock() = None;
}

pub struct LocalWhisper {
    _ctx: Arc<WhisperContext>,
    state: WhisperState,
    threads: i32,
    beam: u32,
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
        let state = ctx.create_state().map_err(|e| anyhow!("whisper 상태 생성 실패: {e}"))?;
        let threads = if threads == 0 { default_threads() } else { threads };
        Ok(Self {
            _ctx: ctx,
            state,
            threads: threads as i32,
            beam,
        })
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

fn params_template<'a>(threads: i32, beam: u32, req: &'a AsrRequest<'a>) -> FullParams<'a, 'a> {
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
    if !req.prompt.trim().is_empty() {
        params.set_initial_prompt(req.prompt);
    }
    params
}

impl LocalWhisper {
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

        let mut params = params_template(self.threads, self.beam, req);
        if reduced {
            params.set_audio_ctx(audio_ctx);
        }
        self.state.full(params, samples).map_err(|e| anyhow!("whisper 추론 실패: {e}"))?;
        let mut text = self.collect();

        // 줄인 창에서 반복 루프에 빠졌거나, 한국어인데 엉뚱한 영어가 나왔으면 전체 창으로 한 번 더 (느리지만 드묾)
        // (AAC(m4a) 녹음에서 특히 잘 생긴다)
        if reduced && (crate::text::is_degenerate(&text) || crate::text::is_wrong_script(&text, req.language)) {
            // 바로 30초 창으로 가면 이 CPU 에서 50초가 넘게 걸려 뒤 발화가 밀리므로, 창을 두 배로 늘려 한 번 더
            let retry = (audio_ctx * 2).min(1500);
            log::debug!("반복·언어 이상 감지 → 창 {audio_ctx} → {retry} 로 재인식: {text}");
            let mut params = params_template(self.threads, self.beam, req);
            if retry < 1500 {
                params.set_audio_ctx(retry);
            }
            self.state.full(params, samples).map_err(|e| anyhow!("whisper 추론 실패: {e}"))?;
            text = self.collect();
        }
        Ok(text)
    }
}
