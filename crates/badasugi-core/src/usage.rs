//! API 사용 한도 — 공급자가 매 응답 헤더로 알려 주는 "남은 요청 · 처리량"을 마지막 값으로 기억한다.
//!
//! 일반 API 키로는 잔액 · 크레딧을 조회할 수 없어서, 대신 이 값을 설정 › API 사용량에 보여 준다.
//! - Groq · OpenAI · OpenAI 호환: `x-ratelimit-{limit|remaining|reset}-{requests|tokens|…}`
//! - Anthropic: `anthropic-ratelimit-{requests|tokens|input-tokens|output-tokens}-{limit|remaining|reset}`
//! - 한도에 걸렸을 때: `retry-after`

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RateSnapshot {
    pub provider: String,
    /// 받은 시각 (유닉스 초)
    pub at: u64,
    /// (헤더 이름, 값) — 한도 관련 헤더만
    pub items: Vec<(String, String)>,
}

fn rates() -> &'static Mutex<BTreeMap<String, RateSnapshot>> {
    static RATES: OnceLock<Mutex<BTreeMap<String, RateSnapshot>>> = OnceLock::new();
    RATES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// API 주소 → 공급자 이름 ("groq" · "openai" · "anthropic" · "gemini" · "custom")
pub fn provider_of(url: &str) -> &'static str {
    let u = url.to_ascii_lowercase();
    if u.contains("api.groq.com") {
        "groq"
    } else if u.contains("api.openai.com") {
        "openai"
    } else if u.contains("api.anthropic.com") {
        "anthropic"
    } else if u.contains("generativelanguage.googleapis.com") {
        "gemini"
    } else {
        "custom"
    }
}

fn is_rate_header(name: &str) -> bool {
    name.starts_with("x-ratelimit-") || name.starts_with("anthropic-ratelimit-") || name == "retry-after"
}

/// 응답 헤더에서 한도 정보를 골라 기억한다 (없으면 그대로)
pub fn record_headers(url: &str, headers: &reqwest::header::HeaderMap) {
    let items: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| is_rate_header(k.as_str()))
        .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
        .collect();
    if items.is_empty() {
        return;
    }
    let provider = provider_of(url).to_string();
    let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Ok(mut r) = rates().lock() {
        r.insert(provider.clone(), RateSnapshot { provider, at, items });
    }
}

/// 공급자별 마지막 한도 정보
pub fn snapshot() -> Vec<RateSnapshot> {
    rates().lock().map(|r| r.values().cloned().collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn keeps_only_rate_limit_headers_per_provider() {
        assert_eq!(provider_of("https://api.groq.com/openai/v1/audio/transcriptions"), "groq");
        assert_eq!(provider_of("https://generativelanguage.googleapis.com/v1beta/openai"), "gemini");
        assert_eq!(provider_of("http://localhost:8000/v1"), "custom");

        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining-requests", HeaderValue::from_static("1999"));
        h.insert("x-ratelimit-limit-requests", HeaderValue::from_static("2000"));
        h.insert("content-type", HeaderValue::from_static("application/json"));
        record_headers("https://api.groq.com/openai/v1/audio/transcriptions", &h);
        let s = snapshot().into_iter().find(|s| s.provider == "groq").unwrap();
        assert_eq!(s.items.len(), 2);
        assert!(s.items.iter().all(|(k, _)| k.starts_with("x-ratelimit-")));
    }
}
