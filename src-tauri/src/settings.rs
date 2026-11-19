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
    /// 이름을 붙인 화자의 목소리를 기억해 두었다가 다음 회의에서 같은 목소리에 그 이름을 붙인다
    /// (목소리 특징값만 이 PC 의 voices.json 에 · 녹음은 저장하지 않음)
    pub remember_voices: bool,
    /// 회의 녹음도 함께 저장 (Ogg Opus, 1시간 약 11 MB)
    pub save_audio: bool,
    /// 녹음이 끝나면 로컬 모델을 메모리에서 내림 (대기 중 가볍게)
    pub unload_when_idle: bool,

    pub hotkey: String,
    /// 중요 표시 ⭐ 전역 단축키 — 녹음 · 다시 듣기 중 지금 말하는 곳을 메모에 `[시각] ⭐` 로 (빈 값이면 없음)
    pub star_hotkey: String,
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
    /// 사용량 금액을 원화로 보여 줄 때 쓰는 환율 (1달러 = ? 원)
    pub usd_krw: f64,
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

/// 기본 환율 — 2026년 11월 초 원/달러 (설정 › 사용량에서 바꿀 수 있다)
pub const DEFAULT_USD_KRW: f64 = 1360.0;

impl Default for Settings {
    fn default() -> Self {
        Self {
            engine: "local".into(),
            local_model: "large-v3-turbo-q5_0".into(),
            beam_size: 1,
            threads: 0,
            openai_model: "gpt-transcribe".into(),
            groq_model: "whisper-large-v3-turbo".into(),
            custom_url: "http://localhost:8000/v1".into(),
            custom_model: "whisper-1".into(),
            source: SourceKind::Mic,
            mic_device: None,
            language: "ko".into(),
            glossary: String::new(),
            diarization: DiarizationConfig::default(),
            remember_voices: true,
            save_audio: false,
            unload_when_idle: true,
            hotkey: "Ctrl+Alt+R".into(),
            star_hotkey: "Ctrl+Alt+B".into(),
            always_on_top: false,
            theme: "system".into(),
            auto_update: true,
            last_version: String::new(),
            mini_mode: false,
            mini_pos: None,
            mini_docked: true,
            full_bounds: None,
            summary: SummarySettings::default(),
            usd_krw: DEFAULT_USD_KRW,
        }
    }
}

/// JSON 파일을 읽는다. 있는데 해석하지 못하면(손으로 고침 · 이전 버전으로 되돌림 · 쓰다 끊김) 기본값으로 시작하되,
/// 원래 파일은 `<이름>.json.bad` 로 남겨 둔다 — 곧 이어질 저장이 원래 내용을 덮어써 없애지 않게
pub fn load_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    let Ok(bytes) = std::fs::read(path) else {
        return T::default();
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            let bad = path.with_extension("json.bad");
            let kept = std::fs::write(&bad, &bytes).is_ok();
            log::warn!(
                "{} 을(를) 읽지 못해 기본값으로 시작합니다 ({e}){}",
                path.display(),
                if kept { format!(" — 원래 파일은 {} 에 남겨 둠", bad.display()) } else { String::new() }
            );
            T::default()
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        let mut s: Settings = load_json_or_default(path);
        s.migrate();
        s
    }

    /// 예전 기본값을 새 기본값으로. OpenAI 는 gpt-4o-transcribe 를 2027-02-26 에 끝내고
    /// 더 정확하고 25% 싼 gpt-transcribe 로 바꾸라고 한다 (2026-07 출시).
    fn migrate(&mut self) {
        if self.openai_model.trim() == "gpt-4o-transcribe" {
            self.openai_model = "gpt-transcribe".into();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_settings_are_kept_aside_before_defaults() {
        let dir = std::env::temp_dir().join(format!("bd-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, b"{ \"hotkey\": \"Ctrl+Alt+R\", oops").unwrap();
        let s = Settings::load(&path);
        assert_eq!(s.hotkey, Settings::default().hotkey);
        let bad = std::fs::read(dir.join("settings.json.bad")).unwrap();
        assert_eq!(bad, b"{ \"hotkey\": \"Ctrl+Alt+R\", oops");
        // 없는 파일은 조용히 기본값
        let none: Settings = load_json_or_default(&dir.join("none.json"));
        assert_eq!(none.hotkey, Settings::default().hotkey);
        assert!(!dir.join("none.json.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_openai_default_moves_to_gpt_transcribe() {
        let dir = std::env::temp_dir().join(format!("bd-settings-mig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, br#"{ "openaiModel": "gpt-4o-transcribe" }"#).unwrap();
        assert_eq!(Settings::load(&path).openai_model, "gpt-transcribe");
        // 일부러 고른 다른 모델은 그대로
        std::fs::write(&path, br#"{ "openaiModel": "gpt-4o-mini-transcribe" }"#).unwrap();
        assert_eq!(Settings::load(&path).openai_model, "gpt-4o-mini-transcribe");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_settings_get_the_star_hotkey() {
        // 중요 표시 단축키가 생기기 전의 설정 파일도 기본 단축키로 읽힌다 (녹음 단축키는 그대로)
        let s: Settings = serde_json::from_str(r#"{ "hotkey": "Ctrl+Shift+F9" }"#).unwrap();
        assert_eq!((s.hotkey.as_str(), s.star_hotkey.as_str()), ("Ctrl+Shift+F9", "Ctrl+Alt+B"));
    }
}
