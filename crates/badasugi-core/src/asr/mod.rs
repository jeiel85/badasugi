//! 음성 인식 엔진. 로컬(whisper.cpp) 과 OpenAI 호환 API(OpenAI · Groq · 자체 서버) 를 같은 인터페이스로 다룬다.

pub mod openai;
#[cfg(feature = "local-whisper")]
pub mod whisper;

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EngineConfig {
    /// 로컬 whisper.cpp (ggml 모델 파일)
    #[serde(rename_all = "camelCase")]
    Local {
        model_path: PathBuf,
        /// 0 = 자동
        #[serde(default)]
        threads: usize,
        /// 1 = greedy(빠름), 2 이상 = beam search(정확, 느림)
        #[serde(default = "one")]
        beam_size: u32,
    },
    /// OpenAI 호환 `/audio/transcriptions` 엔드포인트
    #[serde(rename_all = "camelCase")]
    Api {
        base_url: String,
        api_key: String,
        model: String,
    },
}

fn one() -> u32 {
    1
}

impl EngineConfig {
    /// 동시에 처리할 발화 수. 로컬은 CPU 를 다 쓰므로 1, API 는 네트워크 대기라 여러 개.
    pub fn concurrency(&self) -> usize {
        match self {
            EngineConfig::Local { .. } => 1,
            EngineConfig::Api { .. } => 3,
        }
    }

    pub fn label(&self) -> String {
        match self {
            EngineConfig::Local { model_path, .. } => format!(
                "로컬 · {}",
                model_path.file_stem().and_then(|s| s.to_str()).unwrap_or("whisper").trim_start_matches("ggml-")
            ),
            EngineConfig::Api { model, base_url, .. } => {
                let host = base_url
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .split('/')
                    .next()
                    .unwrap_or("");
                format!("API · {model} ({host})")
            }
        }
    }
}

pub struct AsrRequest<'a> {
    /// 16 kHz 모노
    pub samples: &'a [f32],
    /// "ko", "en", … 또는 "auto"
    pub language: &'a str,
    /// 용어집/문맥 프롬프트
    pub prompt: &'a str,
}

pub trait Transcriber: Send {
    fn transcribe(&mut self, req: &AsrRequest) -> Result<String>;
}

pub fn create(cfg: &EngineConfig) -> Result<Box<dyn Transcriber>> {
    match cfg {
        #[cfg(feature = "local-whisper")]
        EngineConfig::Local {
            model_path,
            threads,
            beam_size,
        } => Ok(Box::new(whisper::LocalWhisper::new(model_path, *threads, *beam_size)?)),
        #[cfg(not(feature = "local-whisper"))]
        EngineConfig::Local { .. } => anyhow::bail!("이 빌드에는 로컬 엔진이 포함되어 있지 않습니다"),
        EngineConfig::Api {
            base_url,
            api_key,
            model,
        } => Ok(Box::new(openai::ApiTranscriber::new(base_url, api_key, model)?)),
    }
}
