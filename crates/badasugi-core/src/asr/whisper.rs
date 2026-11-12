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
    // CPU 에서도 flash attention 이 20~30% 빠르다 (문제가 있으면 BADASUGI_FLASH_ATTN=0 으로 끔)
    params.flash_attn(std::env::var("BADASUGI_FLASH_ATTN").map(|v| v != "0").unwrap_or(true));
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
        let audio_ctx = (frames + 100).clamp(384, 1500);
        let reduced = audio_ctx < 1500 && std::env::var("BADASUGI_FULL_CTX").is_err();

        let mut params = params_template(self.threads, self.beam, req);
        if reduced {
            params.set_audio_ctx(audio_ctx);
        }
        self.state.full(params, samples).map_err(|e| anyhow!("whisper 추론 실패: {e}"))?;
        let mut text = self.collect();

        // 줄인 창에서 반복 루프에 빠졌으면 전체 창으로 한 번 더 (느리지만 드묾)
        if reduced && crate::text::is_degenerate(&text) {
            log::debug!("반복 감지 → 전체 창으로 재인식");
            let params = params_template(self.threads, self.beam, req);
            self.state.full(params, samples).map_err(|e| anyhow!("whisper 추론 실패: {e}"))?;
            text = self.collect();
        }
        Ok(text)
    }
}
