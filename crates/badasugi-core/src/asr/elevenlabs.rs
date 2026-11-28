//! ElevenLabs 음성 인식 API (Scribe) — `POST {base}/v1/speech-to-text`, 헤더 `xi-api-key`, multipart
//!
//! - `model_id`: `scribe_v3`(가장 정확, 2026-11 기준) · `scribe_v2`. `…_realtime` · `…_live` 는 웹소켓 전용이라 쓰지 않는다
//! - `language_code`: "ko" (ISO-639-1 · -3 둘 다 받는다) — 자동 감지면 보내지 않는다
//! - `tag_audio_events=false` ("(웃음)" 같은 소리 표시 빼기) · `timestamps_granularity=none` · `diarize=false`
//!   (화자 구분은 앱이 따로 한다) · `no_verbatim=true` ("음 · 어" 같은 군말 빼기)
//! - 용어집은 `keyterms` 를 낱말마다 같은 이름으로 되풀이해 보낸다 (최대 1,000개 · 50자 · 5낱말, `<>{}[]\` 는 못 씀).
//!   100개를 넘기면 요청마다 최소 20초로 매기므로 100개까지만 보낸다. 용어를 넣으면 요금이 시간당 $0.05 오른다.
//!
//! 문서: <https://elevenlabs.io/docs/api-reference/speech-to-text/convert>

use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::blocking::{multipart, Client};

use super::openai::{http_client, parse_json, send, with_retries};
use super::{AsrRequest, Transcriber};

pub const DEFAULT_URL: &str = "https://api.elevenlabs.io";
pub const DEFAULT_MODEL: &str = "scribe_v3";
/// 보내는 용어 최대 수 — 넘으면 요청마다 최소 20초로 매긴다
pub const MAX_KEYTERMS: usize = 100;

/// 말하는 동안 미리 보기는 이만큼만 (OpenAI 호환 엔진과 같다)
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(8);

pub struct ElevenLabsTranscriber {
    client: Client,
    url: String,
    api_key: String,
    model: String,
}

/// ElevenLabs 오류 본문 `{"detail":{"code","message","status"}}` · 검증 오류 `{"detail":[{"msg"}]}` · `{"detail":"…"}`
fn error_message(v: &serde_json::Value) -> Option<String> {
    let d = &v["detail"];
    if let Some(s) = d.as_str() {
        return Some(s.to_string());
    }
    if let Some(items) = d.as_array() {
        let msgs: Vec<&str> = items.iter().filter_map(|i| i["msg"].as_str()).collect();
        return (!msgs.is_empty()).then(|| msgs.join(" · "));
    }
    let msg = d["message"].as_str()?;
    Some(match d["code"].as_str().or_else(|| d["status"].as_str()) {
        Some(code) => format!("{msg} ({code})"),
        None => msg.to_string(),
    })
}

impl ElevenLabsTranscriber {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Result<Self> {
        if api_key.trim().is_empty() {
            return Err(anyhow!("ElevenLabs API 키를 설정에서 입력해 주세요"));
        }
        let base = match base_url.trim().trim_end_matches('/') {
            "" => DEFAULT_URL,
            b => b,
        };
        Ok(Self {
            client: http_client(Duration::from_secs(90))?,
            url: format!("{base}/v1/speech-to-text"),
            api_key: api_key.trim().to_string(),
            model: match model.trim() {
                "" => DEFAULT_MODEL.to_string(),
                m => m.to_string(),
            },
        })
    }

    /// 다시 보내지 않고 한 번만 (연결 테스트)
    pub fn once(&self, req: &AsrRequest) -> Result<String> {
        self.call(&crate::wav::encode_pcm16(req.samples)?, req)
    }

    fn call(&self, wav: &[u8], req: &AsrRequest) -> Result<String> {
        let file = multipart::Part::bytes(wav.to_vec()).file_name("speech.wav").mime_str("audio/wav")?;
        let mut form = multipart::Form::new().part("file", file);
        for (name, value) in form_fields(&self.model, req) {
            form = form.text(name, value);
        }
        let mut rb = self.client.post(&self.url).header("xi-api-key", &self.api_key).multipart(form);
        if req.preview.is_some() {
            rb = rb.timeout(PREVIEW_TIMEOUT);
        }
        let body = send(rb, "elevenlabs", error_message)?;
        let v = parse_json(&body)?;
        Ok(v["text"].as_str().unwrap_or_default().trim().to_string())
    }
}

impl Transcriber for ElevenLabsTranscriber {
    fn transcribe(&mut self, req: &AsrRequest) -> Result<String> {
        let wav = crate::wav::encode_pcm16(req.samples)?;
        with_retries(req.preview.is_some(), || self.call(&wav, req))
    }
}

/// 요청 본문의 글자 칸 (파일 빼고)
fn form_fields(model: &str, req: &AsrRequest) -> Vec<(&'static str, String)> {
    let mut f = vec![
        ("model_id", model.to_string()),
        ("tag_audio_events", "false".to_string()),
        ("timestamps_granularity", "none".to_string()),
        ("diarize", "false".to_string()),
        ("no_verbatim", "true".to_string()),
        ("temperature", "0".to_string()),
        ("file_format", "other".to_string()),
    ];
    if req.language != "auto" && !req.language.is_empty() {
        f.push(("language_code", req.language.to_string()));
    }
    for k in keyterms(req.keywords) {
        f.push(("keyterms", k));
    }
    f
}

/// 보낼 용어 — 50자 · 5낱말 이하, 쓸 수 없는 글자(`<>{}[]\`)는 빼고, 겹치면 하나만, [`MAX_KEYTERMS`] 개까지
pub fn keyterms(terms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in terms {
        let t: String = t.chars().filter(|c| !matches!(c, '<' | '>' | '{' | '}' | '[' | ']' | '\\')).collect();
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() || t.chars().count() > 50 || t.split(' ').count() > 5 || out.contains(&t) {
            continue;
        }
        out.push(t);
        if out.len() == MAX_KEYTERMS {
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
        AsrRequest {
            samples: &[],
            language,
            prompt: "회의 녹취록입니다. 주요 용어: OKPOS.",
            keywords,
            context: "앞 문맥은 보내지 않는다",
            preview: None,
            abort: None,
        }
    }

    #[test]
    fn keyterms_are_cleaned_and_capped() {
        let terms: Vec<String> = ["OKPOS", "스캔<프리>", "OKPOS", "  ", "가 나 다 라 마 바", "{정산}"].iter().map(|s| s.to_string()).collect();
        assert_eq!(keyterms(&terms), ["OKPOS", "스캔프리", "정산"]);
        let many: Vec<String> = (0..300).map(|i| format!("용어{i}")).collect();
        assert_eq!(keyterms(&many).len(), MAX_KEYTERMS);
    }

    #[test]
    fn form_sends_plain_text_options_and_repeated_keyterms() {
        let kw = vec!["OKPOS".to_string(), "스캔프리".to_string()];
        let f = form_fields("scribe_v3", &req("ko", &kw));
        let get = |n: &str| f.iter().filter(|(k, _)| *k == n).map(|(_, v)| v.as_str()).collect::<Vec<_>>();
        assert_eq!(get("model_id"), ["scribe_v3"]);
        assert_eq!(get("language_code"), ["ko"]);
        assert_eq!(get("tag_audio_events"), ["false"]);
        assert_eq!(get("keyterms"), ["OKPOS", "스캔프리"]);
        assert!(get("prompt").is_empty(), "프롬프트 칸은 없다");
        // 자동 감지면 언어를 보내지 않는다
        assert!(form_fields("scribe_v3", &req("auto", &[])).iter().all(|(k, _)| *k != "language_code"));
    }

    /// 요청 하나에 정해 둔 응답을 돌려주는 가짜 서버 → (주소, 받은 요청 머리 + 본문)
    fn mock(replies: Vec<String>) -> (String, mpsc::Receiver<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
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
    fn transcribes_with_the_key_header() {
        let (base, rx) = mock(vec![http(
            "200 OK",
            r#"{"language_code":"kor","language_probability":0.99,"text":" 스캔프리 결제를 금요일까지 배포해요. ","words":[]}"#,
        )]);
        let mut t = ElevenLabsTranscriber::new(&base, "xi-test", "").unwrap();
        let samples = vec![0.0f32; 16_000];
        let kw = vec!["스캔프리".to_string()];
        let r = AsrRequest { samples: &samples, ..req("ko", &kw) };
        assert_eq!(t.transcribe(&r).unwrap(), "스캔프리 결제를 금요일까지 배포해요.");
        let got = rx.recv().unwrap();
        assert!(got.starts_with("POST /v1/speech-to-text "), "{}", &got[..60]);
        assert!(got.to_lowercase().contains("xi-api-key: xi-test"));
        assert!(got.contains("name=\"keyterms\"\r\n\r\n스캔프리"), "용어는 낱말마다 같은 이름으로");
        assert!(got.contains("name=\"model_id\"\r\n\r\nscribe_v3"), "모델을 비우면 scribe_v3");
    }

    #[test]
    fn bad_key_and_no_credits_are_not_retried() {
        for (status, body, want) in [
            ("401 Unauthorized", r#"{"detail":{"type":"authentication_error","code":"invalid_api_key","message":"Invalid API key","status":"invalid_api_key"}}"#, "Invalid API key (invalid_api_key)"),
            ("422 Unprocessable Entity", r#"{"detail":[{"loc":["body","model_id"],"msg":"Input should be 'scribe_v2' or 'scribe_v3'"}]}"#, "Input should be"),
        ] {
            let (base, _rx) = mock(vec![http(status, body)]);
            let mut t = ElevenLabsTranscriber::new(&base, "xi-test", "scribe_v3").unwrap();
            let samples = vec![0.0f32; 1600];
            let e = t.transcribe(&AsrRequest { samples: &samples, ..req("ko", &[]) }).unwrap_err();
            assert!(!super::super::retryable(&e), "{status}");
            assert!(e.to_string().contains(want), "{e}");
        }
    }

    #[test]
    fn connection_test_sends_one_second_of_silence_once() {
        // 연결 테스트는 오류여도(서비스 장애) 다시 보내지 않는다
        let (base, rx) = mock(vec![
            http("200 OK", r#"{"text":""}"#),
            http("503 Service Unavailable", r#"{"detail":{"message":"busy"}}"#),
        ]);
        let cfg = super::super::EngineConfig::Service {
            service: super::super::Service::Elevenlabs,
            base_url: base,
            key_id: String::new(),
            secret: "xi-test".into(),
            model: String::new(),
        };
        super::super::test_connection(&cfg).unwrap();
        let got = rx.recv().unwrap();
        assert!(got.contains("name=\"language_code\"\r\n\r\nko"));
        let e = super::super::test_connection(&cfg).unwrap_err();
        assert!(e.to_string().contains("503"), "{e}");
        assert_eq!(rx.try_iter().count(), 1, "한 번만 보냈다");
    }

    #[test]
    fn out_of_credits_is_fatal_too() {
        // 402 (크레딧 부족)도 다시 보내도 소용없다
        let (base, _rx) = mock(vec![http("402 Payment Required", r#"{"detail":{"code":"insufficient_credits","message":"You have run out of credits"}}"#)]);
        let mut t = ElevenLabsTranscriber::new(&base, "xi-test", "").unwrap();
        let samples = vec![0.0f32; 1600];
        let e = t.transcribe(&AsrRequest { samples: &samples, ..req("ko", &[]) }).unwrap_err();
        assert!(!super::super::retryable(&e));
        assert!(e.to_string().contains("run out of credits"), "{e}");
    }
}
