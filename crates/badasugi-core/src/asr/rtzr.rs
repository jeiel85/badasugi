//! 리턴제로(RTZR) 음성 인식 API — 스트리밍 STT (WebSocket)
//!
//! 1. `POST {base}/v1/authenticate` (client_id · client_secret, 폼) → `access_token` · `expire_at` (6시간)
//! 2. `wss://{host}/v1/transcribe:streaming?sample_rate=16000&encoding=LINEAR16&…` 에 `Authorization: Bearer` 로 연결
//! 3. 발화 소리(헤더 없는 PCM16)를 바이너리 프레임으로 보내고 텍스트 `EOS` 로 맺는다
//! 4. 서버가 보내는 결과 중 `final: true` 인 것의 `alternatives[0].text` 를 이어 붙인다 — 다 보내면 서버가 연결을 닫는다
//!
//! 파일 STT(`POST /v1/transcribe` → `GET /v1/transcribe/{id}`)는 쓰지 않는다: 결과 조회를 5초 간격으로 하라고 하고
//! (더 자주 하면 429), 요청이 몰리면 처리를 시작하기까지 30분 넘게 걸릴 수 있다고 해서 발화마다 바로 받아 적는 앱과 맞지 않는다.
//! 요금은 둘이 같다 — 시간당 1,000원(부가세 별도, 월 1,000시간까지), 연결(파일)마다 최소 10초.
//!
//! 문서: <https://developers.rtzr.ai/docs/stt-streaming/websocket/>, <https://developers.rtzr.ai/docs/authentications/>

use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use reqwest::blocking::Client;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::Message;

use super::openai::{http_client, parse_json, send, with_retries, ApiError};
use super::{AsrRequest, Transcriber};

pub const DEFAULT_URL: &str = "https://openapi.vito.ai";
/// 사용량 · 요금에 쓰는 모델 이름 (스트리밍 모델은 언어마다 sommers_ko · sommers_ja · whisper 를 고른다 — 요금은 같다)
pub const DEFAULT_MODEL: &str = "sommers";

/// 확정 문장 하나를 받아 적는 데 기다리는 최대 시간
const TIMEOUT: Duration = Duration::from_secs(90);
/// 말하는 동안 미리 보기는 이만큼만 (OpenAI 호환 엔진과 같다)
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(8);
/// 한 번에 보내는 소리 (바이트) — 0.1초
const CHUNK_BYTES: usize = 3200;
/// 토큰이 이만큼 안쪽으로 남으면 새로 받는다
const TOKEN_MARGIN_SECS: u64 = 300;

pub struct RtzrTranscriber {
    client: Client,
    base: String,
    client_id: String,
    client_secret: String,
}

/// 받은 토큰 — 워커 여럿 · 녹음 여러 번이 함께 쓴다 ((주소, client id) → (토큰, 만료 유닉스 초))
fn tokens() -> &'static Mutex<HashMap<(String, String), (String, u64)>> {
    static T: OnceLock<Mutex<HashMap<(String, String), (String, u64)>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 리턴제로 오류 본문 `{"code":"H0002","msg":"…"}` 에서 메시지
fn error_message(v: &serde_json::Value) -> Option<String> {
    let msg = v["msg"].as_str().or_else(|| v["message"].as_str())?;
    Some(match v["code"].as_str() {
        Some(code) => format!("{msg} ({code})"),
        None => msg.to_string(),
    })
}

impl RtzrTranscriber {
    pub fn new(base_url: &str, client_id: &str, client_secret: &str, _model: &str) -> Result<Self> {
        let (client_id, client_secret) = (client_id.trim(), client_secret.trim());
        if client_id.is_empty() || client_secret.is_empty() {
            return Err(anyhow!("리턴제로 Client ID 와 Client Secret 을 설정에서 입력해 주세요"));
        }
        let base = match base_url.trim().trim_end_matches('/') {
            "" => DEFAULT_URL.to_string(),
            b => b.to_string(),
        };
        Ok(Self {
            client: http_client(Duration::from_secs(30))?,
            base,
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
        })
    }

    /// 쓸 수 있는 토큰 — 없거나 곧 끝나면(또는 `renew`) 새로 받는다
    fn token(&self, renew: bool) -> Result<String> {
        let key = (self.base.clone(), self.client_id.clone());
        if !renew {
            if let Some((t, exp)) = tokens().lock().ok().and_then(|m| m.get(&key).cloned()) {
                if exp > now_secs() + TOKEN_MARGIN_SECS {
                    return Ok(t);
                }
            }
        }
        // 폼 본문 (reqwest 의 form 기능 없이)
        let rb = self
            .client
            .post(format!("{}/v1/authenticate", self.base))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("client_id={}&client_secret={}", encode(&self.client_id), encode(&self.client_secret)));
        let body = send(rb, "rtzr", error_message)?;
        let v = parse_json(&body)?;
        let token = v["access_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| anyhow!("리턴제로 인증 응답에 토큰이 없어요: {}", body.chars().take(200).collect::<String>()))?
            .to_string();
        // 만료 시각을 알려 주지 않으면 문서대로 6시간
        let exp = v["expire_at"].as_u64().unwrap_or(now_secs() + 6 * 3600);
        if let Ok(mut m) = tokens().lock() {
            m.insert(key, (token.clone(), exp));
        }
        Ok(token)
    }

    /// 스트리밍 주소 (쿼리까지)
    fn stream_url(&self, req: &AsrRequest) -> String {
        let ws = if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.base.clone()
        };
        let mut q = vec![
            ("sample_rate", crate::SAMPLE_RATE.to_string()),
            ("encoding", "LINEAR16".into()),
            ("domain", "MEETING".into()),
            ("use_itn", "true".into()),
            ("use_disfluency_filter", "true".into()),
            ("use_profanity_filter", "false".into()),
            ("use_punctuation", "true".into()),
        ];
        let korean = matches!(req.language, "ko" | "auto" | "");
        match req.language {
            "ja" => q.push(("model_name", "sommers_ja".into())),
            l if !korean => {
                q.push(("model_name", "whisper".into()));
                q.push(("language", l.into()));
            }
            _ => q.push(("model_name", "sommers_ko".into())),
        }
        // 키워드 부스팅은 한국어 모델만
        if korean {
            let kw = keywords(req.keywords);
            if !kw.is_empty() {
                q.push(("keywords", kw.join(",")));
            }
        }
        let query: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", encode(v))).collect();
        format!("{ws}/v1/transcribe:streaming?{}", query.join("&"))
    }

    /// 발화 하나를 한 연결로 받아 적는다
    fn stream(&self, token: &str, pcm: &[u8], req: &AsrRequest, deadline: Instant) -> Result<String> {
        let mut request = self
            .stream_url(req)
            .into_client_request()
            .map_err(|e| anyhow!("리턴제로 주소가 올바르지 않아요: {e}"))?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().map_err(|_| anyhow!("리턴제로 토큰에 쓸 수 없는 글자가 있어요"))?,
        );
        let uri = request.uri().clone();
        let host = uri.host().unwrap_or_default().trim_matches(['[', ']']).to_string();
        let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("wss") { 443 } else { 80 });
        let tcp = connect(&host, port, deadline)?;
        let (mut ws, _) = tungstenite::client_tls_with_config(request, tcp, None, None).map_err(|e| match e {
            tungstenite::HandshakeError::Failure(e) => handshake_error(e),
            tungstenite::HandshakeError::Interrupted(_) => ApiError::Network("리턴제로 서버와 연결하다 멈췄어요".into()),
        })?;
        let io = |e: tungstenite::Error| ApiError::Network(format!("리턴제로 서버와 주고받지 못했어요 ({e})"));
        for chunk in pcm.chunks(CHUNK_BYTES) {
            ws.send(Message::binary(chunk.to_vec())).map_err(io)?;
        }
        ws.send(Message::text("EOS")).map_err(io)?;

        let mut parts: Vec<String> = Vec::new();
        loop {
            if Instant::now() >= deadline {
                let _ = ws.close(None);
                return Err(ApiError::Network("리턴제로 서버 응답이 너무 늦어요".into()).into());
            }
            match ws.read() {
                Ok(Message::Text(t)) => {
                    let v: serde_json::Value = match serde_json::from_str(t.as_str()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if v.get("alternatives").is_none() {
                        // 결과가 아닌 오류 알림 {"code","msg"}
                        if let Some(m) = error_message(&v) {
                            let _ = ws.close(None);
                            return Err(anyhow!("리턴제로가 받아 적지 못했어요: {m}"));
                        }
                        continue;
                    }
                    if v["final"].as_bool() == Some(true) {
                        let text = v["alternatives"][0]["text"].as_str().unwrap_or_default().trim();
                        if !text.is_empty() {
                            parts.push(text.to_string());
                        }
                    }
                }
                Ok(Message::Close(frame)) => {
                    if let Some(f) = frame.filter(|f| !matches!(f.code, CloseCode::Normal | CloseCode::Away)) {
                        let reason = f.reason.as_str().trim();
                        return Err(anyhow!(
                            "리턴제로가 연결을 닫았어요 ({}{}{})",
                            u16::from(f.code),
                            if reason.is_empty() { "" } else { " · " },
                            reason
                        ));
                    }
                    break;
                }
                Ok(_) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    return Err(ApiError::Network("리턴제로 서버 응답이 너무 늦어요".into()).into());
                }
                Err(e) => return Err(io(e).into()),
            }
        }
        Ok(parts.join(" "))
    }

    /// 다시 보내지 않고 한 번만 (연결 테스트)
    pub fn once(&self, req: &AsrRequest) -> Result<String> {
        self.call(&crate::wav::pcm16_le(req.samples), req)
    }

    fn call(&self, pcm: &[u8], req: &AsrRequest) -> Result<String> {
        let deadline = Instant::now() + if req.preview.is_some() { PREVIEW_TIMEOUT } else { TIMEOUT };
        let token = self.token(false)?;
        match self.stream(&token, pcm, req, deadline) {
            // 토큰이 서버에서 먼저 끝났으면 한 번만 새로 받아 다시
            Err(e) if is_unauthorized(&e) => {
                let token = self.token(true)?;
                self.stream(&token, pcm, req, deadline)
            }
            r => r,
        }
    }
}

impl Transcriber for RtzrTranscriber {
    fn transcribe(&mut self, req: &AsrRequest) -> Result<String> {
        let pcm = crate::wav::pcm16_le(req.samples);
        with_retries(req.preview.is_some(), || self.call(&pcm, req))
    }
}

fn is_unauthorized(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::Status { status: 401, .. }))
}

/// 키워드 부스팅에 넣을 용어 — 리턴제로는 한글 음절과 띄어쓰기로만 된 20자 이하 낱말을 100개까지 받는다
/// (영문 · 숫자가 섞인 용어는 뺀다 — 넣으면 요청이 거절될 수 있다)
fn keywords(terms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in terms {
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        let ok = !t.is_empty()
            && t.chars().count() <= 20
            && t.chars().all(|c| c == ' ' || ('가'..='힣').contains(&c));
        if ok && !out.contains(&t) {
            out.push(t);
        }
        if out.len() == 100 {
            break;
        }
    }
    out
}

/// 쿼리 값 퍼센트 인코딩 (영숫자와 `-_.~` 만 그대로)
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// TCP 연결 (연결 10초 · 읽기 · 쓰기는 남은 시간까지)
fn connect(host: &str, port: u16, deadline: Instant) -> Result<TcpStream, ApiError> {
    let cannot = |d: String| ApiError::Network(format!("서버에 연결할 수 없어요 — 인터넷 연결이나 주소를 확인해 주세요 ({d})"));
    let addrs = (host, port).to_socket_addrs().map_err(|e| cannot(format!("{host}: {e}")))?;
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
            Ok(s) => {
                let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(100));
                let _ = s.set_read_timeout(Some(left));
                let _ = s.set_write_timeout(Some(left));
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(cannot(last.map_or_else(|| format!("{host} 주소를 찾지 못함"), |e| e.to_string())))
}

/// 연결(업그레이드) 실패 — 서버가 HTTP 오류로 답했으면 상태 · 메시지를, 아니면 연결 문제로
fn handshake_error(e: tungstenite::Error) -> ApiError {
    match e {
        tungstenite::Error::Http(resp) => {
            let status = resp.status().as_u16();
            let body = resp.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default().to_string();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| error_message(&v))
                .unwrap_or_else(|| body.chars().take(300).collect());
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|s| s.is_finite() && *s >= 0.0)
                .map(Duration::from_secs_f64);
            ApiError::Status { status, retry_after, message }
        }
        tungstenite::Error::Io(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            ApiError::Network("리턴제로 서버 응답이 너무 늦어요".into())
        }
        tungstenite::Error::Tls(e) => ApiError::Network(format!("리턴제로 서버와 안전한 연결을 맺지 못했어요 ({e})")),
        e => ApiError::Network(format!("리턴제로 서버와 연결하지 못했어요 ({e})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    #[test]
    fn keywords_keep_only_short_hangul_terms() {
        let terms: Vec<String> = ["스캔프리", "OKPOS", "키오스크 정산", "결제2", "  포스  기기 ", "스캔프리", "가나다라마바사아자차카타파하가나다라마바사"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(keywords(&terms), ["스캔프리", "키오스크 정산", "포스 기기"]);
        let many: Vec<String> = (0..150).map(|i| format!("용어{}", char::from_u32(0xAC00 + i).unwrap())).collect();
        assert_eq!(keywords(&many).len(), 100);
    }

    #[test]
    fn stream_url_picks_model_by_language_and_encodes_keywords() {
        let t = RtzrTranscriber::new("https://openapi.vito.ai/", "id", "secret", "").unwrap();
        let kw = vec!["스캔프리".to_string(), "OKPOS".to_string()];
        let req = |language| AsrRequest { samples: &[], language, prompt: "", keywords: &kw, context: "", preview: None, abort: None };
        let ko = t.stream_url(&req("ko"));
        assert!(ko.starts_with("wss://openapi.vito.ai/v1/transcribe:streaming?sample_rate=16000&encoding=LINEAR16"), "{ko}");
        assert!(ko.contains("model_name=sommers_ko"));
        assert!(ko.contains("keywords=%EC%8A%A4%EC%BA%94%ED%94%84%EB%A6%AC"), "한글 용어만 인코딩해서: {ko}");
        assert!(!ko.contains("OKPOS"));
        let en = t.stream_url(&req("en"));
        assert!(en.contains("model_name=whisper") && en.contains("language=en") && !en.contains("keywords="), "{en}");
        assert!(t.stream_url(&req("ja")).contains("model_name=sommers_ja"));
    }

    #[test]
    fn needs_both_id_and_secret() {
        assert!(RtzrTranscriber::new("", "id", " ", "").is_err());
        assert!(RtzrTranscriber::new("", "", "secret", "").is_err());
    }

    /// 가짜 리턴제로 서버 — 같은 포트에서 인증(HTTP)과 스트리밍(WebSocket)을 받는다.
    /// `reject_first_stream`: 첫 스트리밍 연결은 401 (토큰이 서버에서 먼저 끝난 경우)
    struct Mock {
        base: String,
        auths: Arc<AtomicUsize>,
        /// 스트리밍 연결마다 (쿼리, Authorization, 받은 소리 바이트 수)
        streams: mpsc::Receiver<(String, String, usize)>,
    }

    fn mock(reject_first_stream: bool, finals: &'static [&'static str]) -> Mock {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let auths = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let a = auths.clone();
        std::thread::spawn(move || {
            let mut rejected = !reject_first_stream;
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut head = [0u8; 4];
                let _ = s.peek(&mut head);
                if &head == b"POST" {
                    // 인증: 요청을 읽고 토큰을 준다
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    assert!(req.contains("POST /v1/authenticate"), "{req}");
                    let i = a.fetch_add(1, Ordering::SeqCst) + 1;
                    let body = format!(r#"{{"access_token":"tok{i}","expire_at":{}}}"#, now_secs() + 3600 * 6);
                    let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    continue;
                }
                let mut seen = (String::new(), String::new());
                let reject = !rejected;
                rejected = true;
                let ws = tungstenite::accept_hdr(s, |r: &tungstenite::handshake::server::Request, resp| {
                    seen = (
                        r.uri().query().unwrap_or_default().to_string(),
                        r.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string(),
                    );
                    if reject {
                        let mut e = tungstenite::handshake::server::ErrorResponse::new(Some(r#"{"code":"H0002","msg":"invalid token"}"#.into()));
                        *e.status_mut() = tungstenite::http::StatusCode::UNAUTHORIZED;
                        return Err(e);
                    }
                    Ok(resp)
                });
                let Ok(mut ws) = ws else {
                    let _ = tx.send((seen.0, seen.1, 0));
                    continue;
                };
                let mut bytes = 0;
                loop {
                    match ws.read() {
                        Ok(Message::Binary(b)) => bytes += b.len(),
                        Ok(Message::Text(t)) if t.as_str() == "EOS" => break,
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
                // 중간 결과 하나 + 확정 결과들
                let _ = ws.send(Message::text(r#"{"seq":0,"final":false,"alternatives":[{"text":"중간"}]}"#));
                for (i, f) in finals.iter().enumerate() {
                    let _ = ws.send(Message::text(format!(r#"{{"seq":{i},"start_at":0,"duration":1000,"final":true,"alternatives":[{{"text":"{f}","confidence":0.9}}]}}"#)));
                }
                let _ = ws.close(None);
                let _ = ws.flush();
                // 클라이언트가 닫기 응답을 받을 때까지
                while ws.read().is_ok() {}
                let _ = tx.send((seen.0, seen.1, bytes));
            }
        });
        Mock { base, auths, streams: rx }
    }

    #[test]
    fn streams_one_utterance_and_joins_final_results() {
        let m = mock(false, &["스캔프리 결제를", "금요일까지 배포해요."]);
        let mut t = RtzrTranscriber::new(&m.base, "id-a", "secret", "").unwrap();
        let samples = vec![0.1f32; 16_000];
        let kw = vec!["스캔프리".to_string()];
        let req = AsrRequest { samples: &samples, language: "ko", prompt: "무시", keywords: &kw, context: "", preview: None, abort: None };
        assert_eq!(t.transcribe(&req).unwrap(), "스캔프리 결제를 금요일까지 배포해요.");
        let (query, auth, bytes) = m.streams.recv().unwrap();
        assert_eq!(auth, "Bearer tok1");
        assert_eq!(bytes, 32_000, "1초 = 16,000 샘플 × 2바이트");
        assert!(query.contains("model_name=sommers_ko") && query.contains("keywords="), "{query}");
        // 같은 키로 다시 받아 적으면 토큰을 다시 받지 않는다
        let mut t2 = RtzrTranscriber::new(&m.base, "id-a", "secret", "").unwrap();
        t2.transcribe(&req).unwrap();
        assert_eq!(m.auths.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn expired_token_is_renewed_once() {
        let m = mock(true, &["다시 받았어요."]);
        let mut t = RtzrTranscriber::new(&m.base, "id-b", "secret", "").unwrap();
        let samples = vec![0.0f32; 1600];
        let req = AsrRequest { samples: &samples, language: "ko", prompt: "", keywords: &[], context: "", preview: None, abort: None };
        assert_eq!(t.transcribe(&req).unwrap(), "다시 받았어요.");
        assert_eq!(m.auths.load(Ordering::SeqCst), 2, "401 이면 토큰을 새로 받는다");
        let (_, first_auth, _) = m.streams.recv().unwrap();
        let (_, second_auth, _) = m.streams.recv().unwrap();
        assert_eq!((first_auth.as_str(), second_auth.as_str()), ("Bearer tok1", "Bearer tok2"));
    }

    #[test]
    fn wrong_keys_are_reported_without_retrying() {
        // 인증이 401 이면 다시 보내지 않고, 원인을 우리말로
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                h.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let body = r#"{"code":"H0002","msg":"unauthorized"}"#;
                let _ = write!(s, "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
            }
        });
        let mut t = RtzrTranscriber::new(&base, "id-c", "wrong", "").unwrap();
        let samples = vec![0.0f32; 1600];
        let req = AsrRequest { samples: &samples, language: "ko", prompt: "", keywords: &[], context: "", preview: None, abort: None };
        let e = t.transcribe(&req).unwrap_err();
        assert!(!super::super::retryable(&e));
        let msg = e.to_string();
        assert!(msg.contains("401") && msg.contains("unauthorized (H0002)"), "{msg}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
