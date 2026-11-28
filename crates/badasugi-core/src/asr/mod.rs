//! 음성 인식 엔진. 로컬(whisper.cpp), OpenAI 호환 API(OpenAI · Groq · 자체 서버),
//! 전용 형식 API(리턴제로 · CLOVA Speech · ElevenLabs) 를 같은 인터페이스로 다룬다.

pub mod clova;
pub mod elevenlabs;
pub mod gpu;
pub mod openai;
pub mod rtzr;
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
    /// 요청 형식이 저마다 다른 음성 인식 API (리턴제로 · CLOVA Speech · ElevenLabs)
    #[serde(rename_all = "camelCase")]
    Service {
        service: Service,
        /// API 주소 — 비우면 서비스 기본 주소. CLOVA Speech 는 도메인마다 받는 Invoke URL
        #[serde(default)]
        base_url: String,
        /// 리턴제로 Client ID (다른 서비스는 비움)
        #[serde(default)]
        key_id: String,
        /// 리턴제로 Client Secret · CLOVA Speech Secret Key · ElevenLabs API 키
        secret: String,
        /// 리턴제로 sommers · ElevenLabs scribe_v… (CLOVA Speech 는 비움)
        #[serde(default)]
        model: String,
    },
}

/// 전용 형식 음성 인식 API
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    /// 리턴제로 (RTZR · Sommers)
    Rtzr,
    /// 네이버 클라우드 CLOVA Speech
    Clova,
    /// ElevenLabs Scribe
    Elevenlabs,
}

impl Service {
    /// 설정의 엔진 값이자 사용량 공급자 이름
    pub fn id(self) -> &'static str {
        match self {
            Service::Rtzr => "rtzr",
            Service::Clova => "clova",
            Service::Elevenlabs => "elevenlabs",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Service::Rtzr => "리턴제로",
            Service::Clova => "CLOVA Speech",
            Service::Elevenlabs => "ElevenLabs",
        }
    }
}

fn one() -> u32 {
    1
}

impl EngineConfig {
    /// 동시에 처리할 발화 수. 로컬은 CPU 를 다 쓰므로 1, API 는 네트워크 대기라 여러 개.
    pub fn concurrency(&self) -> usize {
        match self {
            EngineConfig::Local { .. } => 1,
            EngineConfig::Api { .. } | EngineConfig::Service { .. } => 3,
        }
    }

    /// API 사용량을 셀 이름 (공급자, 모델) — 로컬 엔진은 없음.
    /// ElevenLabs 는 용어(keyterms)를 보내면 단가가 달라 모델 이름에 `+keyterms` 를 붙인다.
    pub fn usage(&self, keywords: &[String]) -> Option<(&'static str, String)> {
        match self {
            EngineConfig::Local { .. } => None,
            EngineConfig::Api { base_url, model, .. } => Some((crate::usage::provider_of(base_url), model.trim().to_string())),
            EngineConfig::Service { service, model, .. } => {
                let mut m = service_model(*service, model);
                if *service == Service::Elevenlabs && !elevenlabs::keyterms(keywords).is_empty() {
                    m.push_str("+keyterms");
                }
                Some((service.id(), m))
            }
        }
    }

    /// 바로 앞에서 받아 적은 말을 앞 문맥으로 넘길지 — OpenAI 의 GPT 기반 인식 모델(gpt-transcribe 등)만.
    /// OpenAI 는 긴 녹음을 나눠 보낼 때 앞 조각의 녹취를 프롬프트로 넘기라고 권한다.
    /// Whisper(로컬 · Groq · whisper-1)는 넘기지 않는다: 앞 문맥의 버릇까지 그대로 따라 해서 (11-19 회의 녹음 비교,
    /// examples/asrcompare) 로컬은 문장부호가 아예 사라졌고, Groq 는 토막마다 마침표를 찍거나 영문 약어를 소문자로 바꿨으며,
    /// 앞 문맥의 엉뚱한 말을 이어 붙이기도 했다.
    pub fn wants_context(&self) -> bool {
        matches!(self, EngineConfig::Api { model, .. } if openai::is_gpt_transcriber(model))
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
            EngineConfig::Service { service, model, .. } => match model.trim() {
                "" => format!("API · {}", service.name()),
                m => format!("API · {} {m}", service.name()),
            },
        }
    }
}

/// 사용량 · 요금에 쓸 모델 이름 — 모델을 고르지 않는 서비스는 서비스 이름 그대로
fn service_model(service: Service, model: &str) -> String {
    match (service, model.trim()) {
        (Service::Clova, _) => clova::MODEL.to_string(),
        (Service::Rtzr, "") => rtzr::DEFAULT_MODEL.to_string(),
        (Service::Elevenlabs, "") => elevenlabs::DEFAULT_MODEL.to_string(),
        (_, m) => m.to_string(),
    }
}

pub struct AsrRequest<'a> {
    /// 16 kHz 모노
    pub samples: &'a [f32],
    /// "ko", "en", … 또는 "auto"
    pub language: &'a str,
    /// 용어집 프롬프트 ("회의 녹취록입니다. 주요 용어: …") — API 엔진용. 로컬 엔진은 `keywords` 가 있으면
    /// 이것 대신 낱말로 따로 만든다 (`whisper::LOCAL_PROMPT_TOKENS`)
    pub prompt: &'a str,
    /// 용어집 낱말 — gpt-transcribe 는 따로 보내고, 로컬 엔진은 이것으로 프롬프트를 만든다
    pub keywords: &'a [String],
    /// 바로 앞에서 받아 적은 말 (같은 회의) — 문장부호 · 띄어쓰기 · 표기를 앞 문장과 맞추게 한다.
    /// 앞 문맥을 받는 엔진만 채운다 ([`EngineConfig::wants_context`]), 아니면 비어 있다.
    pub context: &'a str,
    /// 말하는 동안 미리 보기 — 있으면 빠르게 한 번만 풀고(로컬: greedy · 다시 풀기 없음),
    /// 이 함수가 true 를 돌려주면(확정 인식할 발화가 기다리면) 로컬 엔진은 하던 인식을 그만둔다.
    pub preview: Option<&'a AbortCheck<'a>>,
    /// 하던 인식을 그만둘지 (녹음 뒤 다시 받아 적기 — 녹음 · 말로 입력이 시작되면 CPU · GPU 를 바로 내준다).
    /// 미리 보기와 달리 평소처럼 받아 적고, true 가 되면 로컬 엔진은 그만두고 오류를 돌려준다 (API 는 보지 않는다)
    pub abort: Option<&'a AbortCheck<'a>>,
}

/// 미리 보기 인식을 그만둘지 (계산 중에 여러 번 부른다)
pub type AbortCheck<'a> = dyn Fn() -> bool + Sync + 'a;

impl AsrRequest<'_> {
    /// 모델에 보낼 프롬프트: 용어집 문장 + 앞 문맥. 인식 모델은 프롬프트를 '바로 앞 녹취' 로 보고 이어 쓰므로
    /// 앞 문맥을 끝에 둔다. 합쳐서 [`PROMPT_MAX_BYTES`] 를 넘지 않게 용어집은 뒤쪽 낱말부터,
    /// 앞 문맥은 앞쪽 낱말부터 덜어 낸다 (용어집을 먼저 지킨다).
    pub fn full_prompt(&self) -> String {
        let p = fit_front(self.prompt.trim(), PROMPT_MAX_BYTES);
        let room = if p.is_empty() { PROMPT_MAX_BYTES } else { PROMPT_MAX_BYTES.saturating_sub(p.len() + 1) };
        match (p, fit_back(self.context.trim(), room)) {
            (p, "") => p,
            (p, c) if p.is_empty() => c.to_string(),
            (p, c) => format!("{p} {c}"),
        }
    }
}

/// 프롬프트 최대 길이 (UTF-8 바이트). Groq 는 896 바이트를 넘는 프롬프트를 거절해(400) 그 발화를 받아 적지 못한다
/// (2026-11-19 확인: "prompt length must be 896 characters or fewer" — 한글 한 글자를 3으로 센다).
/// OpenAI whisper-1 은 224 토큰을 넘으면 앞쪽을 버린다.
pub const PROMPT_MAX_BYTES: usize = 880;

/// 앞에서부터 `max` 바이트 안쪽 — 넘치면 쉼표 · 띄어쓰기 자리에서 끊고 마침표로 맺는다 (용어집 문장)
fn fit_front(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub(1);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let head = &s[..end];
    let cut = head.rfind([',', ' ']).unwrap_or(0);
    let head = head[..cut].trim_end_matches([',', ' ']);
    if head.is_empty() {
        String::new()
    } else {
        format!("{head}.")
    }
}

/// 끝에서부터 `max` 바이트 안쪽 — 넘치면 앞쪽 낱말을 뺀다 (앞 문맥)
fn fit_back(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    let tail = &s[start..];
    let mid_word = !s[..start].ends_with(char::is_whitespace);
    match (mid_word, tail.find(char::is_whitespace)) {
        (true, Some(sp)) => tail[sp..].trim_start(),
        (true, None) => "",
        _ => tail.trim_start(),
    }
}

/// 앞 문맥으로 넘기는 최대 글자 수 — 한두 문장. 11-19 회의 녹음 비교(Groq whisper-large-v3)에서
/// 40자는 문체가 거의 달라지지 않았고 80자부터 앞 문장을 따라갔다.
pub const CONTEXT_CHARS: usize = 80;

/// 같은 회의에서 받아 적은 최근 문장 — 다음 발화를 보낼 때 앞 문맥으로 쓴다.
/// API 는 발화 여러 개를 함께 보내므로, 발화 번호가 더 앞선 것 중 끝난 것만 쓴다.
pub struct RecentText {
    done: std::collections::BTreeMap<u64, String>,
    max_chars: usize,
}

impl Default for RecentText {
    fn default() -> Self {
        Self::with_max_chars(CONTEXT_CHARS)
    }
}

impl RecentText {
    /// 들고 있는 문장 수 (동시에 보내는 발화보다 넉넉히)
    const KEEP: usize = 8;

    /// 앞 문맥을 최대 `max_chars` 글자까지 (0 이면 넘기지 않음)
    pub fn with_max_chars(max_chars: usize) -> Self {
        Self {
            done: Default::default(),
            max_chars,
        }
    }

    pub fn record(&mut self, id: u64, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.done.insert(id, text.to_string());
        while self.done.len() > Self::KEEP {
            self.done.pop_first();
        }
    }

    /// `id` 보다 앞선 문장들의 끝부분 (최대 글자 수까지, 낱말 중간에서 자르지 않는다)
    pub fn before(&self, id: u64) -> String {
        if self.max_chars == 0 {
            return String::new();
        }
        let joined = self.done.range(..id).map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" ");
        tail_chars(&joined, self.max_chars)
    }
}

/// 끝에서 `max` 글자 이내 — 잘린 자리가 낱말 중간이면 그 낱말은 뺀다
fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let start = s.char_indices().nth(n - max).map_or(0, |(i, _)| i);
    let cut = &s[start..];
    let mid_word = !s[..start].ends_with(char::is_whitespace);
    match (mid_word, cut.find(char::is_whitespace)) {
        (true, Some(sp)) => cut[sp..].trim_start().to_string(),
        _ => cut.trim_start().to_string(),
    }
}

pub trait Transcriber: Send {
    fn transcribe(&mut self, req: &AsrRequest) -> Result<String>;
}

/// API 인식 오류가 나중에 다시 보내 볼 만한지 — 연결 문제 · 요청 한도 · 서비스 장애는 그렇고,
/// 키 · 모델 이름 · 요청 형식 오류는 아니다. 응답 해석 실패 같은 그 밖의 오류도 한 번 더 해 볼 만하다.
pub fn retryable(e: &anyhow::Error) -> bool {
    e.downcast_ref::<openai::ApiError>().is_none_or(|a| !a.is_fatal())
}

/// 로컬 엔진이 마지막으로 모델을 연 장치 ("GPU · …" / "CPU") — 아직 열지 않았거나 로컬 엔진이 없는 빌드면 없음
pub fn local_device() -> Option<String> {
    #[cfg(feature = "local-whisper")]
    {
        whisper::device()
    }
    #[cfg(not(feature = "local-whisper"))]
    {
        None
    }
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
        EngineConfig::Service {
            service,
            base_url,
            key_id,
            secret,
            model,
        } => Ok(match service {
            Service::Rtzr => Box::new(rtzr::RtzrTranscriber::new(base_url, key_id, secret, model)?),
            Service::Clova => Box::new(clova::ClovaTranscriber::new(base_url, secret)?),
            Service::Elevenlabs => Box::new(elevenlabs::ElevenLabsTranscriber::new(base_url, secret, model)?),
        }),
    }
}

/// 연결 확인 (설정 › 녹음의 연결 테스트): 1초 무음을 한 번 보내 키 · 주소 · 모델이 맞는지 본다 — 다시 보내지 않는다.
/// 로컬 엔진은 확인할 것이 없어 바로 Ok.
pub fn test_connection(cfg: &EngineConfig) -> Result<()> {
    match cfg {
        EngineConfig::Local { .. } => Ok(()),
        EngineConfig::Api {
            base_url,
            api_key,
            model,
        } => openai::test_connection(base_url, api_key, model),
        EngineConfig::Service {
            service,
            base_url,
            key_id,
            secret,
            model,
        } => {
            let silence = vec![0.0f32; crate::SAMPLE_RATE as usize];
            let req = AsrRequest {
                samples: &silence,
                language: "ko",
                prompt: "",
                keywords: &[],
                context: "",
                preview: None,
                abort: None,
            };
            // 다시 보내지 않고 한 번만 (기다리는 시간은 확정 문장과 같다)
            match service {
                Service::Rtzr => rtzr::RtzrTranscriber::new(base_url, key_id, secret, model)?.once(&req),
                Service::Clova => clova::ClovaTranscriber::new(base_url, secret)?.once(&req),
                Service::Elevenlabs => elevenlabs::ElevenLabsTranscriber::new(base_url, secret, model)?.once(&req),
            }
            .map(|_| ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_uses_only_earlier_utterances_in_order() {
        let mut r = RecentText::default();
        r.record(2, "둘째 문장이에요.");
        r.record(0, "첫 문장이에요.");
        r.record(5, "다섯째는 아직 뒤예요.");
        r.record(3, "  ");
        assert_eq!(r.before(0), "");
        assert_eq!(r.before(3), "첫 문장이에요. 둘째 문장이에요.");
    }

    #[test]
    fn context_keeps_the_tail_without_cutting_a_word() {
        let mut r = RecentText::default();
        let long = "가나다라마바사 ".repeat(20);
        r.record(0, &long);
        r.record(1, "그래서 결제는 됐어요.");
        let c = r.before(2);
        assert!(c.chars().count() <= CONTEXT_CHARS, "{c}");
        assert!(c.ends_with("그래서 결제는 됐어요."));
        assert!(c.starts_with("가나다라마바사"), "낱말 중간에서 자르지 않는다: {c}");
    }

    #[test]
    fn keeps_only_a_few_recent_sentences() {
        let mut r = RecentText::default();
        for i in 0..20 {
            r.record(i, &format!("문장{i}"));
        }
        assert_eq!(r.done.len(), RecentText::KEEP);
        assert!(r.before(20).ends_with("문장19"));
    }

    #[test]
    fn full_prompt_puts_context_after_glossary() {
        let mut req = AsrRequest {
            samples: &[],
            language: "ko",
            prompt: "회의 녹취록입니다. 주요 용어: OKPOS.",
            keywords: &[],
            context: "네, 좋아요.",
            preview: None,
            abort: None,
        };
        assert_eq!(req.full_prompt(), "회의 녹취록입니다. 주요 용어: OKPOS. 네, 좋아요.");
        req.prompt = "";
        assert_eq!(req.full_prompt(), "네, 좋아요.");
        req.context = "";
        assert_eq!(req.full_prompt(), "");
    }

    #[test]
    fn long_glossary_and_context_stay_under_the_groq_limit() {
        // Groq 가 400 으로 거절했던 길이 (1152 바이트)
        let terms: Vec<String> = (1..=70).map(|i| format!("용어항목{i}")).collect();
        let prompt = format!("회의 녹취록입니다. 주요 용어: {}.", terms.join(", "));
        let context = "그래서 결제 모듈은 금요일까지 배포하기로 했고요, 온보딩 시안은 다음 주에 다시 보기로 했어요.";
        let req = AsrRequest {
            samples: &[],
            language: "ko",
            prompt: &prompt,
            keywords: &[],
            context,
            preview: None,
            abort: None,
        };
        let full = req.full_prompt();
        assert!(full.len() <= PROMPT_MAX_BYTES, "{}", full.len());
        assert!(full.starts_with("회의 녹취록입니다. 주요 용어: 용어항목1, 용어항목2,"));
        // 넘치는 용어는 뒤에서부터 빠지고, 앞 문맥은 남는 자리만큼만
        assert!(!full.contains("용어항목70"));
        assert!(full.trim_end().ends_with('.'), "{full}");

        // 용어집이 짧으면 앞 문맥이 다 들어간다
        let req = AsrRequest { prompt: "회의 녹취록입니다. 주요 용어: OKPOS.", ..req };
        assert_eq!(req.full_prompt(), format!("회의 녹취록입니다. 주요 용어: OKPOS. {context}"));
    }

    #[test]
    fn only_gpt_transcribers_get_the_previous_context() {
        let api = |model: &str| EngineConfig::Api {
            base_url: String::new(),
            api_key: String::new(),
            model: model.into(),
        };
        for m in ["gpt-transcribe", "GPT-4o-transcribe", "gpt-4o-mini-transcribe"] {
            assert!(api(m).wants_context(), "{m}");
        }
        for m in ["whisper-large-v3", "whisper-large-v3-turbo", "whisper-1", "gpt-5-mini"] {
            assert!(!api(m).wants_context(), "{m}");
        }
        let local = EngineConfig::Local {
            model_path: "ggml-large-v3-turbo-q5_0.bin".into(),
            threads: 0,
            beam_size: 1,
        };
        assert!(!local.wants_context());
    }

    #[test]
    fn services_count_usage_under_their_own_name() {
        let svc = |service, model: &str| EngineConfig::Service {
            service,
            base_url: String::new(),
            key_id: String::new(),
            secret: "k".into(),
            model: model.into(),
        };
        assert_eq!(svc(Service::Rtzr, "").usage(&[]), Some(("rtzr", "sommers".to_string())));
        assert_eq!(svc(Service::Clova, "").usage(&[]), Some(("clova", clova::MODEL.to_string())));
        // ElevenLabs 는 용어를 보내면 단가가 달라 따로 센다 (보낼 용어가 없으면 그대로)
        let kw = vec!["OKPOS".to_string()];
        assert_eq!(svc(Service::Elevenlabs, "").usage(&kw), Some(("elevenlabs", "scribe_v3+keyterms".to_string())));
        let empty = vec!["  ".to_string()];
        assert_eq!(svc(Service::Elevenlabs, "scribe_v2").usage(&empty), Some(("elevenlabs", "scribe_v2".to_string())));
        assert_eq!(svc(Service::Clova, "").label(), "API · CLOVA Speech");
        assert_eq!(svc(Service::Elevenlabs, "scribe_v3").label(), "API · ElevenLabs scribe_v3");
        assert!(!svc(Service::Rtzr, "").wants_context(), "앞 문맥은 OpenAI GPT 인식 모델만");
        assert_eq!(svc(Service::Rtzr, "").concurrency(), 3);
        let json = serde_json::to_value(svc(Service::Elevenlabs, "scribe_v3")).unwrap();
        assert_eq!((&json["kind"], &json["service"]), (&serde_json::json!("service"), &serde_json::json!("elevenlabs")));
        let local = EngineConfig::Local {
            model_path: "m.bin".into(),
            threads: 0,
            beam_size: 1,
        };
        assert_eq!(local.usage(&kw), None);
    }

    #[test]
    fn fit_back_drops_whole_words_from_the_front() {
        assert_eq!(fit_back("가나 다라 마바", 100), "가나 다라 마바");
        // 끝 8바이트: "라 마바" 중 '라' 는 낱말 중간이라 뺀다
        assert_eq!(fit_back("가나 다라 마바", 8), "마바");
        assert_eq!(fit_back("가나다라마바", 6), "");
        assert_eq!(fit_front("가나, 다라, 마바.", 13), "가나.");
    }
}
