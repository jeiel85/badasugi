//! 사용자 설정 (settings.json) 과 API 키 (Windows 자격 증명 관리자)

use std::path::{Path, PathBuf};

use anyhow::Result;
use badasugi_core::audio::SourceKind;
use badasugi_core::speaker::DiarizationConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// "local" | "openai" | "groq" | "custom"
    pub engine: String,
    /// 로컬 모델 id (models::WHISPER_MODELS) 또는 모델 폴더의 파일 이름
    pub local_model: String,
    pub beam_size: u32,
    /// 0 = 자동
    pub threads: usize,
    pub openai_model: String,
    pub groq_model: String,
    pub custom_url: String,
    pub custom_model: String,

    pub source: SourceKind,
    pub mic_device: Option<String>,
    pub language: String,
    /// 용어집 (고유명사·전문용어) — 인식 정확도를 올린다
    pub glossary: String,
    pub diarization: DiarizationConfig,
    /// 회의 녹음(WAV)도 함께 저장
    pub save_audio: bool,
    /// 녹음이 끝나면 로컬 모델을 메모리에서 내림 (대기 중 가볍게)
    pub unload_when_idle: bool,

    pub hotkey: String,
    pub always_on_top: bool,
    /// "system" | "light" | "dark"
    pub theme: String,
    /// 새 버전을 백그라운드에서 받아 두었다가, 녹음 중이 아니고 창이 숨겨져 있을 때 조용히 설치
    pub auto_update: bool,
    /// 마지막으로 실행한 버전 (업데이트 직후 안내용)
    pub last_version: String,
    /// 미니 창(녹음 도크만, 항상 위) 모드
    pub mini_mode: bool,
    /// 미니 창 위치 (물리 픽셀)
    pub mini_pos: Option<[i32; 2]>,
    /// 미니 창이 작업 영역 오른쪽 아래(시계 위)에 붙어 있음 — 배율·해상도가 바뀌어도 그 자리로 다시 붙인다
    pub mini_docked: bool,
    /// 미니 창으로 바꾸기 전 큰 창 위치(물리 픽셀 x, y)·크기(논리 픽셀 너비, 높이)
    pub full_bounds: Option<[i32; 4]>,

    pub summary: SummarySettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SummarySettings {
    /// "ollama" | "groq" | "gemini" | "openai" | "anthropic" | "custom"
    pub provider: String,
    pub ollama_url: String,
    pub ollama_model: String,
    pub groq_model: String,
    pub gemini_model: String,
    pub openai_model: String,
    pub anthropic_model: String,
    pub custom_url: String,
    pub custom_model: String,
}

impl Default for SummarySettings {
    fn default() -> Self {
        Self {
            provider: "ollama".into(),
            ollama_url: "http://localhost:11434".into(),
            ollama_model: "exaone3.5:7.8b".into(),
            groq_model: "openai/gpt-oss-120b".into(),
            // 항상 최신 Flash 를 가리키는 별칭
            gemini_model: "gemini-flash-latest".into(),
            openai_model: "gpt-5-mini".into(),
            anthropic_model: "claude-opus-5-5".into(),
            custom_url: String::new(),
            custom_model: String::new(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            engine: "local".into(),
            local_model: "large-v3-turbo-q5_0".into(),
            beam_size: 1,
            threads: 0,
            openai_model: "gpt-4o-transcribe".into(),
            groq_model: "whisper-large-v3-turbo".into(),
            custom_url: "http://localhost:8000/v1".into(),
            custom_model: "whisper-1".into(),
            source: SourceKind::Mic,
            mic_device: None,
            language: "ko".into(),
            glossary: String::new(),
            diarization: DiarizationConfig::default(),
            save_audio: false,
            unload_when_idle: true,
            hotkey: "Ctrl+Alt+R".into(),
            always_on_top: false,
            theme: "system".into(),
            auto_update: true,
            last_version: String::new(),
            mini_mode: false,
            mini_pos: None,
            mini_docked: true,
            full_bounds: None,
            summary: SummarySettings::default(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self)?)?;
            f.sync_all()?;
        }
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// 로컬 모델 파일 경로
    pub fn local_model_path(&self, models_dir: &Path) -> PathBuf {
        match badasugi_core::models::find_whisper(&self.local_model) {
            Some(spec) => spec.path_in(models_dir),
            None => models_dir.join(&self.local_model),
        }
    }
}

const SERVICE: &str = "badasugi";

/// API 키 저장소 키 이름
pub const KEY_NAMES: &[&str] = &["openai", "groq", "custom", "anthropic", "gemini", "llm-custom"];

/// API 키 저장소 (Windows 자격 증명 관리자)
pub fn get_key(name: &str) -> Option<String> {
    keyring::Entry::new(SERVICE, name)
        .ok()?
        .get_password()
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn set_key(name: &str, value: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, name)?;
    if value.trim().is_empty() {
        let _ = entry.delete_credential();
    } else {
        entry.set_password(value.trim())?;
    }
    Ok(())
}
