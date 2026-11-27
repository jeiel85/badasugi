//! 네이버 클라우드 CLOVA Speech — 장문 인식 API 를 동기(sync)로
//!
//! `POST {Invoke URL}/recognizer/upload`, 헤더 `X-CLOVASPEECH-API-KEY: {Secret Key}`, multipart:
//! `media`(소리 파일) + `params`(JSON) → `{"result":"COMPLETED","text":"…"}`
//!
//! - Invoke URL · Secret Key 는 콘솔의 CLOVA Speech 도메인 › 빌더 › 설정 › 연동 정보에서 받는다 (도메인마다 다르다)
//! - 화자 구분(`diarization`, 기본 켬)은 앱이 따로 하므로 끈다. 낱말 시각(`wordAlignment`)도 쓰지 않는다
//! - 용어집은 `boostings: [{"words": "a,b,c"}]` — 한글 · 영어, 1,000개까지(500개 이하 권장), 한 글자 낱말은 안 된다
//! - 요금: 15초 단위 올림 5원(부가세 별도) · Free 플랜은 한 달 20분 무료
//!
//! 예전 CSR(짧은 문장 인식, Client ID · Secret)은 2026-11-24 에 새 가입을 닫고 2027-01-21 에 끝난다.
//! CLOVA Speech 단문 인식(15초 4원)은 용어 부스팅이 한국어 · 3글자 이상만이라 쓰지 않는다.
//!
//! 문서: <https://api.ncloud-docs.com/docs/ai-application-service-clovaspeech-longsentence-local>

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::blocking::{multipart, Client};

use super::openai::{http_client, parse_json, send, with_retries, ApiError};
use super::{AsrRequest, Transcriber};

/// 사용량 · 요금에 쓰는 모델 이름
pub const MODEL: &str = "장문 인식";
/// 보내는 용어 최대 수 (문서 권장)
pub const MAX_BOOSTINGS: usize = 500;

/// 말하는 동안 미리 보기는 이만큼만 (OpenAI 호환 엔진과 같다)
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(8);

pub struct ClovaTranscriber {
    client: Client,
    url: String,
    secret: String,
}

/// 오류 본문 — `{"message":"…"}` · `{"result":"FAILED","message":"…"}` · API Gateway `{"error":{"message":"…"}}`
fn error_message(v: &serde_json::Value) -> Option<String> {
    let msg = v["message"].as_str().or_else(|| v["error"]["message"].as_str())?;
    Some(match v["result"].as_str().or_else(|| v["error"]["errorCode"].as_str()) {
        Some(code) if code != msg => format!("{msg} ({code})"),
        _ => msg.to_string(),
    })
}

impl ClovaTranscriber {
    pub fn new(invoke_url: &str, secret: &str) -> Result<Self> {
        let base = invoke_url.trim().trim_end_matches('/');
        // 콘솔에서 /recognizer/upload 까지 붙여 복사해도 된다
        let base = base.strip_suffix("/recognizer/upload").unwrap_or(base);
        if base.is_empty() {
            return Err(anyhow!("CLOVA Speech Invoke URL 을 설정에서 입력해 주세요"));
        }
        if !base.starts_with("https://") && !base.starts_with("http://") {
            return Err(anyhow!("CLOVA Speech Invoke URL 은 https:// 로 시작해요 — 콘솔의 연동 정보에서 그대로 복사해 주세요"));
        }
        if secret.trim().is_empty() {
            return Err(anyhow!("CLOVA Speech Secret Key 를 설정에서 입력해 주세요"));
        }
        Ok(Self {
            client: http_client(Duration::from_secs(90))?,
            url: format!("{base}/recognizer/upload"),
            secret: secret.trim().to_string(),
        })
    }

    /// 다시 보내지 않고 한 번만 (연결 테스트)
    pub fn once(&self, req: &AsrRequest) -> Result<String> {
        self.call(&crate::wav::encode_pcm16(req.samples)?, req)
    }

    fn call(&self, wav: &[u8], req: &AsrRequest) -> Result<String> {
        let media = multipart::Part::bytes(wav.to_vec()).file_name("speech.wav").mime_str("audio/wav")?;
        let params = multipart::Part::text(params(req).to_string()).mime_str("application/json")?;
        let form = multipart::Form::new().part("media", media).part("params", params);
        let mut rb = self
            .client
            .post(&self.url)
            .header("X-CLOVASPEECH-API-KEY", &self.secret)
            .header("Accept", "application/json")
            .multipart(form);
        if req.preview.is_some() {
            rb = rb.timeout(PREVIEW_TIMEOUT);
        }
        let body = send(rb, "clova", error_message)?;
        let v = parse_json(&body)?;
        match v["result"].as_str() {
            Some("COMPLETED") | None => Ok(v["text"].as_str().unwrap_or_default().trim().to_string()),
            Some(code) => {
                let msg = v["message"].as_str().unwrap_or_default();
                // 작업이 몰림 · 서버가 바쁨 → 요청 한도처럼 조금 뒤 다시
                if code.contains("TOO_MANY") || code.contains("BUSY") {
                    return Err(ApiError::Status {
                        status: 429,
                        retry_after: None,
                        message: format!("{msg} ({code})"),
                    }
                    .into());
                }
                Err(anyhow!("CLOVA Speech 가 받아 적지 못했어요: {msg} ({code})"))
            }
        }
    }
}

impl Transcriber for ClovaTranscriber {
    fn transcribe(&mut self, req: &AsrRequest) -> Result<String> {
        let wav = crate::wav::encode_pcm16(req.samples)?;
        with_retries(req.preview.is_some(), || self.call(&wav, req))
    }
}

/// 앱의 언어 코드 → CLOVA Speech 언어 (자동 감지는 한국어 + 영어 섞인 말)
fn language(lang: &str) -> &'static str {
    match lang {
        "en" => "en-US",
        "ja" => "ja",
        "zh" => "zh-cn",
        "auto" => "enko",
        _ => "ko-KR",
    }
}

/// 요청 설정 (`params` 칸의 JSON)
fn params(req: &AsrRequest) -> serde_json::Value {
    let mut p = serde_json::json!({
        "language": language(req.language),
        "completion": "sync",
        "wordAlignment": false,
        "fullText": true,
        "noiseFiltering": true,
        "diarization": { "enable": false },
    });
    let words = boostings(req.keywords);
    if !words.is_empty() {
        p["boostings"] = serde_json::json!([{ "words": words.join(",") }]);
    }
    p
}

/// 부스팅할 용어 — 쉼표로 이어 보내므로 쉼표는 빼고, 한 글자 낱말은 빼고(지원 안 됨), 겹치면 하나만, [`MAX_BOOSTINGS`] 개까지
pub fn boostings(terms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in terms {
        let t: String = t.chars().filter(|c| !matches!(c, ',' | '"' | '\\')).collect();
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.chars().filter(|c| !c.is_whitespace()).count() < 2 || out.contains(&t) {
            continue;
        }
        out.push(t);
        if out.len() == MAX_BOOSTINGS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::mpsc;

    fn req<'a>(language: &'a str, keywords: &'a [String]) -> AsrRequest<'a> {
        AsrRequest { samples: &[], language, prompt: "", keywords, context: "", preview: None }
    }

    #[test]
    fn params_turn_off_diarization_and_boost_the_glossary() {
        let kw = vec!["OKPOS".to_string(), "스캔프리".to_string(), "값".to_string(), "정산, 마감".to_string()];
        let p = params(&req("ko", &kw));
        assert_eq!(p["language"], "ko-KR");
        assert_eq!(p["completion"], "sync");
        assert_eq!(p["diarization"]["enable"], false);
        assert_eq!(p["wordAlignment"], false);
        assert_eq!(p["boostings"][0]["words"], "OKPOS,스캔프리,정산 마감", "한 글자는 빼고 쉼표는 지운다");
        // 용어가 없으면 boostings 를 보내지 않는다
        assert!(params(&req("auto", &[])).get("boostings").is_none());
        assert_eq!(params(&req("auto", &[]))["language"], "enko");
    }

    #[test]
    fn invoke_url_is_checked_and_trimmed() {
        assert!(ClovaTranscriber::new("", "s").is_err());
        assert!(ClovaTranscriber::new("clovaspeech-gw.ncloud.com/external/v1/1/abc", "s").is_err());
        assert!(ClovaTranscriber::new("https://clovaspeech-gw.ncloud.com/external/v1/1/abc", " ").is_err());
        let t = ClovaTranscriber::new("https://clovaspeech-gw.ncloud.com/external/v1/1/abc/recognizer/upload/", "s").unwrap();
        assert_eq!(t.url, "https://clovaspeech-gw.ncloud.com/external/v1/1/abc/recognizer/upload");
    }

    fn mock(replies: Vec<String>) -> (String, mpsc::Receiver<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/external/v1/123/abcdef", l.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for reply in replies {
                let (mut s, _) = l.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head_end = loop {
                    let n = s.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                    assert!(n > 0);
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = s.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    buf.extend_from_slice(&chunk[..n]);
                }
                let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
                s.write_all(reply.as_bytes()).unwrap();
            }
        });
        (base, rx)
    }

    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn sync_upload_returns_the_full_text() {
        let (base, rx) = mock(vec![http(
            "200 OK",
            r#"{"result":"COMPLETED","message":"Succeeded","token":"t","segments":[],"text":"스캔프리 결제는 금요일까지 배포해요.","confidence":0.93}"#,
        )]);
        let mut t = ClovaTranscriber::new(&base, "clova-secret").unwrap();
        let samples = vec![0.0f32; 16_000];
        let kw = vec!["스캔프리".to_string()];
        assert_eq!(t.transcribe(&AsrRequest { samples: &samples, ..req("ko", &kw) }).unwrap(), "스캔프리 결제는 금요일까지 배포해요.");
        let got = rx.recv().unwrap();
        assert!(got.starts_with("POST /external/v1/123/abcdef/recognizer/upload "), "{}", &got[..80]);
        assert!(got.to_lowercase().contains("x-clovaspeech-api-key: clova-secret"));
        assert!(got.contains("name=\"media\"; filename=\"speech.wav\""));
        assert!(got.contains("\"completion\":\"sync\"") && got.contains("\"words\":\"스캔프리\""), "params JSON");
    }

    #[test]
    fn wrong_secret_is_fatal_and_busy_is_retried() {
        let (base, _rx) = mock(vec![http("401 Unauthorized", r#"{"message":"Invalid secret"}"#)]);
        let mut t = ClovaTranscriber::new(&base, "wrong").unwrap();
        let samples = vec![0.0f32; 1600];
        let e = t.transcribe(&AsrRequest { samples: &samples, ..req("ko", &[]) }).unwrap_err();
        assert!(!super::super::retryable(&e));
        assert!(e.to_string().contains("401") && e.to_string().contains("Invalid secret"), "{e}");

        // 바쁘다고 하면 다시 보내 두 번째에 받는다
        let (base, rx) = mock(vec![
            http("200 OK", r#"{"result":"ERROR_SERVER_BUSY","message":"server busy"}"#),
            http("200 OK", r#"{"result":"COMPLETED","message":"Succeeded","text":"됐어요."}"#),
        ]);
        let mut t = ClovaTranscriber::new(&base, "s").unwrap();
        assert_eq!(t.transcribe(&AsrRequest { samples: &samples, ..req("ko", &[]) }).unwrap(), "됐어요.");
        assert_eq!(rx.try_iter().count(), 2);
    }
}
