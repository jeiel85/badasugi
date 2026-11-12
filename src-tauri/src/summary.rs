//! 회의록 요약 (로컬 LLM · API)
//!
//! - Ollama (로컬, 기본: LG EXAONE 3.5 — 한국어 특화)
//! - OpenAI (Chat Completions)
//! - Anthropic Claude (Messages API, raw HTTP)
//! - OpenAI 호환 사용자 지정 서버 (LM Studio, vLLM 등)

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::session::SessionDoc;
use crate::settings::{get_key, SummarySettings};

const SYSTEM: &str = "당신은 한국 회사의 회의록을 정리하는 비서입니다. \
녹취록은 음성 인식 결과라서 오탈자와 잘못 들린 단어가 있을 수 있으니 문맥으로 바로잡아 이해하세요. \
녹취록에 없는 내용은 지어내지 마세요. 결과는 한국어 마크다운으로, 아래 형식만 출력하세요.\n\n\
### 한 줄 요약\n(회의 전체를 한 문장으로)\n\n\
### 주요 논의\n- (핵심 논의를 3~7개 불릿으로, 누가 어떤 의견을 냈는지 화자 이름 포함)\n\n\
### 결정 사항\n- (확정된 것만. 없으면 \"없음\")\n\n\
### 할 일\n- [ ] (할 일) — 담당: (화자) / 기한: (언급된 경우)\n\n\
### 남은 질문\n- (결론이 나지 않았거나 확인이 필요한 것. 없으면 \"없음\")";

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
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    anyhow!("요약 API 오류 {}: {}", status.as_u16(), msg)
}

pub fn summarize(cfg: &SummarySettings, doc: &SessionDoc) -> Result<String> {
    if doc.utterances.is_empty() {
        bail!("요약할 내용이 없습니다");
    }
    let user = transcript_prompt(doc);
    match cfg.provider.as_str() {
        "ollama" => ollama(cfg, &user),
        "openai" => openai_compat(
            "https://api.openai.com/v1",
            get_key("openai").as_deref(),
            &cfg.openai_model,
            &user,
        ),
        "anthropic" => anthropic(cfg, &user),
        "custom" => openai_compat(&cfg.custom_url, get_key("llm-custom").as_deref(), &cfg.custom_model, &user),
        other => bail!("알 수 없는 요약 제공자: {other}"),
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
    let resp = client(600)?.post(&url).json(&body).send().map_err(|e| {
        anyhow!("Ollama 에 연결할 수 없습니다. Ollama 가 실행 중인지 확인해 주세요. ({e})")
    })?;
    let status = resp.status();
    let text = resp.text()?;
    if !status.is_success() {
        return Err(error_message(status, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    Ok(v["message"]["content"].as_str().unwrap_or_default().trim().to_string())
}

fn openai_compat(base: &str, key: Option<&str>, model: &str, user: &str) -> Result<String> {
    if base.trim().is_empty() {
        bail!("요약 API 주소를 입력해 주세요");
    }
    let url = format!("{}/chat/completions", base.trim().trim_end_matches('/'));
    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": SYSTEM },
            { "role": "user", "content": user }
        ]
    });
    let mut rb = client(300)?.post(&url).json(&body);
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

fn anthropic(cfg: &SummarySettings, user: &str) -> Result<String> {
    let key = get_key("anthropic").ok_or_else(|| anyhow!("Anthropic API 키를 설정에서 입력해 주세요"))?;
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
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2026-10-21")
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
    let out = out.join("").trim().to_string();
    if out.is_empty() {
        bail!("요약 결과가 비어 있습니다");
    }
    Ok(out)
}
