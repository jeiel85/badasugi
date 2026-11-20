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
    /// 말하는 동안 미리 보기: "auto"(로컬 엔진만) | "on"(API 도 — 요청이 늘어 비용이 오른다) | "off"
    pub live_preview: String,
    /// GPU 가속 (GPU 빌드에서 로컬 엔진) — GPU 가 없거나 열지 못하면 저절로 CPU
    pub gpu: bool,

    pub hotkey: String,
    /// 중요 표시 ⭐ 전역 단축키 — 녹음 · 다시 듣기 중 지금 말하는 곳을 메모에 `[시각] ⭐` 로 (빈 값이면 없음)
    pub star_hotkey: String,
    pub always_on_top: bool,
    /// 회의 시작 알림 — 회의 앱이 마이크를 쓰기 시작하면 받아쓸지 묻는다 (녹음을 저절로 시작하지는 않음)
    pub meeting_alert: bool,
    /// 회의 시작 알림이 지켜볼 앱 (meeting::MATCHERS 의 id)
    pub meeting_apps: Vec<String>,
    /// 일정에서 회의 제목 · 참석자 가져오기 — 켜져 있는 클래식 Outlook 의 일정을 읽는다 (calendar.rs)
    pub calendar: bool,
    /// 녹음 폴더 지켜보기 — 폴더에 새로 생긴 녹음 파일을 알아서 받아 적는다 (watch.rs)
    pub watch_folder: bool,
    /// 지켜볼 폴더 (휴대폰 녹음 · 녹음기 파일이 쌓이는 OneDrive 폴더 등)
    pub watch_dir: String,
    /// 받아 적은 뒤 요약까지 (설정 › 요약의 AI 로)
    pub watch_summarize: bool,
    /// 받아쓰기 입력 모드 — 단축키를 누른 채 말하면 지금 커서가 있는 창에 글자가 들어간다 (dictate.rs)
    pub dictation: bool,
    /// 말로 입력 단축키 (누른 채 말하기 · 톡 누르면 말이 끝날 때까지)
    pub dictation_hotkey: String,
    /// "paste" = 클립보드 붙여넣기(원래 클립보드는 되돌림) · "type" = 한 글자씩
    pub dictation_insert: String,
    /// AI 로 다듬기 (설정 › 요약의 AI — 맞춤법 · 띄어쓰기)
    pub dictation_polish: bool,
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
    /// 번역 (요약에 쓰는 AI 로)
    pub translate: TranslateSettings,
    /// 사용량 금액을 원화로 보여 줄 때 쓰는 환율 (1달러 = ? 원)
    pub usd_krw: f64,
}

/// 번역 — 요약 · 묻기에 쓰는 AI 제공자로 (translate.rs)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TranslateSettings {
    /// 번역할 언어 (translate::LANGS 의 코드)
    pub lang: String,
    /// 녹음 중에 확정된 문장을 바로 번역 (API 면 몇 문장마다 요청이 간다 — 기본 꺼짐)
    pub live: bool,
}

impl Default for TranslateSettings {
    fn default() -> Self {
        Self { lang: "ko".into(), live: false }
    }
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
    /// 기본 요약 틀 (templates.rs 의 기본 틀 id 또는 내 틀 id) — 회의마다 마지막으로 쓴 틀이 있으면 그것
    pub template: String,
    /// 내 틀 (설정 › 요약에서 만든 것)
    pub templates: Vec<CustomTemplate>,
}

/// 내 요약 틀 — 섹션은 한 줄에 하나("섹션 이름" 또는 "섹션 이름 — 무엇을 적을지")
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CustomTemplate {
    pub id: String,
    pub name: String,
    pub sections: String,
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
            template: crate::templates::DEFAULT.into(),
            templates: Vec::new(),
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
            live_preview: "auto".into(),
            gpu: true,
            hotkey: "Ctrl+Alt+R".into(),
            star_hotkey: "Ctrl+Alt+B".into(),
            always_on_top: false,
            meeting_alert: false,
            meeting_apps: crate::meeting::default_apps(),
            calendar: false,
            watch_folder: false,
            watch_dir: String::new(),
            watch_summarize: true,
            dictation: false,
            dictation_hotkey: "Ctrl+Alt+Space".into(),
            dictation_insert: "paste".into(),
            dictation_polish: false,
            theme: "system".into(),
            auto_update: true,
            last_version: String::new(),
            mini_mode: false,
            mini_pos: None,
            mini_docked: true,
            full_bounds: None,
            summary: SummarySettings::default(),
            translate: TranslateSettings::default(),
            usd_krw: DEFAULT_USD_KRW,
        }
    }
}

/// 두 단축키가 같은 조합인지 (대소문자 · 빈칸은 무시, 빈 값은 겹치지 않는다)
pub fn same_hotkey(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && a == b
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
        // 녹음 단축키를 이미 Ctrl+Alt+B 로 바꿔 쓰던 사람은 새 기본 중요 표시 단축키와 겹친다 →
        // 녹음 단축키를 지키고 중요 표시는 비운다 (설정 › 일반에서 다른 조합으로 정한다)
        if same_hotkey(&self.hotkey, &self.star_hotkey) {
            self.star_hotkey.clear();
        }
        // 말로 입력 단축키가 다른 단축키와 겹치면 말로 입력 쪽을 비운다 (설정 › 일반에서 다시 정한다)
        if same_hotkey(&self.dictation_hotkey, &self.hotkey) || same_hotkey(&self.dictation_hotkey, &self.star_hotkey) {
            self.dictation_hotkey.clear();
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

    /// 실시간 녹음에서 말하는 동안 미리 보기를 켤지 — 자동이면 로컬 엔진만
    /// (로컬은 확정 인식이 쉴 때만 돌리고 밀리면 저절로 끈다 · API 는 요청 수만큼 비용이 늘어 직접 켤 때만)
    pub fn wants_preview(&self) -> bool {
        match self.live_preview.as_str() {
            "on" => true,
            "off" => false,
            _ => self.engine == "local",
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
    fn dictation_is_off_and_does_not_steal_a_hotkey() {
        let s: Settings = serde_json::from_str(r#"{ "hotkey": "Ctrl+Alt+R" }"#).unwrap();
        assert!(!s.dictation);
        assert_eq!((s.dictation_hotkey.as_str(), s.dictation_insert.as_str()), ("Ctrl+Alt+Space", "paste"));
        assert!(!s.dictation_polish);
        // 녹음 단축키를 이미 Ctrl+Alt+Space 로 쓰던 설정: 녹음 단축키를 지키고 말로 입력은 비운다
        let dir = std::env::temp_dir().join(format!("bd-settings-dict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, br#"{ "hotkey": "ctrl+alt+space" }"#).unwrap();
        let s = Settings::load(&path);
        assert_eq!((s.hotkey.as_str(), s.dictation_hotkey.as_str()), ("ctrl+alt+space", ""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_watch_is_off_until_chosen() {
        // 녹음 폴더 지켜보기가 생기기 전의 설정 파일: 꺼져 있고, 켜면 요약까지
        let s: Settings = serde_json::from_str(r#"{ "hotkey": "Ctrl+Alt+R" }"#).unwrap();
        assert!(!s.watch_folder);
        assert!(s.watch_dir.is_empty());
        assert!(s.watch_summarize);
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
    fn live_preview_is_on_by_default_only_for_the_local_engine() {
        // 미리 보기가 생기기 전의 설정 파일도 자동으로 읽힌다
        let s: Settings = serde_json::from_str(r#"{ "engine": "local" }"#).unwrap();
        assert_eq!(s.live_preview, "auto");
        assert!(s.wants_preview());
        // GPU 가속은 기본 켬 (없으면 저절로 CPU)
        assert!(s.gpu);
        for engine in ["openai", "groq", "custom"] {
            let mut s = Settings { engine: engine.into(), ..Settings::default() };
            assert!(!s.wants_preview(), "API 는 비용이 들어 기본 꺼짐: {engine}");
            s.live_preview = "on".into();
            assert!(s.wants_preview(), "{engine}");
        }
        let s = Settings { live_preview: "off".into(), ..Settings::default() };
        assert!(!s.wants_preview());
    }

    #[test]
    fn old_settings_get_the_star_hotkey() {
        // 중요 표시 단축키가 생기기 전의 설정 파일도 기본 단축키로 읽힌다 (녹음 단축키는 그대로)
        let s: Settings = serde_json::from_str(r#"{ "hotkey": "Ctrl+Shift+F9" }"#).unwrap();
        assert_eq!((s.hotkey.as_str(), s.star_hotkey.as_str()), ("Ctrl+Shift+F9", "Ctrl+Alt+B"));
        // 녹음 단축키를 이미 Ctrl+Alt+B 로 쓰던 설정: 녹음 단축키를 지키고 중요 표시는 비운다
        let dir = std::env::temp_dir().join(format!("bd-settings-star-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, br#"{ "hotkey": "ctrl+alt+b" }"#).unwrap();
        let s = Settings::load(&path);
        assert_eq!((s.hotkey.as_str(), s.star_hotkey.as_str()), ("ctrl+alt+b", ""));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(same_hotkey("Ctrl+Alt+B", " ctrl + alt + b "));
        assert!(!same_hotkey("", ""));
        assert!(!same_hotkey("Ctrl+Alt+R", "Ctrl+Alt+B"));
    }
}
