//! API 단가 — 설정 › 사용량에서 쓴 양을 금액(달러)으로 바꾸는 데 쓴다
//!
//! 각 공급자의 공식 단가표(2026-11-16 확인, 일반 등급 · 배치 할인 없음)를 옮겨 둔 것이라
//! 실제 청구액과 조금 다를 수 있다. 단가가 바뀌면 이 표만 고치면 되고, 지난 사용량도 새 단가로 다시 셈한다.
//! 표에 없는 모델(사용자 지정 서버 · 단가가 공개되지 않은 모델)은 금액을 셈하지 않는다.
//! 프롬프트 캐시는 Claude(묻기)만 따로 셈한다 — 쓰기는 입력 단가의 1.25배, 읽기는 [`cache_read_price`].
//! (OpenAI · Gemini 가 알아서 하는 캐시 할인은 응답에서 따로 읽지 않아 일반 입력 단가로 셈한다)
//!
//! - Claude: <https://platform.claude.com/docs/en/about-claude/pricing>
//! - Groq: <https://console.groq.com/docs/models> — 음성 인식은 요청마다 최소 10초로 매긴다
//! - OpenAI: <https://developers.openai.com/api/docs/pricing> — 음성 인식은 분당 추정 단가
//! - Gemini: <https://aistudio.google.com/docs/pricing>
//! - 리턴제로: <https://developers.rtzr.ai/docs/pricing/> (2026-11-27 확인) — 원화 · 부가세 별도, 연결마다 최소 10초
//! - CLOVA Speech: <https://www.ncloud.com/product/aiService/clovaSpeech> (2026-11-27 확인) — 원화 · 부가세 별도, 15초 단위 올림
//! - ElevenLabs: <https://elevenlabs.io/pricing/api> (2026-11-27 확인) — 초 단위

/// 모델 이름을 표와 맞춰 볼 수 있게 다듬는다 — 소문자로, "models/" 앞머리와 날짜 꼬리
/// ("-20250929" · "-2025-08-07") 를 뗀다
fn normalize(model: &str) -> String {
    let m = model.trim().to_ascii_lowercase();
    let m = m.strip_prefix("models/").unwrap_or(&m).to_string();
    let b = m.as_bytes();
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    // -YYYYMMDD
    if b.len() > 9 && b[b.len() - 9] == b'-' && digits(&b[b.len() - 8..]) {
        return m[..m.len() - 9].to_string();
    }
    // -YYYY-MM-DD
    if b.len() > 11 {
        let t = &b[b.len() - 11..];
        if t[0] == b'-' && t[5] == b'-' && t[8] == b'-' && digits(&t[1..5]) && digits(&t[6..8]) && digits(&t[9..]) {
            return m[..m.len() - 11].to_string();
        }
    }
    m
}

/// 요약(LLM) 단가 — USD / 100만 토큰 (입력, 출력). 모르는 모델이면 None
pub fn token_price(provider: &str, model: &str) -> Option<(f64, f64)> {
    let m = normalize(model);
    let p = match (provider, m.as_str()) {
        // Claude Haiku 5.5 는 입력이 10만 토큰을 넘는 요청만 5배 — 요약은 구간마다 4만 자 안쪽이라 넘지 않는다
        ("anthropic", "claude-fable-5-2" | "claude-mythos-5-2") => (8.0, 40.0),
        ("anthropic", "claude-fable-5-1" | "claude-mythos-5-1" | "claude-fable-5" | "claude-mythos-5") => (10.0, 50.0),
        ("anthropic", "claude-opus-5-5") => (4.0, 20.0),
        ("anthropic", "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" | "claude-opus-4-5") => {
            (5.0, 25.0)
        }
        ("anthropic", "claude-opus-4-1" | "claude-opus-4-0" | "claude-opus-4") => (15.0, 75.0),
        ("anthropic", "claude-sonnet-5-5" | "claude-sonnet-5") => (2.0, 10.0),
        (
            "anthropic",
            "claude-sonnet-4-6" | "claude-sonnet-4-5" | "claude-sonnet-4-0" | "claude-sonnet-4" | "claude-3-7-sonnet",
        ) => (3.0, 15.0),
        ("anthropic", "claude-haiku-5-5") => (0.10, 0.50),
        ("anthropic", "claude-haiku-4-5") => (1.0, 5.0),
        ("anthropic", "claude-3-5-haiku") => (0.80, 4.0),

        ("groq", "openai/gpt-oss-120b") => (0.15, 0.60),
        ("groq", "openai/gpt-oss-20b" | "openai/gpt-oss-safeguard-20b") => (0.075, 0.30),
        ("groq", "qwen/qwen3.8-27b") => (0.80, 4.0),

        ("openai", "gpt-5.5") => (5.0, 30.0),
        ("openai", "gpt-5.4") => (2.50, 15.0),
        ("openai", "gpt-5.4-mini") => (0.75, 4.50),
        ("openai", "gpt-5.4-nano") => (0.20, 1.25),
        ("openai", "gpt-5.2" | "gpt-5.3-codex") => (1.75, 14.0),
        ("openai", "gpt-5.1" | "gpt-5") => (1.25, 10.0),
        ("openai", "gpt-5-mini") => (0.25, 2.0),
        ("openai", "gpt-5-nano") => (0.05, 0.40),
        ("openai", "gpt-4.1") => (2.0, 8.0),
        ("openai", "gpt-4.1-mini") => (0.40, 1.60),
        ("openai", "gpt-4o") => (2.50, 10.0),
        ("openai", "gpt-4o-mini") => (0.15, 0.60),

        // gemini-flash-latest 같은 별칭은 가리키는 모델이 바뀌어 단가를 정할 수 없다
        ("gemini", "gemini-3.1-pro-preview") => (2.0, 12.0),
        ("gemini", "gemini-3-flash-preview") => (0.50, 3.0),
        ("gemini", "gemini-3.5-flash-lite" | "gemini-2.5-flash") => (0.30, 2.50),
        ("gemini", "gemini-3.1-flash-lite") => (0.25, 1.50),
        ("gemini", "gemini-2.5-flash-lite") => (0.10, 0.40),
        _ => return None,
    };
    Some(p)
}

/// 프롬프트 캐시에 쓰는 입력은 일반 입력 단가의 이 배수 (Claude, 5분 캐시)
pub const CACHE_WRITE_RATE: f64 = 1.25;

/// 프롬프트 캐시에서 읽는 입력 단가 — USD / 100만 토큰. 표에 없으면 None (그때는 입력 단가의 0.1배로 셈)
pub fn cache_read_price(provider: &str, model: &str) -> Option<f64> {
    let m = normalize(model);
    Some(match (provider, m.as_str()) {
        ("anthropic", "claude-fable-5-2" | "claude-mythos-5-2" | "claude-opus-5-5") => 0.20,
        ("anthropic", "claude-fable-5-1" | "claude-mythos-5-1") => 0.25,
        ("anthropic", "claude-sonnet-5-5") => 0.10,
        _ => return None,
    })
}

/// 음성 인식 단가 (1시간) — 달러로 매기는 곳과 원화로 매기는 곳(리턴제로 · CLOVA Speech, 부가세 별도)이 있다
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioPrice {
    Usd(f64),
    Krw(f64),
}

impl AudioPrice {
    /// 달러로 — 원화 단가는 `usd_krw`(설정 › 사용량의 환율)로 나눈다.
    /// 화면은 같은 환율로 다시 원화를 셈하므로 원화 단가의 금액은 환율과 상관없이 그대로 보인다.
    pub fn usd(self, usd_krw: f64) -> f64 {
        match self {
            AudioPrice::Usd(p) => p,
            AudioPrice::Krw(p) => p / if usd_krw > 0.0 { usd_krw } else { crate::settings::DEFAULT_USD_KRW },
        }
    }
}

/// 음성 인식 단가 (1시간). 모르는 모델이면 None
pub fn audio_price(provider: &str, model: &str) -> Option<AudioPrice> {
    use AudioPrice::{Krw, Usd};
    let m = normalize(model);
    Some(match (provider, m.as_str()) {
        ("groq", "whisper-large-v3-turbo") => Usd(0.04),
        ("groq", "whisper-large-v3") => Usd(0.111),
        // 분당 $0.0045 · $0.006 · $0.003
        ("openai", "gpt-transcribe") => Usd(0.27),
        ("openai", "gpt-4o-transcribe" | "whisper-1") => Usd(0.36),
        ("openai", "gpt-4o-mini-transcribe") => Usd(0.18),
        // 리턴제로: 모델 · 파일/스트리밍 상관없이 시간당 1,000원 (한 달 1,000시간까지 — 넘으면 500원)
        ("rtzr", _) => Krw(1000.0),
        // CLOVA Speech 장문 인식: 15초 5원 = 시간당 1,200원
        ("clova", "장문 인식") => Krw(1200.0),
        // ElevenLabs Scribe v3 · v2: 시간당 $0.20, 용어(keyterms)를 넣으면 $0.05 더
        ("elevenlabs", "scribe_v3" | "scribe_v2") => Usd(0.20),
        ("elevenlabs", "scribe_v3+keyterms" | "scribe_v2+keyterms") => Usd(0.25),
        _ => return None,
    })
}

/// 요청 하나에 요금이 매겨지는 녹음 길이(초) — Groq · 리턴제로는 10초보다 짧아도 10초로,
/// CLOVA Speech 는 15초 단위로 올려 매긴다
pub fn billed_secs(provider: &str, secs: f64) -> f64 {
    match provider {
        "groq" | "rtzr" => secs.max(10.0),
        "clova" => (secs / 15.0).ceil().max(1.0) * 15.0,
        _ => secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_models_with_date_suffix_and_case() {
        assert_eq!(token_price("anthropic", "claude-opus-5-5"), Some((4.0, 20.0)));
        assert_eq!(token_price("anthropic", "claude-sonnet-4-5-20250929"), Some((3.0, 15.0)));
        assert_eq!(token_price("openai", "GPT-5-mini-2025-08-07"), Some((0.25, 2.0)));
        assert_eq!(token_price("gemini", "models/gemini-2.5-flash-lite"), Some((0.10, 0.40)));
        // "claude-opus-5" 가 "claude-opus-5-5" 를 삼키지 않는다
        assert_eq!(token_price("anthropic", "claude-opus-5"), Some((5.0, 25.0)));
    }

    #[test]
    fn unknown_or_custom_models_have_no_price() {
        assert_eq!(token_price("custom", "openai/gpt-oss-120b"), None);
        assert_eq!(token_price("gemini", "gemini-flash-latest"), None);
        assert_eq!(audio_price("custom", "whisper-1"), None);
        assert_eq!(audio_price("groq", "whisper-large-v3-turbo"), Some(AudioPrice::Usd(0.04)));
        assert_eq!(audio_price("clova", "모르는 모델"), None);
    }

    #[test]
    fn korean_services_are_priced_in_won() {
        assert_eq!(audio_price("rtzr", "sommers"), Some(AudioPrice::Krw(1000.0)));
        assert_eq!(audio_price("clova", "장문 인식"), Some(AudioPrice::Krw(1200.0)));
        assert_eq!(audio_price("elevenlabs", "scribe_v3"), Some(AudioPrice::Usd(0.20)));
        assert_eq!(audio_price("elevenlabs", "scribe_v3+keyterms"), Some(AudioPrice::Usd(0.25)));
        // 원화 단가는 환율로 나눠 달러로 (화면이 같은 환율로 원화를 셈한다)
        assert!((AudioPrice::Krw(1000.0).usd(1250.0) - 0.8).abs() < 1e-12);
        assert_eq!(AudioPrice::Usd(0.2).usd(1250.0), 0.2);
        assert!((AudioPrice::Krw(1360.0).usd(0.0) - 1.0).abs() < 1e-12, "환율이 비면 기본 환율");
    }

    #[test]
    fn groq_bills_at_least_ten_seconds_per_request() {
        assert_eq!(billed_secs("groq", 3.2), 10.0);
        assert_eq!(billed_secs("groq", 12.5), 12.5);
        assert_eq!(billed_secs("openai", 3.2), 3.2);
        assert_eq!(billed_secs("rtzr", 4.0), 10.0);
        assert_eq!(billed_secs("elevenlabs", 4.0), 4.0);
        // CLOVA Speech 는 15초 단위 올림
        assert_eq!(billed_secs("clova", 0.4), 15.0);
        assert_eq!(billed_secs("clova", 15.0), 15.0);
        assert_eq!(billed_secs("clova", 15.2), 30.0);
    }
}
