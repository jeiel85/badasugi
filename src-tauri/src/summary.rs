//! 회의록 요약 (로컬 LLM · API)
//!
//! - Ollama (로컬, 기본: LG EXAONE 3.5 — 한국어 특화. 앱에 내장하지 않고 Ollama 를 따로 설치해 쓴다)
//! - Groq (OpenAI 호환 · 무료 등급 있음 · 음성 인식 Groq 키를 같이 씀)
//! - Google Gemini (OpenAI 호환 엔드포인트 · 무료 등급 있음)
//! - OpenAI (Chat Completions · 음성 인식 OpenAI 키를 같이 씀)
//! - Anthropic Claude (Messages API, raw HTTP)
//! - OpenAI 호환 사용자 지정 서버 (LM Studio, vLLM, OpenRouter 등)

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::session::SessionDoc;
use crate::settings::{get_key, SummarySettings};

const SYSTEM: &str = "당신은 한국 회사의 회의록을 정리하는 비서입니다. \
녹취록은 음성 인식 결과라서 오탈자와 잘못 들린 단어가 있을 수 있으니 문맥으로 바로잡아 이해하세요. \
녹취록에 없는 내용은 지어내지 마세요. 결과는 한국어 마크다운으로, 아래 형식만 출력하세요.\n\n\
### 제목\n(회의 주제를 20자 이내 명사형으로)\n\n\
### 한 줄 요약\n(회의 전체를 한 문장으로)\n\n\
### 주요 논의\n- (핵심 논의를 3~7개 불릿으로, 누가 어떤 의견을 냈는지 화자 이름 포함)\n\n\
### 결정 사항\n- (확정된 것만. 없으면 \"없음\")\n\n\
### 할 일\n- [ ] (할 일) — 담당: (화자) / 기한: (언급된 경우)\n\n\
### 남은 질문\n- (결론이 나지 않았거나 확인이 필요한 것. 없으면 \"없음\")";

const GROQ_URL: &str = "https://api.groq.com/openai/v1";
const GEMINI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
const OPENAI_URL: &str = "https://api.openai.com/v1";
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION: &str = "2026-10-21";

/// 요약 결과: 본문 + (모델이 제안한) 회의 제목
pub struct Summary {
    pub text: String,
    pub title: Option<String>,
}

fn transcript_prompt(doc: &SessionDoc) -> String {
    format!(
        "회의 제목: {}\n일시: {}\n\n녹취록:\n{}",
        doc.title,
        doc.started_at,
        doc.to_text()
    )
}

fn client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("badasugi/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

fn error_message(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            // OpenAI·Anthropic: {"error":{"message"}}, Gemini: [{"error":{"message"}}], Ollama: {"error":"..."}
            let v = if v.is_array() { v[0].clone() } else { v };
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    let hint = match status.as_u16() {
        401 | 403 => " (API 키를 확인해 주세요)",
        404 => " (모델 이름이나 주소를 확인해 주세요)",
        429 => " (요청 한도를 넘었어요. 잠시 뒤 다시 시도해 주세요)",
        _ => "",
    };
    anyhow!("요약 API 오류 {}: {}{}", status.as_u16(), msg, hint)
}

/// 제공자 → (OpenAI 호환 주소, 키, 모델). Ollama·Claude 는 None
fn openai_like(cfg: &SummarySettings) -> Result<Option<(String, Option<String>, String)>> {
    let need = |name: &str, label: &str| {
        get_key(name).ok_or_else(|| anyhow!("{label} API 키를 설정 › AI 요약에서 입력해 주세요"))
    };
    Ok(match cfg.provider.as_str() {
        "groq" => Some((GROQ_URL.into(), Some(need("groq", "Groq")?), cfg.groq_model.clone())),
        "gemini" => Some((GEMINI_URL.into(), Some(need("gemini", "Gemini")?), cfg.gemini_model.clone())),
        "openai" => Some((OPENAI_URL.into(), Some(need("openai", "OpenAI")?), cfg.openai_model.clone())),
        "custom" => Some((cfg.custom_url.clone(), get_key("llm-custom"), cfg.custom_model.clone())),
        _ => None,
    })
}

pub fn summarize(cfg: &SummarySettings, doc: &SessionDoc) -> Result<Summary> {
    if doc.utterances.is_empty() {
        bail!("요약할 내용이 없습니다");
    }
    let user = transcript_prompt(doc);
    let raw = match cfg.provider.as_str() {
        "ollama" => ollama(cfg, &user)?,
        "anthropic" => anthropic(cfg, &user)?,
        _ => match openai_like(cfg)? {
            Some((base, key, model)) => openai_compat(&base, key.as_deref(), &model, &user)?,
            None => bail!("알 수 없는 요약 제공자: {}", cfg.provider),
        },
    };
    if raw.trim().is_empty() {
        bail!("요약 결과가 비어 있습니다");
    }
    Ok(split_title(&raw))
}

/// 맨 앞 "### 제목" 섹션을 떼어 제목으로 쓴다
fn split_title(raw: &str) -> Summary {
    let text = raw.trim();
    // 모델이 ```markdown 으로 감싸 보내는 경우
    let text = text
        .strip_prefix("```markdown")
        .or_else(|| text.strip_prefix("```md"))
        .or_else(|| text.strip_prefix("```"))
        .map(|t| t.trim_end().trim_end_matches("```").trim())
        .unwrap_or(text);
    let mut lines = text.lines().peekable();
    let mut title = None;
    let mut rest = Vec::new();
    while let Some(line) = lines.next() {
        let h = line.trim_start_matches('#').trim();
        if title.is_none() && line.trim_start().starts_with('#') && h == "제목" {
            // 다음 비어 있지 않은 줄이 제목
            for l in lines.by_ref() {
                let t = l.trim();
                if t.is_empty() {
                    continue;
                }
                if t.starts_with('#') {
                    rest.push(l);
                    break;
                }
                let t = t.trim_start_matches(['-', '*', ' ']).trim_matches(['"', '\'', '*', '「', '」']).trim();
                if !t.is_empty() {
                    title = Some(t.chars().take(40).collect::<String>());
                }
                break;
            }
            continue;
        }
        rest.push(line);
    }
    Summary {
        text: rest.join("\n").trim().to_string(),
        title,
    }
}

fn ollama(cfg: &SummarySettings, user: &str) -> Result<String> {
    let url = format!("{}/api/chat", cfg.ollama_url.trim().trim_end_matches('/'));
    let body = json!({
        "model": cfg.ollama_model,
        "stream": false,
        // 기본 컨텍스트(2~4K)로는 긴 회의가 잘리므로 넉넉히
        "options": { "num_ctx": 32768, "temperature": 0.2 },
        "messages": [
            { "role": "system", "content": SYSTEM },
            { "role": "user", "content": user }
        ]
    });
    let resp = client(900)?.post(&url).json(&body).send().map_err(|e| {
        anyhow!(
            "Ollama 에 연결할 수 없어요. Ollama 를 설치·실행했는지 확인하거나, 설정에서 Groq·Gemini 같은 API 를 골라 주세요. ({e})"
        )
    })?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        if status.as_u16() == 404 && text.contains("not found") {
            bail!(
                "Ollama 에 '{}' 모델이 없어요. 명령 프롬프트에서 `ollama pull {}` 로 먼저 받아 주세요.",
                cfg.ollama_model,
                cfg.ollama_model
            );
        }
        return Err(error_message(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    Ok(v["message"]["content"].as_str().unwrap_or_default().trim().to_string())
}

fn openai_compat(base: &str, key: Option<&str>, model: &str, user: &str) -> Result<String> {
    if base.trim().is_empty() {
        bail!("요약 API 주소를 입력해 주세요");
    }
    if model.trim().is_empty() {
        bail!("요약 모델 이름을 입력해 주세요");
    }
    let url = format!("{}/chat/completions", base.trim().trim_end_matches('/'));
    let body = json!({
        "model": model.trim(),
        "messages": [
            { "role": "system", "content": SYSTEM },
            { "role": "user", "content": user }
        ]
    });
    let mut rb = client(600)?.post(&url).json(&body);
    if let Some(k) = key {
        rb = rb.bearer_auth(k);
    }
    let resp = rb.send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        return Err(error_message(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    if v["choices"][0]["finish_reason"].as_str() == Some("content_filter") {
        bail!("모델이 요약을 거절했습니다 (콘텐츠 필터)");
    }
    Ok(v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string())
}

/// 서버측 거절 폴백을 지원하는 모델
fn supports_fallbacks(model: &str) -> bool {
    matches!(
        model,
        "claude-fable-5-2" | "claude-fable-5-1" | "claude-opus-5-5" | "claude-opus-5" | "claude-sonnet-5-5"
    )
}

fn anthropic_key() -> Result<String> {
    get_key("anthropic").ok_or_else(|| anyhow!("Anthropic API 키를 설정 › AI 요약에서 입력해 주세요"))
}

fn anthropic(cfg: &SummarySettings, user: &str) -> Result<String> {
    let key = anthropic_key()?;
    let model = if cfg.anthropic_model.trim().is_empty() {
        "claude-opus-5-5"
    } else {
        cfg.anthropic_model.trim()
    };
    let mut body = json!({
        "model": model,
        "max_tokens": 16000,
        "system": SYSTEM,
        "messages": [ { "role": "user", "content": user } ],
        "output_config": { "effort": "medium" }
    });
    let mut rb = client(600)?
        .post(format!("{ANTHROPIC_URL}/messages"))
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json");
    if supports_fallbacks(model) {
        // 안전 분류기가 거절하면 서버가 알아서 다른 모델로 다시 시도
        body["fallbacks"] = json!("default");
        rb = rb.header("anthropic-beta", "server-side-fallback-2026-07-01");
    }
    let resp = rb.json(&body).send()?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        return Err(error_message(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    if v["stop_reason"].as_str() == Some("refusal") {
        let why = v["stop_details"]["explanation"].as_str().unwrap_or("");
        bail!("모델이 요약을 거절했습니다. {why}");
    }
    let out: Vec<&str> = v["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"].as_str() == Some("text"))
                .filter_map(|b| b["text"].as_str())
                .collect()
        })
        .unwrap_or_default();
    Ok(out.join("").trim().to_string())
}

/// 연결 확인 겸 사용할 수 있는 모델 목록 (설정 화면 자동 완성용)
pub fn list_models(cfg: &SummarySettings) -> Result<Vec<String>> {
    let c = client(20)?;
    let (resp, ollama) = match cfg.provider.as_str() {
        "ollama" => {
            let url = format!("{}/api/tags", cfg.ollama_url.trim().trim_end_matches('/'));
            let r = c.get(&url).send().map_err(|_| {
                anyhow!("Ollama 에 연결할 수 없어요. https://ollama.com 에서 설치한 뒤 실행해 주세요.")
            })?;
            (r, true)
        }
        "anthropic" => (
            c.get(format!("{ANTHROPIC_URL}/models?limit=100"))
                .header("x-api-key", anthropic_key()?)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .send()?,
            false,
        ),
        _ => {
            let (base, key, _) = openai_like(cfg)?.ok_or_else(|| anyhow!("알 수 없는 요약 제공자"))?;
            if base.trim().is_empty() {
                bail!("요약 API 주소를 입력해 주세요");
            }
            let mut rb = c.get(format!("{}/models", base.trim().trim_end_matches('/')));
            if let Some(k) = key {
                rb = rb.bearer_auth(k);
            }
            (rb.send()?, false)
        }
    };
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        return Err(error_message(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    let mut names: Vec<String> = if ollama {
        v["models"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["name"].as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    } else {
        v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str())
                    // Gemini 는 "models/gemini-…" 로 준다
                    .map(|id| id.trim_start_matches("models/").to_string())
                    // 음성·이미지·임베딩 모델은 요약에 못 쓴다
                    .filter(|id| {
                        let l = id.to_lowercase();
                        !["whisper", "tts", "embed", "image", "dall-e", "moderation", "guard", "audio", "transcribe", "realtime", "veo", "imagen"]
                            .iter()
                            .any(|x| l.contains(x))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    names.sort();
    names.dedup();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_split_out() {
        let s = split_title("### 제목\n출시 일정 회의\n\n### 한 줄 요약\n15일 출시.");
        assert_eq!(s.title.as_deref(), Some("출시 일정 회의"));
        assert!(s.text.starts_with("### 한 줄 요약"));
        let s = split_title("```markdown\n### 제목\n- \"주간 회의\"\n### 한 줄 요약\n끝\n```");
        assert_eq!(s.title.as_deref(), Some("주간 회의"));
        assert_eq!(s.text, "### 한 줄 요약\n끝");
        let s = split_title("### 한 줄 요약\n제목 없음");
        assert!(s.title.is_none());
    }
}
