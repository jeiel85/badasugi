//! OpenAI 호환 음성 인식 API (`POST {base}/audio/transcriptions`)
//!
//! OpenAI(gpt-transcribe, gpt-4o-transcribe, whisper-1), Groq(whisper-large-v3-turbo), 자체 호스팅 서버
//! (faster-whisper-server/speaches, LocalAI 등) 를 모두 같은 코드로 호출한다.

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::blocking::{multipart, Client};

use super::{AsrRequest, Transcriber};

/// 말하는 동안 미리 보기 요청은 이만큼만 기다린다 — 늦게 오는 미리 보기는 쓸모가 없고, 그동안 워커가 묶인다
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(8);

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
        let mut form = multipart::Form::new().part("file", file);
        for (name, value) in form_fields(&self.model, req) {
            form = form.text(name, value);
        }
        let mut rb = self.client.post(&self.url).multipart(form);
        if req.preview.is_some() {
            rb = rb.timeout(PREVIEW_TIMEOUT);
        }
        if !self.api_key.is_empty() {
            rb = rb.bearer_auth(&self.api_key);
        }
        let body = send(rb, crate::usage::provider_of(&self.url), |v| v["error"]["message"].as_str().map(str::to_string))?;
        let v = parse_json(&body)?;
        Ok(v["text"].as_str().unwrap_or_default().to_string())
    }
}

impl Transcriber for ApiTranscriber {
    fn transcribe(&mut self, req: &AsrRequest) -> Result<String> {
        let wav = crate::wav::encode_pcm16(req.samples)?;
        with_retries(req.preview.is_some(), || self.call(&wav, req))
    }
}

/// 요청을 보내고 성공한 응답의 본문을 돌려준다. 서버에 닿지 못했으면 [`ApiError::Network`],
/// 오류 상태면 [`ApiError::Status`] — 메시지는 `message` 로 응답 JSON 에서 고르고, 못 고르면 본문 앞부분.
/// 공급자가 응답 헤더로 알려 주는 남은 한도는 `provider` 이름으로 기억한다 (설정 › 사용량).
pub(crate) fn send(
    rb: reqwest::blocking::RequestBuilder,
    provider: &str,
    message: impl Fn(&serde_json::Value) -> Option<String>,
) -> Result<String, ApiError> {
    let resp = rb.send().map_err(|e| ApiError::Network(describe_http_error(&e)))?;
    crate::usage::record_headers_for(provider, resp.headers());
    let status = resp.status();
    let retry_after = retry_after(resp.headers());
    let body = resp.text().map_err(|e| ApiError::Network(describe_http_error(&e)))?;
    if status.is_success() {
        return Ok(body);
    }
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| message(&v))
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| body.chars().take(300).collect());
    Err(ApiError::Status {
        status: status.as_u16(),
        retry_after,
        message,
    })
}

/// 성공한 응답 본문을 JSON 으로 — 해석하지 못하면 본문 앞부분을 보여 준다
pub(crate) fn parse_json(body: &str) -> Result<serde_json::Value> {
    serde_json::from_str(body).map_err(|_| anyhow!("API 응답 해석 실패: {}", body.chars().take(200).collect::<String>()))
}

/// 확정 문장은 세 번까지 보낸다 — 연결 문제 · 요청 한도 · 서비스 장애일 때만 다시 (인증 · 요청 오류는 다시 보내도 소용없다).
/// 요청 한도(429)면 공급자가 알려 준 만큼 기다린다 (너무 길면 20초까지만 — 나머지는 파이프라인이 맡는다).
/// 미리 보기(`preview`)는 한 번만 보낸다 — 다시 보내거나 요청 한도(Retry-After)를 기다리면 확정 문장을 받아 적을 워커가
/// 수십 초씩 묶이고, 녹음을 멈출 때도 그만큼 기다린다. 실패하면 파이프라인이 미리 보기를 끈다.
pub(crate) fn with_retries(preview: bool, mut call: impl FnMut() -> Result<String>) -> Result<String> {
    if preview {
        return call();
    }
    let mut last_err = None;
    for attempt in 0..3u32 {
        match call() {
            Ok(t) => return Ok(t),
            Err(e) => {
                if !super::retryable(&e) {
                    return Err(e);
                }
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

/// OpenAI 의 새 음성 인식 모델(gpt-transcribe, 2026-07) — 언어를 `languages[]` 로 받고(`language` 와 함께 보내면 안 됨)
/// 용어를 `keywords[]` 로 따로 받는다. whisper-1 · gpt-4o-transcribe 와 Groq · 자체 서버는 예전 형식 그대로.
fn is_gpt_transcribe(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.starts_with("gpt-transcribe") || m.starts_with("gpt-live-transcribe")
}

/// OpenAI 의 GPT 기반 인식 모델 (gpt-transcribe · gpt-4o-transcribe · gpt-4o-mini-transcribe …) — Whisper 가 아니다
pub fn is_gpt_transcriber(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.starts_with("gpt-") && m.contains("transcribe")
}

/// 요청 본문의 글자 칸 (파일 빼고). 같은 이름을 여러 번 보내면 배열이다 (`keywords[]`)
fn form_fields(model: &str, req: &AsrRequest) -> Vec<(&'static str, String)> {
    let new_api = is_gpt_transcribe(model);
    let mut f = vec![
        ("model", model.to_string()),
        ("response_format", "json".to_string()),
        ("temperature", "0".to_string()),
    ];
    if req.language != "auto" && !req.language.is_empty() {
        f.push((if new_api { "languages[]" } else { "language" }, req.language.to_string()));
    }
    let prompt = req.full_prompt();
    if !prompt.is_empty() {
        f.push(("prompt", prompt));
    }
    if new_api {
        // 한 줄이어야 하고 '<' '>' 가 있으면 요청 전체가 거절된다
        for k in req.keywords {
            let k: String = k.chars().filter(|c| !matches!(c, '<' | '>' | '\r' | '\n')).collect();
            let k = k.trim();
            if !k.is_empty() {
                f.push(("keywords[]", k.to_string()));
            }
        }
    }
    f
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
    /// 같은 요청을 다시 보내도 결과가 같을 오류 (키 · 크레딧 · 모델 이름 · 요청 형식)
    pub fn is_fatal(&self) -> bool {
        matches!(self, ApiError::Status { status, .. } if matches!(status, 400 | 401 | 402 | 403 | 404 | 413 | 415 | 422))
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
                    402 => " — 크레딧(잔액)을 확인해 주세요",
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
        keywords: &[],
        context: "",
        preview: None,
        abort: None,
    };
    t.call(&wav, &req).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn req<'a>(keywords: &'a [String], context: &'a str) -> AsrRequest<'a> {
        AsrRequest {
            samples: &[],
            language: "ko",
            prompt: "회의 녹취록입니다. 주요 용어: OKPOS, 스캔프리.",
            keywords,
            context,
            preview: None,
            abort: None,
        }
    }

    fn names(f: &[(&'static str, String)]) -> Vec<&'static str> {
        f.iter().map(|(n, _)| *n).collect()
    }

    #[test]
    fn gpt_transcribe_sends_languages_array_and_keywords() {
        let kw = vec!["OKPOS".to_string(), "스캔<프리>".to_string(), "  ".to_string()];
        let f = form_fields("gpt-transcribe", &req(&kw, "그래서 결제는 됐어요."));
        let n = names(&f);
        assert!(n.contains(&"languages[]"));
        assert!(!n.contains(&"language"), "language 와 languages 를 함께 보내면 안 된다");
        let kws: Vec<&str> = f.iter().filter(|(n, _)| *n == "keywords[]").map(|(_, v)| v.as_str()).collect();
        assert_eq!(kws, ["OKPOS", "스캔프리"]);
        let prompt = &f.iter().find(|(n, _)| *n == "prompt").unwrap().1;
        assert!(prompt.ends_with("그래서 결제는 됐어요."), "앞 문맥은 프롬프트 끝에: {prompt}");
    }

    #[test]
    fn whisper_models_keep_the_old_form() {
        let kw = vec!["OKPOS".to_string()];
        for model in ["whisper-large-v3", "whisper-1", "gpt-4o-transcribe"] {
            let n = names(&form_fields(model, &req(&kw, "")));
            assert!(n.contains(&"language"), "{model}");
            assert!(!n.contains(&"languages[]") && !n.contains(&"keywords[]"), "{model}");
        }
    }

    /// 요청마다 429 (Retry-After 1초) 를 돌려주는 가짜 서버 → (주소, 받은 요청 수)
    fn rate_limited_server() -> (String, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                h.fetch_add(1, Ordering::SeqCst);
                // 요청을 끝까지 읽지 않아도 되게 잠깐만 읽고 답한다
                let mut buf = [0u8; 65536];
                let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                let _ = std::io::Read::read(&mut s, &mut buf);
                let body = r#"{"error":{"message":"rate limit"}}"#;
                let resp = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut s, resp.as_bytes());
            }
        });
        (format!("http://{addr}/v1"), hits)
    }

    #[test]
    fn preview_requests_are_sent_once_without_waiting() {
        let (url, hits) = rate_limited_server();
        let mut t = ApiTranscriber::new(&url, "", "whisper-large-v3-turbo").unwrap();
        let samples = vec![0.0f32; 16_000];
        let abort = || false;
        let req = AsrRequest {
            samples: &samples,
            language: "ko",
            prompt: "",
            keywords: &[],
            context: "",
            preview: Some(&abort),
            abort: None,
        };
        let t0 = std::time::Instant::now();
        assert!(t.transcribe(&req).is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 1, "미리 보기는 다시 보내지 않는다");
        assert!(t0.elapsed() < Duration::from_millis(900), "Retry-After 를 기다리지 않는다: {:?}", t0.elapsed());
        // 확정 문장은 그대로 다시 보낸다 (요청 한도면 기다렸다가 — 세 번까지)
        let req = AsrRequest { preview: None, ..req };
        assert!(t.transcribe(&req).is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn auto_language_and_empty_prompt_are_left_out() {
        let r = AsrRequest {
            samples: &[],
            language: "auto",
            prompt: " ",
            keywords: &[],
            context: "",
            preview: None,
            abort: None,
        };
        assert_eq!(names(&form_fields("gpt-transcribe", &r)), ["model", "response_format", "temperature"]);
    }
}
