//! OpenAI 호환 음성 인식 API (`POST {base}/audio/transcriptions`)
//!
//! OpenAI(gpt-4o-transcribe, whisper-1), Groq(whisper-large-v3-turbo), 자체 호스팅 서버
//! (faster-whisper-server/speaches, LocalAI 등) 를 모두 같은 코드로 호출한다.

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::blocking::{multipart, Client};

use super::{AsrRequest, Transcriber};

pub struct ApiTranscriber {
    client: Client,
    url: String,
    api_key: String,
    model: String,
}

pub fn http_client(timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("badasugi/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

impl ApiTranscriber {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Result<Self> {
        if base_url.trim().is_empty() {
            return Err(anyhow!("API 주소가 비어 있습니다"));
        }
        Ok(Self {
            client: http_client(Duration::from_secs(90))?,
            url: format!("{}/audio/transcriptions", base_url.trim().trim_end_matches('/')),
            api_key: api_key.trim().to_string(),
            model: model.trim().to_string(),
        })
    }

    fn call(&self, wav: &[u8], req: &AsrRequest) -> Result<String> {
        let file = multipart::Part::bytes(wav.to_vec())
            .file_name("speech.wav")
            .mime_str("audio/wav")?;
        let mut form = multipart::Form::new()
            .part("file", file)
            .text("model", self.model.clone())
            .text("response_format", "json")
            .text("temperature", "0");
        if req.language != "auto" && !req.language.is_empty() {
            form = form.text("language", req.language.to_string());
        }
        if !req.prompt.trim().is_empty() {
            form = form.text("prompt", req.prompt.to_string());
        }
        let mut rb = self.client.post(&self.url).multipart(form);
        if !self.api_key.is_empty() {
            rb = rb.bearer_auth(&self.api_key);
        }
        let resp = rb.send().map_err(|e| ApiError::Network(describe_http_error(&e)))?;
        // 공급자가 알려 주는 남은 한도 (설정 › API 사용량)
        crate::usage::record_headers(&self.url, resp.headers());
        let status = resp.status();
        let retry_after = retry_after(resp.headers());
        let body = resp.text().map_err(|e| ApiError::Network(describe_http_error(&e)))?;
        if !status.is_success() {
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| body.chars().take(300).collect());
            return Err(ApiError::Status {
                status: status.as_u16(),
                retry_after,
                message,
            }
            .into());
        }
        let v: serde_json::Value =
            serde_json::from_str(&body).map_err(|_| anyhow!("API 응답 해석 실패: {}", body.chars().take(200).collect::<String>()))?;
        Ok(v["text"].as_str().unwrap_or_default().to_string())
    }
}

impl Transcriber for ApiTranscriber {
    fn transcribe(&mut self, req: &AsrRequest) -> Result<String> {
        let wav = crate::wav::encode_pcm16(req.samples)?;
        let mut last_err = None;
        for attempt in 0..3u32 {
            match self.call(&wav, req) {
                Ok(t) => return Ok(t),
                Err(e) => {
                    // 인증·요청 오류는 다시 보내도 소용없다
                    if !super::retryable(&e) {
                        return Err(e);
                    }
                    // 요청 한도(429)면 공급자가 알려 준 만큼 기다린다 (너무 길면 20초까지만 — 나머지는 파이프라인이 맡는다)
                    let wait = e
                        .downcast_ref::<ApiError>()
                        .and_then(ApiError::retry_after)
                        .map(|d| d.min(MAX_RETRY_WAIT))
                        .unwrap_or(Duration::from_millis(600 * u64::from(attempt + 1)));
                    last_err = Some(e);
                    if attempt < 2 {
                        std::thread::sleep(wait);
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("알 수 없는 API 오류")))
    }
}

/// 요청 한도에 걸렸을 때 한 번에 기다리는 최대 시간
const MAX_RETRY_WAIT: Duration = Duration::from_secs(20);

/// 음성 인식 API 오류 — 다시 보내 볼 만한지 가리고, 사용자에게 원인을 우리말로 보여 준다
#[derive(Debug)]
pub enum ApiError {
    /// 서버에 닿지 못했거나 응답이 늦음 (인터넷 · 주소 · 방화벽)
    Network(String),
    /// 서버가 오류 상태로 답함
    Status {
        status: u16,
        /// `retry-after` (요청 한도에 걸렸을 때)
        retry_after: Option<Duration>,
        message: String,
    },
}

impl ApiError {
    /// 같은 요청을 다시 보내도 결과가 같을 오류 (키 · 모델 이름 · 요청 형식)
    pub fn is_fatal(&self) -> bool {
        matches!(self, ApiError::Status { status, .. } if matches!(status, 400 | 401 | 403 | 404 | 413 | 415 | 422))
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ApiError::Status { retry_after, .. } => *retry_after,
            ApiError::Network(_) => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Network(m) => f.write_str(m),
            ApiError::Status { status, message, .. } => {
                let hint = match status {
                    401 | 403 => " — API 키를 확인해 주세요",
                    404 => " — 주소나 모델 이름을 확인해 주세요",
                    429 => " — 요청 한도에 걸렸어요",
                    500..=599 => " — 서비스 쪽 문제예요",
                    _ => "",
                };
                write!(f, "API 오류 {status}{hint}: {message}")
            }
        }
    }
}

impl std::error::Error for ApiError {}

/// `retry-after: 7` (초) — HTTP 날짜 형식은 드물어 무시한다
fn retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs = h.get("retry-after")?.to_str().ok()?.trim().parse::<f64>().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// 네트워크 오류를 사용자가 알아볼 수 있게: 무슨 일인지 + 가장 안쪽 원인
/// (reqwest 는 "error sending request for url (…)" 만 보여 줘서 끊김인지 시간 초과인지 알 수 없다)
pub fn describe_http_error(e: &reqwest::Error) -> String {
    let what = if e.is_timeout() {
        "서버 응답이 너무 늦어요"
    } else if e.is_connect() {
        "서버에 연결할 수 없어요 — 인터넷 연결이나 주소를 확인해 주세요"
    } else if e.is_decode() || e.is_body() {
        "서버 응답을 끝까지 받지 못했어요"
    } else {
        "서버와 주고받지 못했어요"
    };
    let mut inner = None;
    let mut cur = std::error::Error::source(e);
    while let Some(s) = cur {
        inner = Some(s.to_string());
        cur = s.source();
    }
    match inner.filter(|s| !s.trim().is_empty()) {
        Some(d) => format!("{what} ({})", d.trim()),
        None => what.to_string(),
    }
}

/// 연결 확인용: 0.5초 무음을 보내 인증·모델 이름이 맞는지 본다
pub fn test_connection(base_url: &str, api_key: &str, model: &str) -> Result<()> {
    let t = ApiTranscriber::new(base_url, api_key, model)?;
    let silence = vec![0.0f32; crate::SAMPLE_RATE as usize];
    let wav = crate::wav::encode_pcm16(&silence)?;
    let req = AsrRequest {
        samples: &silence,
        language: "ko",
        prompt: "",
    };
    t.call(&wav, &req).map(|_| ())
}
