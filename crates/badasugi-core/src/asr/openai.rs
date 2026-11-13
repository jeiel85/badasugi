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
        let resp = rb.send()?;
        // 공급자가 알려 주는 남은 한도 (설정 › API 사용량)
        crate::usage::record_headers(&self.url, resp.headers());
        let status = resp.status();
        let body = resp.text()?;
        if !status.is_success() {
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| body.chars().take(300).collect());
            return Err(anyhow!("API 오류 {}: {}", status.as_u16(), msg));
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
        for attempt in 0..3 {
            match self.call(&wav, req) {
                Ok(t) => return Ok(t),
                Err(e) => {
                    let msg = e.to_string();
                    // 인증·요청 오류는 재시도해도 소용없다
                    let fatal = ["API 오류 400", "API 오류 401", "API 오류 403", "API 오류 404"]
                        .iter()
                        .any(|p| msg.starts_with(p));
                    last_err = Some(e);
                    if fatal {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(600 * (attempt + 1)));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("알 수 없는 API 오류")))
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
