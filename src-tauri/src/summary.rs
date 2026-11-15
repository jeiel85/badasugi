//! 회의록 요약 (로컬 LLM · API)
//!
//! - Ollama (로컬, 기본: LG EXAONE 3.5 — 한국어 특화. 앱에 내장하지 않고 Ollama 를 따로 설치해 쓴다)
//! - Groq (OpenAI 호환 · 무료 등급 있음 · 음성 인식 Groq 키를 같이 씀)
//! - Google Gemini (OpenAI 호환 엔드포인트 · 무료 등급 있음)
//! - OpenAI (Chat Completions · 음성 인식 OpenAI 키를 같이 씀)
//! - Anthropic Claude (Messages API, raw HTTP)
//! - OpenAI 호환 사용자 지정 서버 (LM Studio, vLLM, OpenRouter 등)
//!
//! 긴 회의는 한 번에 보내면 로컬 모델 문맥(앞부분이 잘림)이나 무료 등급 분당 토큰 한도(거절)에 걸리므로,
//! 제공자별 크기([`chunk_budget`])를 넘으면 녹취록을 구간으로 나눠 메모한 뒤 합친다.

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

/// 긴 회의: 구간 하나를 나중에 합칠 메모로 정리하라는 지시
const PART_SYSTEM: &str = "당신은 한국 회사의 회의록을 정리하는 비서입니다. \
회의가 길어서 녹취록을 여러 구간으로 나눠 받고 있습니다. 받은 구간의 내용을 나중에 회의 전체 회의록으로 합칠 수 있게 빠짐없이 메모하세요. \
녹취록은 음성 인식 결과라서 오탈자와 잘못 들린 단어가 있을 수 있으니 문맥으로 바로잡아 이해하세요. \
녹취록에 없는 내용은 지어내지 마세요. 숫자 · 날짜 · 사람 이름 · 제품 이름은 그대로 옮기세요. \
결과는 한국어 마크다운으로, 아래 형식만 출력하세요.\n\n\
### 논의\n- (누가 어떤 의견을 냈는지 화자 이름 포함)\n\n\
### 결정 사항\n- (확정된 것만. 없으면 \"없음\")\n\n\
### 할 일\n- [ ] (할 일) — 담당: (화자) / 기한: (언급된 경우)\n\n\
### 남은 질문\n- (없으면 \"없음\")";

/// 한 번에 보낼 녹취록 글자 수 (제공자별). 넘으면 구간으로 나눠 메모한 뒤 합친다
fn chunk_budget(provider: &str) -> usize {
    match provider {
        // 무료 등급의 분당 토큰 한도(TPM)가 작아, 한 요청이 그보다 크면 바로 거절된다
        "groq" => 4_000,
        "gemini" => 150_000,
        "openai" | "anthropic" => 40_000,
        // Ollama(문맥 32K 토큰) · 사용자 지정 서버 — 넘치면 앞부분이 말없이 잘린다
        _ => 12_000,
    }
}

/// 이보다 오래 기다리라는 한도 초과(하루 한도 등)는 기다리지 않고 알린다
const MAX_WAIT_SECS: u64 = 75;
/// 너무 크다고 거절돼도 이보다 잘게는 나누지 않는다
const MIN_CHUNK: usize = 600;

/// 요약 진행 (긴 회의를 나눠 정리할 때 화면에 보여 줌)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// 끝난 요청 수
    pub done: usize,
    /// 전체 요청 수 (구간 수 + 합치기)
    pub total: usize,
    /// 요청 한도 때문에 기다리는 중이면 그 시간(초)
    pub wait_secs: u64,
}

/// 요청 하나의 결과
#[derive(Debug, Default)]
pub struct Reply {
    pub text: String,
    pub in_tokens: u64,
    pub out_tokens: u64,
}

/// 요약 API 오류 — 상태 코드와 다시 시도까지 기다릴 시간을 들고 다닌다 (나눠 보내기 · 기다리기 판단용)
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub retry_after: Option<u64>,
    pub msg: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hint = match self.status {
            401 | 403 => " (API 키를 확인해 주세요)",
            404 => " (모델 이름이나 주소를 확인해 주세요)",
            429 => " (요청 한도를 넘었어요. 잠시 뒤 다시 시도해 주세요)",
            _ => "",
        };
        write!(f, "요약 API 오류 {}: {}{}", self.status, self.msg, hint)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// 요청이 너무 커서 거절됨 (문맥 · 분당 토큰 한도) — 더 잘게 나눠 보내면 된다
    fn too_large(&self) -> bool {
        let m = self.msg.to_lowercase();
        self.status == 413
            || ["too large", "too long", "context length", "context_length", "maximum context", "reduce the length", "context window"]
                .iter()
                .any(|k| m.contains(k))
    }
}

fn too_large(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>().is_some_and(ApiError::too_large)
}

/// 한도 초과(429)라 기다렸다 다시 보내면 되는 경우 그 시간(초)
fn wait_for(e: &anyhow::Error) -> Option<u64> {
    let a = e.downcast_ref::<ApiError>()?;
    (a.status == 429 && !a.too_large()).then(|| a.retry_after.unwrap_or(20))
}

/// 녹취록 줄을 끊지 않고 비슷한 크기의 구간들로 (각 구간 ≤ budget 글자)
fn split_lines(text: &str, budget: usize) -> Vec<String> {
    let budget = budget.max(1);
    let total = text.chars().count();
    // 끝 구간만 짧게 남지 않게: k 구간의 경계(전체의 1/k, 2/k …)를 지나면 자른다
    let k = total.div_ceil(budget).max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut n = 0;
    let mut pos = 0;
    for line in text.lines() {
        let len = line.chars().count() + 1;
        if !cur.is_empty() && (n + len > budget || pos >= total * (out.len() + 1) / k) {
            out.push(std::mem::take(&mut cur));
            n = 0;
        }
        pos += len;
        if len > budget {
            // 한 사람이 아주 길게 말한 문단 — 글자 수로 자른다
            let chars: Vec<char> = line.chars().collect();
            for piece in chars.chunks(budget) {
                out.push(piece.iter().collect::<String>() + "\n");
            }
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
        n += len;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// 구간의 시간 범위 ("[12:30] 김 팀장: …" 줄들에서)
fn span(part: &str) -> (String, String) {
    let time = |l: &str| {
        l.strip_prefix('[')
            .and_then(|r| r.split_once(']'))
            .map(|(t, _)| t.to_string())
            .unwrap_or_default()
    };
    let mut lines = part.lines().filter(|l| l.starts_with('['));
    let from = lines.next().map(time).unwrap_or_default();
    let to = part.lines().rev().find(|l| l.starts_with('[')).map(time).unwrap_or_default();
    (from, to)
}

/// 구간 메모 하나
struct Note {
    from: String,
    to: String,
    text: String,
}

impl Note {
    fn render(&self, label: &str) -> String {
        if self.from.is_empty() {
            format!("## {label}\n{}", self.text.trim())
        } else {
            format!("## {label} ({}~{})\n{}", self.from, self.to, self.text.trim())
        }
    }
}

fn render_notes(notes: &[Note]) -> String {
    notes
        .iter()
        .enumerate()
        .map(|(i, n)| n.render(&format!("구간 {}", i + 1)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 요청 보내기 · 사용량 · 진행 알림 (긴 회의 나눠 정리용)
struct Run<'a> {
    call: &'a mut dyn FnMut(&str, &str) -> Result<Reply>,
    progress: &'a dyn Fn(Progress),
    done: usize,
    total: usize,
    requests: u64,
    in_tokens: u64,
    out_tokens: u64,
}

impl Run<'_> {
    fn report(&self, wait_secs: u64) {
        (self.progress)(Progress { done: self.done, total: self.total, wait_secs });
    }

    /// 한도 초과(429)면 알려 준 시간만큼 기다렸다 다시 (몇 번까지만)
    fn ask(&mut self, system: &str, user: &str) -> Result<String> {
        let mut tries = 0;
        loop {
            match (self.call)(system, user) {
                Ok(r) => {
                    self.requests += 1;
                    self.in_tokens += r.in_tokens;
                    self.out_tokens += r.out_tokens;
                    return Ok(r.text);
                }
                Err(e) => match wait_for(&e) {
                    Some(w) if w <= MAX_WAIT_SECS && tries < 4 => {
                        tries += 1;
                        log::info!("요약 요청 한도 — {w}초 기다렸다 다시");
                        self.report(w);
                        std::thread::sleep(Duration::from_secs(w + 1));
                        self.report(0);
                    }
                    _ => return Err(e),
                },
            }
        }
    }
}

/// 녹취록(`text`)을 요약한다. 한 번에 보내기엔 길면(`budget` 글자 초과) 구간별로 메모한 뒤 합친다.
/// `head` 는 회의 제목 · 일시 줄
fn run_summary(
    call: &mut dyn FnMut(&str, &str) -> Result<Reply>,
    progress: &dyn Fn(Progress),
    head: &str,
    text: &str,
    mut budget: usize,
) -> Result<(String, u64, u64, u64)> {
    let mut r = Run { call, progress, done: 0, total: 1, requests: 0, in_tokens: 0, out_tokens: 0 };
    let len = text.chars().count();
    if len <= budget {
        match r.ask(SYSTEM, &format!("{head}녹취록:\n{text}")) {
            Ok(out) => return Ok((out, r.requests, r.in_tokens, r.out_tokens)),
            // 생각보다 컸다 (토큰이 글자보다 많은 모델 등) — 나눠서
            Err(e) if too_large(&e) && len > MIN_CHUNK * 2 => budget = len / 2 + 1,
            Err(e) => return Err(e),
        }
    }

    // ── 긴 회의: 구간마다 메모 ──
    let mut parts: std::collections::VecDeque<String> = split_lines(text, budget).into();
    log::info!("긴 회의 요약: {}글자를 {}구간으로", len, parts.len());
    r.total = parts.len() + 1;
    r.report(0);
    let mut notes: Vec<Note> = Vec::new();
    while let Some(part) = parts.pop_front() {
        let (from, to) = span(&part);
        let user = format!(
            "{head}구간: {}/{} ({from}~{to})\n\n녹취록:\n{part}",
            notes.len() + 1,
            notes.len() + 1 + parts.len()
        );
        match r.ask(PART_SYSTEM, &user) {
            Ok(text) => {
                notes.push(Note { from, to, text });
                r.done += 1;
                r.report(0);
            }
            Err(e) if too_large(&e) && part.chars().count() > MIN_CHUNK => {
                let halves = split_lines(&part, part.chars().count() / 2 + 1);
                log::info!("요약 구간이 너무 커서 {}조각으로 다시 나눔", halves.len());
                r.total += halves.len().saturating_sub(1);
                for h in halves.into_iter().rev() {
                    parts.push_front(h);
                }
                r.report(0);
            }
            Err(e) => return Err(e),
        }
    }

    // ── 메모 합치기: 아직도 크면 몇 개씩 묶어 한 번 더 줄인다 ──
    for round in 0.. {
        let joined = render_notes(&notes);
        if joined.chars().count() <= budget || notes.len() == 1 || round >= 3 {
            let user = format!(
                "{head}이 회의는 길어서 녹취록을 구간별로 먼저 메모했습니다. 아래 구간 메모를 모두 합쳐 회의 전체 회의록을 만드세요. \
                 같은 내용은 하나로 합치고, 결정 사항과 할 일은 빠뜨리지 마세요.\n\n{joined}"
            );
            match r.ask(SYSTEM, &user) {
                Ok(out) => {
                    r.done = r.total;
                    r.report(0);
                    return Ok((out, r.requests, r.in_tokens, r.out_tokens));
                }
                Err(e) if too_large(&e) && notes.len() > 1 && round < 3 => budget = budget * 2 / 3,
                Err(e) => return Err(e),
            }
        }
        // 묶음 수: 크기로 정하되 매번 적어도 절반으로 준다
        let size = render_notes(&notes).chars().count();
        let groups = size.div_ceil(budget.max(1)).max(2).min(notes.len() / 2).max(1);
        let per = notes.len().div_ceil(groups);
        r.total += groups;
        r.report(0);
        let mut merged = Vec::new();
        let old = std::mem::take(&mut notes);
        for g in old.chunks(per) {
            let user = format!(
                "{head}다음은 이 회의 여러 구간의 메모입니다. 하나의 메모로 합치세요. 같은 내용은 하나로 합치고, 결정 사항과 할 일은 빠뜨리지 마세요.\n\n{}",
                render_notes(g)
            );
            let text = r.ask(PART_SYSTEM, &user)?;
            merged.push(Note { from: g[0].from.clone(), to: g[g.len() - 1].to.clone(), text });
            r.done += 1;
            r.report(0);
        }
        notes = merged;
    }
    unreachable!()
}

/// 요약 결과: 본문 + (모델이 제안한) 회의 제목 + 사용량 (설정 › API 사용량)
pub struct Summary {
    pub text: String,
    pub title: Option<String>,
    /// "groq" · "gemini" · "openai" · "anthropic" · "custom" · "ollama"
    pub provider: String,
    pub in_tokens: u64,
    pub out_tokens: u64,
    /// 보낸 요청 수 (긴 회의는 구간 수 + 합치기)
    pub requests: u64,
}

fn client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("badasugi/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

fn retry_after(h: &reqwest::header::HeaderMap) -> Option<u64> {
    let secs = h.get("retry-after")?.to_str().ok()?.trim().parse::<f64>().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| secs.ceil() as u64)
}

fn error_message(status: reqwest::StatusCode, retry_after: Option<u64>, body: &str) -> anyhow::Error {
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
    anyhow::Error::new(ApiError { status: status.as_u16(), retry_after, msg })
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

type Caller = Box<dyn FnMut(&str, &str) -> Result<Reply>>;

/// 회의록을 요약한다. 긴 회의는 구간으로 나눠 정리하며 `progress` 로 진행을 알린다
pub fn summarize(cfg: &SummarySettings, doc: &SessionDoc, progress: &dyn Fn(Progress)) -> Result<Summary> {
    if doc.utterances.is_empty() {
        bail!("요약할 내용이 없습니다");
    }
    let (provider, mut call): (String, Caller) = match cfg.provider.as_str() {
        "ollama" => {
            let c = cfg.clone();
            ("ollama".into(), Box::new(move |s: &str, u: &str| ollama(&c, s, u)))
        }
        "anthropic" => {
            let c = cfg.clone();
            ("anthropic".into(), Box::new(move |s: &str, u: &str| anthropic(&c, s, u)))
        }
        _ => match openai_like(cfg)? {
            Some((base, key, model)) => (
                badasugi_core::usage::provider_of(&base).to_string(),
                Box::new(move |s: &str, u: &str| openai_compat(&base, key.as_deref(), &model, s, u)),
            ),
            None => bail!("알 수 없는 요약 제공자: {}", cfg.provider),
        },
    };
    let head = format!("회의 제목: {}\n일시: {}\n\n", doc.title, doc.started_at);
    let (raw, requests, in_tokens, out_tokens) =
        run_summary(&mut *call, progress, &head, &doc.to_text(), chunk_budget(&provider))?;
    if raw.trim().is_empty() {
        bail!("요약 결과가 비어 있습니다");
    }
    let mut out = split_title(&raw);
    out.provider = provider;
    out.in_tokens = in_tokens;
    out.out_tokens = out_tokens;
    out.requests = requests;
    Ok(out)
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
        provider: String::new(),
        in_tokens: 0,
        out_tokens: 0,
        requests: 0,
    }
}

fn ollama(cfg: &SummarySettings, system: &str, user: &str) -> Result<Reply> {
    let url = format!("{}/api/chat", cfg.ollama_url.trim().trim_end_matches('/'));
    let body = json!({
        "model": cfg.ollama_model,
        "stream": false,
        // 기본 컨텍스트(2~4K)로는 긴 회의가 잘리므로 넉넉히
        "options": { "num_ctx": 32768, "temperature": 0.2 },
        "messages": [
            { "role": "system", "content": system },
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
        return Err(error_message(status, None, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    Ok(Reply {
        text: v["message"]["content"].as_str().unwrap_or_default().trim().to_string(),
        in_tokens: v["prompt_eval_count"].as_u64().unwrap_or(0),
        out_tokens: v["eval_count"].as_u64().unwrap_or(0),
    })
}

fn openai_compat(base: &str, key: Option<&str>, model: &str, system: &str, user: &str) -> Result<Reply> {
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
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
    });
    let mut rb = client(600)?.post(&url).json(&body);
    if let Some(k) = key {
        rb = rb.bearer_auth(k);
    }
    let resp = rb.send()?;
    badasugi_core::usage::record_headers(&url, resp.headers());
    let status = resp.status();
    let wait = retry_after(resp.headers());
    let text = resp.text()?;
    if !status.is_success() {
        return Err(error_message(status, wait, &text));
    }
    let v: Value = serde_json::from_str(&text)?;
    if v["choices"][0]["finish_reason"].as_str() == Some("content_filter") {
        bail!("모델이 요약을 거절했습니다 (콘텐츠 필터)");
    }
    let usage = &v["usage"];
    Ok(Reply {
        text: v["choices"][0]["message"]["content"].as_str().unwrap_or_default().trim().to_string(),
        in_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
        out_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
    })
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

fn anthropic(cfg: &SummarySettings, system: &str, user: &str) -> Result<Reply> {
    let key = anthropic_key()?;
    let model = if cfg.anthropic_model.trim().is_empty() {
        "claude-opus-5-5"
    } else {
        cfg.anthropic_model.trim()
    };
    let mut body = json!({
        "model": model,
        "max_tokens": 16000,
        "system": system,
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
    badasugi_core::usage::record_headers(ANTHROPIC_URL, resp.headers());
    let status = resp.status();
    let wait = retry_after(resp.headers());
    let text = resp.text()?;
    if !status.is_success() {
        return Err(error_message(status, wait, &text));
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
    let usage = &v["usage"];
    Ok(Reply {
        text: out.join("").trim().to_string(),
        in_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
        out_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
    })
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
        return Err(error_message(status, None, &text));
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

    /// "[mm:ss] 화자: …" 줄을 n 개 (각 줄 약 `width` 글자)
    fn transcript(n: usize, width: usize) -> String {
        (0..n)
            .map(|i| format!("[{:02}:{:02}] 김 팀장: {}\n", i / 60, i % 60, "가".repeat(width)))
            .collect()
    }

    fn too_big() -> anyhow::Error {
        anyhow::Error::new(ApiError {
            status: 413,
            retry_after: None,
            msg: "Request too large for model on tokens per minute (TPM)".into(),
        })
    }

    #[test]
    fn split_keeps_lines_and_balances() {
        let t = transcript(100, 50);
        let parts = split_lines(&t, 1_000);
        assert!(parts.iter().all(|p| p.chars().count() <= 1_000));
        assert_eq!(parts.concat(), t, "줄을 빠뜨리거나 끊으면 안 됨");
        let sizes: Vec<usize> = parts.iter().map(|p| p.chars().count()).collect();
        let (min, max) = (sizes.iter().min().unwrap(), sizes.iter().max().unwrap());
        assert!(max - min < 200, "고르게 나눔: {sizes:?}");
        // 한 문단이 budget 보다 길면 글자 수로 자른다
        let long = split_lines(&"나".repeat(2_500), 1_000);
        assert_eq!(long.len(), 3);
        assert_eq!(span("[01:02] a: x\n[03:04] b: y\n"), ("01:02".into(), "03:04".into()));
    }

    #[test]
    fn short_meeting_is_one_request() {
        let mut sent = Vec::new();
        let mut call = |sys: &str, user: &str| {
            sent.push((sys == SYSTEM, user.to_string()));
            Ok(Reply { text: "요약".into(), in_tokens: 10, out_tokens: 5 })
        };
        let (out, req, i, o) = run_summary(&mut call, &|_| {}, "회의 제목: t\n\n", &transcript(10, 20), 12_000).unwrap();
        assert_eq!((out.as_str(), req, i, o), ("요약", 1, 10, 5));
        assert!(sent[0].0 && sent[0].1.contains("녹취록:"));
    }

    #[test]
    fn long_meeting_is_split_then_merged() {
        let t = transcript(200, 60); // 약 1.5만 글자
        let mut calls = Vec::new();
        let mut call = |sys: &str, user: &str| {
            calls.push((sys == PART_SYSTEM, user.to_string()));
            Ok(Reply { text: format!("메모{}", calls.len()), in_tokens: 1, out_tokens: 1 })
        };
        let seen = std::cell::RefCell::new(Vec::new());
        let (out, req, _, _) =
            run_summary(&mut call, &|p| seen.borrow_mut().push(p), "회의 제목: t\n\n", &t, 4_000).unwrap();
        let parts = calls.iter().filter(|c| c.0).count();
        assert!(parts >= 4, "구간으로 나눔: {parts}");
        assert_eq!(req as usize, parts + 1, "구간 + 합치기");
        let last = calls.last().unwrap();
        assert!(!last.0, "마지막은 최종 회의록 형식");
        assert!(last.1.contains("## 구간 1 (00:00~") && last.1.contains(&format!("메모{parts}")));
        assert_eq!(out, format!("메모{}", parts + 1));
        let seen = seen.borrow();
        assert_eq!(seen.last().map(|p| (p.done, p.total)), Some((parts + 1, parts + 1)));
    }

    #[test]
    fn too_large_part_is_halved_and_retried() {
        let t = transcript(60, 60);
        let mut n = 0;
        let mut call = |sys: &str, user: &str| {
            n += 1;
            // 처음 한 번 보낸 요청(전체)과 큰 구간은 거절
            if sys == SYSTEM && !user.contains("구간 메모") || user.chars().count() > 1_500 {
                return Err(too_big());
            }
            Ok(Reply { text: "ok".into(), ..Default::default() })
        };
        let (out, req, _, _) = run_summary(&mut call, &|_| {}, "", &t, 5_000).unwrap();
        assert_eq!(out, "ok");
        assert!(req >= 5, "한 번에 → 두 구간 → 다시 반씩, 끝까지: {req}회");
    }

    #[test]
    fn rate_limit_waits_then_retries_but_not_for_daily_limits() {
        let mut n = 0;
        let mut call = |_: &str, _: &str| {
            n += 1;
            if n == 1 {
                return Err(anyhow::Error::new(ApiError { status: 429, retry_after: Some(0), msg: "rate limit".into() }));
            }
            Ok(Reply { text: "ok".into(), ..Default::default() })
        };
        let (out, req, ..) = run_summary(&mut call, &|_| {}, "", "[00:00] a: b\n", 100).unwrap();
        assert_eq!((out.as_str(), req), ("ok", 1), "한도 초과 뒤 다시 보내 성공");
        let mut call = |_: &str, _: &str| -> Result<Reply> {
            Err(anyhow::Error::new(ApiError { status: 429, retry_after: Some(3600), msg: "daily limit".into() }))
        };
        let e = run_summary(&mut call, &|_| {}, "", "[00:00] a: b\n", 100).unwrap_err();
        assert!(e.to_string().contains("429"), "하루 한도는 기다리지 않고 알림: {e}");
    }
}
