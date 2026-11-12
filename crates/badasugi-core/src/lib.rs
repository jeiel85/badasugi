//! 받아쓰기(badasugi) 코어.
//!
//! 오디오 캡처 → 16 kHz 리샘플 → Silero VAD 발화 분할 → 화자 임베딩/클러스터링 → 음성 인식(로컬 whisper.cpp
//! 또는 OpenAI 호환 API) 으로 이어지는 실시간 파이프라인을 제공한다. UI 와는 [`pipeline::Event`] 로만 통신한다.

pub mod asr;
pub mod audio;
pub mod decode;
pub mod fbank;
pub mod models;
pub mod onnx;
pub mod perf;
pub mod pipeline;
pub mod resample;
pub mod speaker;
pub mod text;
pub mod vad;
pub mod wav;

/// 파이프라인 내부 표준 샘플레이트 (whisper · Silero · 화자 모델 공통)
pub const SAMPLE_RATE: u32 = 16_000;

/// 샘플 수 → 밀리초
#[inline]
pub fn samples_to_ms(n: u64) -> u64 {
    n * 1000 / SAMPLE_RATE as u64
}
