//! 회의록 요약 (로컬 LLM · API)
//!
//! - Ollama (로컬, 기본: LG EXAONE 3.5 — 한국어 특화. 앱에 내장하지 않고 Ollama 를 따로 설치해 쓴다)
//! - Groq (OpenAI 호환 · 무료 등급 있음 · 음성 인식 Groq 키를 같이 씀)
//! - Google Gemini (OpenAI 호환 엔드포인트 · 무료 등급 있음)
//! - OpenAI (Chat Completions · 음성 인식 OpenAI 키를 같이 씀)
//! - Anthropic Claude (Messages API, raw HTTP · API 키 또는 Anthropic CLI `ant auth login` 로그인)
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

/// 요약 진행 (화면의 진행 막대 · 문구)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    /// 끝난 요청 수
    pub done: usize,
    /// 전체 요청 수 (구간 수 + 합치기)
    pub total: usize,
    /// 요청 한도 때문에 기다리는 중이면 그 시간(초)
    pub wait_secs: u64,
    /// 지금 요청의 답을 쓴 만큼 (0~1) — 모델이 회의록 형식의 어느 섹션을 쓰는지로 어림한다
    pub frac: f32,
    /// 모델이 답을 쓰기 시작했는지 (아니면 아직 녹취록을 읽는 중)
    pub writing: bool,
    /// 지금 쓰는 섹션 이름 (모르면 "")
    pub section: &'static str,
}

/// 쓰는 동안 진행을 이보다 자주 알리지 않는다 (섹션이 바뀔 때는 바로)
const REPORT_EVERY: Duration = Duration::from_millis(200);

/// 답 형식의 섹션: (이름, 답 전체에서 차지하는 몫, 보통 길이(글자))
type Sections = &'static [(&'static str, f32, f32)];

/// [`SYSTEM`] 형식 — 몫은 실제 회의록에서 섹션마다 차지하는 대략의 비율
const FINAL_SECTIONS: Sections = &[
    ("제목", 0.02, 20.0),
    ("한 줄 요약", 0.06, 80.0),
    ("주요 논의", 0.45, 700.0),
    ("결정 사항", 0.15, 250.0),
    ("할 일", 0.22, 350.0),
    ("남은 질문", 0.10, 150.0),
];

/// [`PART_SYSTEM`] 형식 (구간 메모 · 메모 합치기 · 줄이기)
const PART_SECTIONS: Sections = &[
    ("논의", 0.55, 600.0),
    ("결정 사항", 0.15, 200.0),
    ("할 일", 0.20, 250.0),
    ("남은 질문", 0.10, 120.0),
];

/// 받아 오는 답에서 지금 어느 섹션을 쓰는지 따라가며 진행을 어림한다.
/// 섹션 안에서는 보통 길이만큼 쓰면 그 섹션 몫의 63%, 세 배면 95% — 다음 섹션 제목이 나와야 넘어간다
struct Writing {
    sections: Sections,
    /// 지금 줄 (제목 줄인지 보려고 앞부분만)
    line: String,
    at: Option<usize>,
    in_section: usize,
    chars: usize,
    /// 지금까지 알린 가장 큰 진행 — 첫 섹션 제목이 나올 때 글자 수 어림보다 낮아져도 막대가 되돌아가지 않게
    best: f32,
}

impl Writing {
    fn new(sections: Sections) -> Self {
        Writing { sections, line: String::new(), at: None, in_section: 0, chars: 0, best: 0.0 }
    }

    /// 받은 글 조각을 더한다. 새 섹션으로 넘어갔으면 true
    fn push(&mut self, delta: &str) -> bool {
        let mut moved = false;
        for ch in delta.chars() {
            self.chars += 1;
            self.in_section += 1;
            if ch != '\n' {
                // 섹션 제목은 짧다 — 긴 줄은 앞부분만 있어도 제목이 아님을 안다
                if self.line.chars().count() < 40 {
                    self.line.push(ch);
                }
                continue;
            }
            if let Some(i) = self.header(&self.line) {
                // 앞으로만 (모델이 같은 이름을 본문에 한 줄로 다시 써도 되돌아가지 않게)
                if self.at.is_none_or(|a| i > a) {
                    self.at = Some(i);
                    self.in_section = 0;
                    moved = true;
                }
            }
            self.line.clear();
        }
        self.best = self.best.max(self.estimate());
        moved
    }

    /// "### 주요 논의", "## 결정 사항", "**할 일**:" 같은 줄이 몇 번째 섹션인지
    fn header(&self, line: &str) -> Option<usize> {
        let t = line
            .trim()
            .trim_start_matches('#')
            .trim()
            .trim_end_matches(':')
            .trim_matches('*')
            .trim_end_matches(':')
            .trim();
        self.sections.iter().position(|s| s.0 == t)
    }

    fn frac(&self) -> f32 {
        self.best
    }

    fn estimate(&self) -> f32 {
        let ease = |n: usize, len: f32| 1.0 - (-(n as f32) / len).exp();
        match self.at {
            Some(i) => {
                let start: f32 = self.sections[..i].iter().map(|s| s.1).sum();
                let (_, share, len) = self.sections[i];
                (start + share * ease(self.in_section, len)).min(0.99)
            }
            // 섹션 제목 없이 쓰는 모델 — 쓴 글자 수로만 (끝을 모르니 90%까지)
            None => 0.9 * ease(self.chars, self.sections.iter().map(|s| s.2).sum()),
        }
    }

    fn section(&self) -> &'static str {
        self.at.map_or("", |i| self.sections[i].0)
    }
}

/// 요청 하나의 결과
#[derive(Debug, Default)]
pub struct Reply {
    pub text: String,
    /// 실제로 답한 모델 (응답에 없으면 요청한 모델) — 사용량 금액 셈용
    pub model: String,
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

/// 사용자가 요약을 멈춤
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(CANCELLED_MSG)
    }
}

impl std::error::Error for Cancelled {}

pub const CANCELLED_MSG: &str = "요약을 멈췄어요";

/// 네트워크 오류를 우리말로 (끊김 · 시간 초과를 가려서)
fn net(e: reqwest::Error) -> anyhow::Error {
    anyhow!(badasugi_core::asr::openai::describe_http_error(&e))
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
#[derive(Clone)]
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

/// 이어진 메모들을 실제 크기대로 묶는다 (묶음마다 ≤ budget 글자, 메모 하나가 더 크면 혼자)
fn pack(notes: &[Note], budget: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut size) = (0, 0);
    for (i, n) in notes.iter().enumerate() {
        let len = n.render("구간 00").chars().count() + 2;
        if i > start && size + len > budget {
            out.push(start..i);
            start = i;
            size = 0;
        }
        size += len;
    }
    if start < notes.len() {
        out.push(start..notes.len());
    }
    out
}

const MERGE_ASK: &str = "다음은 이 회의 여러 구간의 메모입니다. 하나의 메모로 합치세요. \
같은 내용은 하나로 합치고, 결정 사항과 할 일은 빠뜨리지 마세요.";
const SHRINK_ASK: &str = "다음은 이 회의 한 구간의 메모입니다. 결정 사항 · 할 일 · 숫자 · 이름은 그대로 두고, \
나머지 논의는 핵심만 남겨 절반 길이로 줄이세요.";

/// 요청 하나 보내기: (지시, 내용, 받는 대로 글 조각을 넘길 곳) → 결과
type Ask<'a> = dyn FnMut(&str, &str, &mut dyn FnMut(&str)) -> Result<Reply> + 'a;

/// 요청 보내기 · 진행 알림. 요청이 성공할 때마다 `on_reply` 로 사용량을 바로 알린다
struct Run<'a> {
    call: &'a mut Ask<'a>,
    progress: &'a dyn Fn(Progress),
    on_reply: &'a dyn Fn(&Reply),
    done: usize,
    total: usize,
}

impl Run<'_> {
    fn report(&self, wait_secs: u64) {
        (self.progress)(Progress {
            done: self.done,
            total: self.total,
            wait_secs,
            frac: 0.0,
            writing: false,
            section: "",
        });
    }

    /// 한도 초과(429)면 알려 준 시간만큼 기다렸다 다시 (몇 번까지만).
    /// 답을 받는 동안 어느 섹션을 쓰는지로 진행을 알린다
    fn ask(&mut self, system: &str, user: &str) -> Result<String> {
        let sections = if system == SYSTEM { FINAL_SECTIONS } else { PART_SECTIONS };
        let mut tries = 0;
        loop {
            let (done, total, progress) = (self.done, self.total, self.progress);
            let mut w = Writing::new(sections);
            let mut last: Option<std::time::Instant> = None;
            let mut on_text = |delta: &str| {
                let moved = w.push(delta);
                if moved || last.is_none_or(|t| t.elapsed() >= REPORT_EVERY) {
                    last = Some(std::time::Instant::now());
                    progress(Progress { done, total, wait_secs: 0, frac: w.frac(), writing: true, section: w.section() });
                }
            };
            match (self.call)(system, user, &mut on_text) {
                Ok(r) => {
                    // 뒤 요청이 실패해도 이미 쓴 만큼은 사용량에 남게 바로 알린다
                    (self.on_reply)(&r);
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

    /// 메모 묶음을 하나로. 그래도 너무 크다고 거절되면 반으로 나눠 따로 합친다
    fn merge(&mut self, head: &str, g: &[Note]) -> Result<Vec<Note>> {
        if g.len() < 2 {
            return Ok(g.to_vec());
        }
        match self.ask(PART_SYSTEM, &format!("{head}{MERGE_ASK}\n\n{}", render_notes(g))) {
            Ok(text) => {
                self.done += 1;
                self.report(0);
                Ok(vec![Note { from: g[0].from.clone(), to: g[g.len() - 1].to.clone(), text }])
            }
            Err(e) if too_large(&e) => {
                let (a, b) = g.split_at(g.len() / 2);
                // 요청 하나가 (2개 이상인 쪽마다) 하나씩으로 바뀜
                self.total = (self.total + (a.len() > 1) as usize + (b.len() > 1) as usize).saturating_sub(1);
                let mut out = self.merge(head, a)?;
                out.extend(self.merge(head, b)?);
                Ok(out)
            }
            Err(e) => Err(e),
        }
    }
}

/// 녹취록(`text`)을 요약한다. 한 번에 보내기엔 길면(`budget` 글자 초과) 구간별로 메모한 뒤 합친다.
/// `head` 는 회의 제목 · 일시 줄
fn run_summary(
    call: &mut Ask,
    progress: &dyn Fn(Progress),
    on_reply: &dyn Fn(&Reply),
    head: &str,
    text: &str,
    mut budget: usize,
) -> Result<String> {
    let mut r = Run { call, progress, on_reply, done: 0, total: 1 };
    let len = text.chars().count();
    if len <= budget {
        match r.ask(SYSTEM, &format!("{head}녹취록:\n{text}")) {
            Ok(out) => return Ok(out),
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

    // ── 메모 합치기: 한 번에 보내기엔 크면 크기대로 묶어 줄인 뒤 (최대 3번) ──
    for round in 0.. {
        let joined = render_notes(&notes);
        if joined.chars().count() <= budget || round >= 3 {
            let user = format!(
                "{head}이 회의는 길어서 녹취록을 구간별로 먼저 메모했습니다. 아래 구간 메모를 모두 합쳐 회의 전체 회의록을 만드세요. \
                 같은 내용은 하나로 합치고, 결정 사항과 할 일은 빠뜨리지 마세요.\n\n{joined}"
            );
            match r.ask(SYSTEM, &user) {
                Ok(out) => {
                    r.done = r.total;
                    r.report(0);
                    return Ok(out);
                }
                Err(e) if too_large(&e) && round < 3 => budget = budget * 2 / 3,
                Err(e) => return Err(e),
            }
        }
        let groups = pack(&notes, budget);
        if groups.len() < notes.len() {
            // 크기에 맞춰 묶어 합친다 (묶음마다 ≤ budget)
            r.total += groups.iter().filter(|g| g.len() > 1).count();
            r.report(0);
            let old = std::mem::take(&mut notes);
            for g in groups {
                notes.extend(r.merge(head, &old[g])?);
            }
        } else {
            // 메모 하나하나가 커서 묶을 수 없다 — 각각 줄인다
            r.total += notes.len();
            r.report(0);
            for i in 0..notes.len() {
                let user = format!("{head}{SHRINK_ASK}\n\n{}", notes[i].render("구간"));
                notes[i].text = r.ask(PART_SYSTEM, &user)?;
                r.done += 1;
                r.report(0);
            }
        }
    }
    unreachable!()
}

/// 요약 결과: 본문 + (모델이 제안한) 회의 제목
pub struct Summary {
    pub text: String,
    pub title: Option<String>,
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
        get_key(name).ok_or_else(|| anyhow!("{label} API 키를 설정 › 요약에서 입력해 주세요"))
    };
    Ok(match cfg.provider.as_str() {
        "groq" => Some((GROQ_URL.into(), Some(need("groq", "Groq")?), cfg.groq_model.clone())),
        "gemini" => Some((GEMINI_URL.into(), Some(need("gemini", "Gemini")?), cfg.gemini_model.clone())),
        "openai" => Some((OPENAI_URL.into(), Some(need("openai", "OpenAI")?), cfg.openai_model.clone())),
        "custom" => Some((cfg.custom_url.clone(), get_key("llm-custom"), cfg.custom_model.clone())),
        _ => None,
    })
}

type Caller = Box<Ask<'static>>;

/// 회의록을 요약한다. 답은 받는 대로 읽어 `progress` 로 진행(긴 회의는 구간까지)을,
/// `on_usage(제공자, 응답)` 로 요청마다 사용량을 알린다 (중간에 실패해도 이미 쓴 만큼은 셀 수 있게)
pub fn summarize(
    cfg: &SummarySettings,
    doc: &SessionDoc,
    progress: &dyn Fn(Progress),
    on_usage: &dyn Fn(&str, &Reply),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Summary> {
    if doc.utterances.is_empty() {
        bail!("요약할 내용이 없습니다");
    }
    let (provider, mut call): (String, Caller) = match cfg.provider.as_str() {
        "ollama" => {
            let c = cfg.clone();
            ("ollama".into(), Box::new(move |s: &str, u: &str, t: &mut dyn FnMut(&str)| ollama(&c, s, u, t)))
        }
        "anthropic" => {
            let c = cfg.clone();
            ("anthropic".into(), Box::new(move |s: &str, u: &str, t: &mut dyn FnMut(&str)| anthropic(&c, s, u, t)))
        }
        _ => match openai_like(cfg)? {
            Some((base, key, model)) => (
                badasugi_core::usage::provider_of(&base).to_string(),
                Box::new(move |s: &str, u: &str, t: &mut dyn FnMut(&str)| {
                    openai_compat(&base, key.as_deref(), &model, s, u, t)
                }),
            ),
            None => bail!("알 수 없는 요약 제공자: {}", cfg.provider),
        },
    };
    let head = format!("회의 제목: {}\n일시: {}\n\n", doc.title, doc.started_at);
    let on_reply = |r: &Reply| on_usage(&provider, r);
    // 멈추면 다음 요청부터 보내지 않는다 (보내 둔 요청은 끝날 때까지 기다릴 수밖에 없어 화면은 먼저 돌려보낸다)
    let mut guarded = |s: &str, u: &str, t: &mut dyn FnMut(&str)| -> Result<Reply> {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(anyhow::Error::new(Cancelled));
        }
        call(s, u, t)
    };
    let raw = run_summary(&mut guarded, progress, &on_reply, &head, &doc.to_text(), chunk_budget(&provider))?;
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

/// 답을 받다가 응답이 끊김 없이 너무 오래 멈춤 (읽기 한 번마다 클라이언트 시간 제한이 걸린다)
#[derive(Debug)]
struct StreamStalled(String);

impl std::fmt::Display for StreamStalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StreamStalled {}

fn read_error(e: std::io::Error) -> anyhow::Error {
    // reqwest 오류가 io 오류에 싸여 온다
    match e.get_ref().and_then(|i| i.downcast_ref::<reqwest::Error>()) {
        Some(re) if re.is_timeout() => anyhow::Error::new(StreamStalled(format!(
            "요약 답을 받다가 서버가 멈췄어요 ({})",
            badasugi_core::asr::openai::describe_http_error(re)
        ))),
        Some(re) => anyhow!("요약 답을 받다가 끊겼어요 ({})", badasugi_core::asr::openai::describe_http_error(re)),
        None if e.kind() == std::io::ErrorKind::TimedOut => {
            anyhow::Error::new(StreamStalled(format!("요약 답을 받다가 서버가 멈췄어요 ({e})")))
        }
        None => anyhow!("요약 답을 받다가 끊겼어요 ({e})"),
    }
}

/// 스트리밍 답을 한 줄씩 (SSE 의 "data: …" 줄 · Ollama 의 JSON 줄). `f` 가 false 를 주면 그만 읽는다
fn each_line(r: impl std::io::Read, mut f: impl FnMut(&str) -> Result<bool>) -> Result<()> {
    use std::io::BufRead;
    let mut rd = std::io::BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).map_err(read_error)? == 0 {
            return Ok(());
        }
        if !f(line.trim_end_matches(['\r', '\n']))? {
            return Ok(());
        }
    }
}

/// SSE 의 "data: …" 줄 → JSON (다른 줄 · 깨진 줄은 None)
fn sse_json(line: &str) -> Option<Value> {
    serde_json::from_str(line.strip_prefix("data:")?.trim()).ok()
}

/// 답을 보내는 도중 온 오류 ({"error": …}) — 상태 코드가 없으니 오류 종류로 가늠한다
fn stream_error(v: &Value) -> Option<anyhow::Error> {
    let e = &v["error"];
    if e.is_null() {
        return None;
    }
    let msg = e["message"].as_str().or_else(|| e.as_str()).map(str::to_string).unwrap_or_else(|| e.to_string());
    let status = e["code"]
        .as_u64()
        .and_then(|c| u16::try_from(c).ok())
        .filter(|c| (400..600).contains(c))
        .unwrap_or(match e["type"].as_str().unwrap_or("") {
            "rate_limit_error" => 429,
            "overloaded_error" => 529,
            "invalid_request_error" => 400,
            _ => 500,
        });
    Some(anyhow::Error::new(ApiError { status, retry_after: None, msg }))
}

/// Ollama 의 답 (한 줄에 JSON 하나, 마지막 줄 "done": true 에 토큰 수)
fn read_ollama_stream(r: impl std::io::Read, on_text: &mut dyn FnMut(&str)) -> Result<Reply> {
    let mut out = Reply::default();
    each_line(r, |line| {
        if line.trim().is_empty() {
            return Ok(true);
        }
        let v: Value = serde_json::from_str(line).map_err(|e| anyhow!("Ollama 답을 읽지 못했어요: {e}"))?;
        if let Some(e) = v["error"].as_str() {
            bail!("Ollama 오류: {e}");
        }
        if let Some(m) = v["model"].as_str().filter(|m| !m.is_empty()) {
            out.model = m.to_string();
        }
        if let Some(t) = v["message"]["content"].as_str().filter(|t| !t.is_empty()) {
            out.text.push_str(t);
            on_text(t);
        }
        if v["done"].as_bool() == Some(true) {
            out.in_tokens = v["prompt_eval_count"].as_u64().unwrap_or(0);
            out.out_tokens = v["eval_count"].as_u64().unwrap_or(0);
            return Ok(false);
        }
        Ok(true)
    })?;
    out.text = out.text.trim().to_string();
    Ok(out)
}

const OLLAMA_SLOW: &str = "Ollama 가 15분 넘게 답하지 못했어요. 더 가벼운 모델을 고르거나 Groq·Gemini 같은 API 를 써 보세요.";

fn ollama(cfg: &SummarySettings, system: &str, user: &str, on_text: &mut dyn FnMut(&str)) -> Result<Reply> {
    let url = format!("{}/api/chat", cfg.ollama_url.trim().trim_end_matches('/'));
    let body = json!({
        "model": cfg.ollama_model,
        // 받는 대로 읽어 진행을 보여 준다
        "stream": true,
        // 기본 컨텍스트(2~4K)로는 긴 회의가 잘리므로 넉넉히
        "options": { "num_ctx": 32768, "temperature": 0.2 },
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
    });
    // 시간 제한은 읽기 한 번마다 — 녹취록을 읽는 동안(첫 글자 전) · 쓰다 멈춘 동안 15분
    let resp = client(900)?.post(&url).json(&body).send().map_err(|e| {
        if e.is_timeout() {
            anyhow!(OLLAMA_SLOW)
        } else {
            anyhow!(
                "Ollama 에 연결할 수 없어요. Ollama 를 설치·실행했는지 확인하거나, 설정에서 Groq·Gemini 같은 API 를 골라 주세요. ({})",
                badasugi_core::asr::openai::describe_http_error(&e)
            )
        }
    })?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().map_err(net)?;
        if status.as_u16() == 404 && text.contains("not found") {
            bail!(
                "Ollama 에 '{}' 모델이 없어요. 명령 프롬프트에서 `ollama pull {}` 로 먼저 받아 주세요.",
                cfg.ollama_model,
                cfg.ollama_model
            );
        }
        return Err(error_message(status, None, &text));
    }
    let mut reply =
        read_ollama_stream(resp, on_text).map_err(|e| if e.is::<StreamStalled>() { anyhow!(OLLAMA_SLOW) } else { e })?;
    if reply.model.is_empty() {
        reply.model = cfg.ollama_model.clone();
    }
    Ok(reply)
}

/// OpenAI 호환 서버에 보내는 방식 — 받지 않는 서버가 있어 차례로 물러선다
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Streaming {
    /// 받는 대로 + 마지막에 토큰 수 (stream_options.include_usage)
    WithUsage,
    /// 받는 대로 (stream_options 를 모르는 서버)
    Plain,
    /// 한 번에 (스트리밍을 모르는 서버)
    Off,
}

/// 오류 문구가 스트리밍 설정을 받지 않는다는 뜻인지 ("upstream" 같은 낱말은 빼고)
fn mentions_stream(body: &str) -> bool {
    body.to_lowercase()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|w| w == "stream" || w.starts_with("stream_"))
}

/// OpenAI 호환 한 번에 받은 답
fn openai_reply(v: &Value) -> Result<Reply> {
    if v["choices"][0]["finish_reason"].as_str() == Some("content_filter") {
        bail!("모델이 요약을 거절했습니다 (콘텐츠 필터)");
    }
    let usage = &v["usage"];
    Ok(Reply {
        text: v["choices"][0]["message"]["content"].as_str().unwrap_or_default().trim().to_string(),
        model: v["model"].as_str().unwrap_or_default().to_string(),
        in_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
        out_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
    })
}

/// OpenAI 호환 스트리밍 답 (SSE). 토큰 수는 마지막 조각의 usage (Groq 는 x_groq.usage 에도)
fn read_openai_stream(r: impl std::io::Read, on_text: &mut dyn FnMut(&str)) -> Result<Reply> {
    let mut out = Reply::default();
    let mut filtered = false;
    each_line(r, |line| {
        if line.strip_prefix("data:").is_some_and(|d| d.trim() == "[DONE]") {
            return Ok(false);
        }
        let Some(v) = sse_json(line) else { return Ok(true) };
        if let Some(e) = stream_error(&v) {
            return Err(e);
        }
        if let Some(m) = v["model"].as_str().filter(|m| !m.is_empty()) {
            out.model = m.to_string();
        }
        let c = &v["choices"][0];
        if let Some(t) = c["delta"]["content"].as_str().filter(|t| !t.is_empty()) {
            out.text.push_str(t);
            on_text(t);
        }
        if c["finish_reason"].as_str() == Some("content_filter") {
            filtered = true;
        }
        let usage = if v["usage"].is_object() { &v["usage"] } else { &v["x_groq"]["usage"] };
        if usage.is_object() {
            out.in_tokens = usage["prompt_tokens"].as_u64().unwrap_or(out.in_tokens);
            out.out_tokens = usage["completion_tokens"].as_u64().unwrap_or(out.out_tokens);
        }
        Ok(true)
    })?;
    if filtered {
        bail!("모델이 요약을 거절했습니다 (콘텐츠 필터)");
    }
    out.text = out.text.trim().to_string();
    Ok(out)
}

fn openai_compat(
    base: &str,
    key: Option<&str>,
    model: &str,
    system: &str,
    user: &str,
    on_text: &mut dyn FnMut(&str),
) -> Result<Reply> {
    if base.trim().is_empty() {
        bail!("요약 API 주소를 입력해 주세요");
    }
    if model.trim().is_empty() {
        bail!("요약 모델 이름을 입력해 주세요");
    }
    let url = format!("{}/chat/completions", base.trim().trim_end_matches('/'));
    for mode in [Streaming::WithUsage, Streaming::Plain, Streaming::Off] {
        let mut body = json!({
            "model": model.trim(),
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ]
        });
        if mode != Streaming::Off {
            body["stream"] = json!(true);
        }
        if mode == Streaming::WithUsage {
            body["stream_options"] = json!({ "include_usage": true });
        }
        // 시간 제한은 읽기 한 번마다 — 긴 답도 받는 동안은 끊기지 않는다
        let mut rb = client(600)?.post(&url).json(&body);
        if let Some(k) = key {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().map_err(net)?;
        badasugi_core::usage::record_headers(&url, resp.headers());
        let status = resp.status();
        let wait = retry_after(resp.headers());
        if !status.is_success() {
            let text = resp.text().map_err(net)?;
            if mode != Streaming::Off && matches!(status.as_u16(), 400 | 422) && mentions_stream(&text) {
                log::info!("요약 서버가 스트리밍 설정({mode:?})을 받지 않아 물러서서 다시");
                continue;
            }
            return Err(error_message(status, wait, &text));
        }
        // 스트리밍을 무시하고 한 번에 보내는 서버도 있다
        let json_body = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("application/json"));
        let mut reply = if mode == Streaming::Off || json_body {
            let text = resp.text().map_err(net)?;
            let reply = openai_reply(&serde_json::from_str(&text)?)?;
            on_text(&reply.text);
            reply
        } else {
            read_openai_stream(resp, on_text)?
        };
        // 실제로 답한 모델 (응답에 없으면 요청한 모델) — 사용량 금액 셈용
        if reply.model.is_empty() {
            reply.model = model.trim().to_string();
        }
        return Ok(reply);
    }
    unreachable!()
}

/// 서버측 거절 폴백을 지원하는 모델
fn supports_fallbacks(model: &str) -> bool {
    matches!(
        model,
        "claude-fable-5-2" | "claude-fable-5-1" | "claude-opus-5-5" | "claude-opus-5" | "claude-sonnet-5-5"
    )
}

/// `output_config.effort` 를 받는 모델인지 — Claude Opus 4.5 부터 받고, Sonnet 4.5 · Haiku 4.5 와 그 이전 모델은
/// 보내면 400 으로 거절한다 (설정에서 모델 목록을 불러와 옛 모델을 고를 수 있다)
fn supports_effort(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let old = m.starts_with("claude-3")
        || m.starts_with("claude-2")
        || m.starts_with("claude-instant")
        || ["sonnet-4-5", "haiku-4-5", "opus-4-1", "opus-4-0", "sonnet-4-0", "opus-4-2025", "sonnet-4-2025"]
            .iter()
            .any(|k| m.contains(k));
    !old
}

/// Claude 인증: 저장한 API 키, 없으면 Anthropic CLI(`ant auth login`) 로그인 토큰
enum ClaudeAuth {
    Key(String),
    Login(String),
}

const OAUTH_BETA: &str = "oauth-2025-04-20";

impl ClaudeAuth {
    fn get() -> Result<Self> {
        if let Some(k) = get_key("anthropic") {
            return Ok(Self::Key(k));
        }
        let ant = crate::ant::find().ok_or_else(|| {
            anyhow!("Anthropic API 키를 설정 › 요약에서 입력하거나, Anthropic CLI(ant)를 설치하고 ant auth login 으로 로그인해 주세요")
        })?;
        Ok(Self::Login(crate::ant::access_token(&ant)?))
    }

    /// 인증 · 버전 · 베타 헤더를 붙인다. 로그인 토큰은 `x-api-key` 가 아니라 Bearer 로 보내고 OAuth 베타를 켜야 한다
    fn apply(&self, rb: reqwest::blocking::RequestBuilder, betas: &[&str]) -> reqwest::blocking::RequestBuilder {
        let rb = match self {
            Self::Key(k) => rb.header("x-api-key", k),
            Self::Login(t) => rb.bearer_auth(t),
        };
        let rb = rb.header("anthropic-version", ANTHROPIC_VERSION);
        match beta_header(betas, matches!(self, Self::Login(_))) {
            Some(b) => rb.header("anthropic-beta", b),
            None => rb,
        }
    }

    /// 로그인 토큰이 거절되면 API 키가 아니라 ant 로그인을 다시 하라고 알린다
    fn explain(&self, e: anyhow::Error) -> anyhow::Error {
        match (self, e.downcast_ref::<ApiError>()) {
            (Self::Login(_), Some(a)) if matches!(a.status, 401 | 403) => anyhow!(
                "ant 로그인으로 Claude 를 쓸 수 없어요: {} — 명령 프롬프트에서 ant auth login 을 다시 실행해 주세요",
                a.msg
            ),
            _ => e,
        }
    }
}

/// `anthropic-beta` 값 (여러 개는 쉼표로 잇는다)
fn beta_header(betas: &[&str], login: bool) -> Option<String> {
    let mut all = betas.to_vec();
    if login {
        all.push(OAUTH_BETA);
    }
    (!all.is_empty()).then(|| all.join(","))
}

/// Claude 스트리밍 답 (SSE). 입력 토큰은 message_start, 출력 토큰은 message_delta 에.
/// 서버측 거절 폴백이 중간에 일어나면 같은 스트림에서 이어 쓰므로 글은 그대로 이어 붙이면 된다
fn read_anthropic_stream(r: impl std::io::Read, on_text: &mut dyn FnMut(&str)) -> Result<Reply> {
    let mut out = Reply::default();
    let mut stop = String::new();
    let mut why = String::new();
    each_line(r, |line| {
        let Some(v) = sse_json(line) else { return Ok(true) };
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let m = &v["message"];
                // 쓰기 전에 거절해 다른 모델로 넘어갔으면(fallbacks) 여기부터 그 모델
                out.model = m["model"].as_str().unwrap_or_default().to_string();
                out.in_tokens = m["usage"]["input_tokens"].as_u64().unwrap_or(0);
                out.out_tokens = m["usage"]["output_tokens"].as_u64().unwrap_or(0);
            }
            // 쓰다가 거절해 다른 모델이 이어 쓰면 그 경계에 fallback 블록이 온다
            "content_block_start" if v["content_block"]["type"].as_str() == Some("fallback") => {
                if let Some(m) = v["content_block"]["to"]["model"].as_str() {
                    out.model = m.to_string();
                }
            }
            "content_block_delta" if v["delta"]["type"].as_str() == Some("text_delta") => {
                if let Some(t) = v["delta"]["text"].as_str().filter(|t| !t.is_empty()) {
                    out.text.push_str(t);
                    on_text(t);
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    stop = s.to_string();
                }
                let details = if v["delta"]["stop_details"].is_object() { &v["delta"]["stop_details"] } else { &v["stop_details"] };
                if let Some(e) = details["explanation"].as_str() {
                    why = e.to_string();
                }
                let u = &v["usage"];
                out.in_tokens = u["input_tokens"].as_u64().unwrap_or(out.in_tokens);
                out.out_tokens = u["output_tokens"].as_u64().unwrap_or(out.out_tokens);
            }
            "message_stop" => return Ok(false),
            "error" => return Err(stream_error(&v).unwrap_or_else(|| anyhow!("요약 API 오류"))),
            _ => {}
        }
        Ok(true)
    })?;
    if stop == "refusal" {
        bail!("모델이 요약을 거절했습니다. {why}");
    }
    out.text = out.text.trim().to_string();
    Ok(out)
}

fn anthropic(cfg: &SummarySettings, system: &str, user: &str, on_text: &mut dyn FnMut(&str)) -> Result<Reply> {
    // 로그인 토큰은 짧게 유효하니 요청마다 받는다 (긴 회의는 요약에 수십 분 걸리기도 한다)
    let auth = ClaudeAuth::get()?;
    let model = if cfg.anthropic_model.trim().is_empty() {
        "claude-opus-5-5"
    } else {
        cfg.anthropic_model.trim()
    };
    let mut body = json!({
        "model": model,
        "max_tokens": 16000,
        // 받는 대로 읽어 진행을 보여 준다 (시간 제한도 읽기 한 번마다라 긴 답이 끊기지 않는다)
        "stream": true,
        "system": system,
        "messages": [ { "role": "user", "content": user } ]
    });
    if supports_effort(model) {
        body["output_config"] = json!({ "effort": "medium" });
    }
    let mut betas = Vec::new();
    if supports_fallbacks(model) {
        // 안전 분류기가 거절하면 서버가 알아서 다른 모델로 다시 시도
        body["fallbacks"] = json!("default");
        betas.push("server-side-fallback-2026-07-01");
    }
    let rb = client(600)?
        .post(format!("{ANTHROPIC_URL}/messages"))
        .header("content-type", "application/json");
    let resp = auth.apply(rb, &betas).json(&body).send().map_err(net)?;
    badasugi_core::usage::record_headers(ANTHROPIC_URL, resp.headers());
    let status = resp.status();
    let wait = retry_after(resp.headers());
    if !status.is_success() {
        let text = resp.text().map_err(net)?;
        return Err(auth.explain(error_message(status, wait, &text)));
    }
    let mut reply = read_anthropic_stream(resp, on_text).map_err(|e| auth.explain(e))?;
    if reply.model.is_empty() {
        reply.model = model.to_string();
    }
    Ok(reply)
}

/// 연결 확인 겸 사용할 수 있는 모델 목록 (설정 화면 자동 완성용)
pub fn list_models(cfg: &SummarySettings) -> Result<Vec<String>> {
    let c = client(20)?;
    let mut claude: Option<ClaudeAuth> = None;
    let (resp, ollama) = match cfg.provider.as_str() {
        "ollama" => {
            let url = format!("{}/api/tags", cfg.ollama_url.trim().trim_end_matches('/'));
            let r = c.get(&url).send().map_err(|_| {
                anyhow!("Ollama 에 연결할 수 없어요. https://ollama.com 에서 설치한 뒤 실행해 주세요.")
            })?;
            (r, true)
        }
        "anthropic" => {
            let auth = claude.insert(ClaudeAuth::get()?);
            (auth.apply(c.get(format!("{ANTHROPIC_URL}/models?limit=100")), &[]).send().map_err(net)?, false)
        }
        _ => {
            let (base, key, _) = openai_like(cfg)?.ok_or_else(|| anyhow!("알 수 없는 요약 제공자"))?;
            if base.trim().is_empty() {
                bail!("요약 API 주소를 입력해 주세요");
            }
            let mut rb = c.get(format!("{}/models", base.trim().trim_end_matches('/')));
            if let Some(k) = key {
                rb = rb.bearer_auth(k);
            }
            (rb.send().map_err(net)?, false)
        }
    };
    let status = resp.status();
    let text = resp.text().map_err(net)?;
    if !status.is_success() {
        let e = error_message(status, None, &text);
        return Err(match &claude {
            Some(auth) => auth.explain(e),
            None => e,
        });
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
    fn login_turns_on_the_oauth_beta() {
        assert_eq!(beta_header(&[], false), None);
        assert_eq!(beta_header(&[], true).as_deref(), Some("oauth-2025-04-20"));
        let fb = ["server-side-fallback-2026-07-01"];
        assert_eq!(beta_header(&fb, false).as_deref(), Some("server-side-fallback-2026-07-01"));
        assert_eq!(beta_header(&fb, true).as_deref(), Some("server-side-fallback-2026-07-01,oauth-2025-04-20"));
    }

    #[test]
    fn rejected_login_points_to_ant_not_the_api_key() {
        let rejected = || anyhow::Error::new(ApiError { status: 401, retry_after: None, msg: "Invalid bearer token".into() });
        let login = ClaudeAuth::Login("t".into()).explain(rejected()).to_string();
        assert!(login.contains("ant auth login") && login.contains("Invalid bearer token"), "{login}");
        let key = ClaudeAuth::Key("k".into()).explain(rejected()).to_string();
        assert!(key.contains("API 키를 확인"), "{key}");
        // 한도 초과 같은 다른 오류는 그대로 둬야 기다렸다 다시 보내기가 동작한다
        let busy = anyhow::Error::new(ApiError { status: 429, retry_after: Some(5), msg: "rate".into() });
        assert!(ClaudeAuth::Login("t".into()).explain(busy).downcast_ref::<ApiError>().is_some());
    }

    #[test]
    fn effort_is_sent_only_to_models_that_take_it() {
        for m in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-2", "claude-opus-4-5", "claude-opus-4-8"] {
            assert!(supports_effort(m), "{m}");
        }
        for m in ["claude-sonnet-4-5", "claude-haiku-4-5-20251001", "claude-sonnet-4-5-20250929", "claude-opus-4-1-20250805", "claude-3-5-haiku-latest", "claude-sonnet-4-20250514"] {
            assert!(!supports_effort(m), "{m}");
        }
    }

    #[test]
    fn cancel_stops_before_the_next_request() {
        let flag = std::sync::atomic::AtomicBool::new(true);
        let doc: SessionDoc = serde_json::from_value(json!({
            "version": 1, "id": "t", "title": "t", "startedAt": "2026-11-16 09:00", "durationMs": 1000, "engine": "x",
            "utterances": [ { "id": 0, "startMs": 0, "endMs": 1000, "speaker": 0, "text": "안녕하세요" } ]
        }))
        .unwrap();
        let cfg = SummarySettings { provider: "ollama".into(), ollama_url: "http://127.0.0.1:9".into(), ..Default::default() };
        let Err(e) = summarize(&cfg, &doc, &|_| {}, &|_, _| {}, &flag) else {
            panic!("멈췄는데 요약이 됐다");
        };
        assert_eq!(e.to_string(), CANCELLED_MSG);
    }

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

    /// 성공한 요청 수를 세는 사용량 콜백
    fn counter() -> (std::rc::Rc<std::cell::Cell<u64>>, impl Fn(&Reply)) {
        let n = std::rc::Rc::new(std::cell::Cell::new(0));
        let m = n.clone();
        (n, move |_: &Reply| m.set(m.get() + 1))
    }

    #[test]
    fn short_meeting_is_one_request() {
        let mut sent = Vec::new();
        let mut call = |sys: &str, user: &str, _: &mut dyn FnMut(&str)| {
            sent.push((sys == SYSTEM, user.to_string()));
            Ok(Reply { text: "요약".into(), in_tokens: 10, out_tokens: 5, ..Default::default() })
        };
        let (req, on_reply) = counter();
        let out = run_summary(&mut call, &|_| {}, &on_reply, "회의 제목: t\n\n", &transcript(10, 20), 12_000).unwrap();
        assert_eq!((out.as_str(), req.get()), ("요약", 1));
        assert!(sent[0].0 && sent[0].1.contains("녹취록:"));
    }

    #[test]
    fn long_meeting_is_split_then_merged() {
        let t = transcript(200, 60); // 약 1.5만 글자
        let mut calls = Vec::new();
        let mut call = |sys: &str, user: &str, _: &mut dyn FnMut(&str)| {
            calls.push((sys == PART_SYSTEM, user.to_string()));
            Ok(Reply { text: format!("메모{}", calls.len()), in_tokens: 1, out_tokens: 1, ..Default::default() })
        };
        let seen = std::cell::RefCell::new(Vec::new());
        let (req, on_reply) = counter();
        let out = run_summary(&mut call, &|p| seen.borrow_mut().push(p), &on_reply, "회의 제목: t\n\n", &t, 4_000).unwrap();
        let parts = calls.iter().filter(|c| c.0).count();
        assert!(parts >= 4, "구간으로 나눔: {parts}");
        assert_eq!(req.get() as usize, parts + 1, "구간 + 합치기");
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
        let mut call = |sys: &str, user: &str, _: &mut dyn FnMut(&str)| {
            // 처음 한 번 보낸 요청(전체)과 큰 구간은 거절
            if sys == SYSTEM && !user.contains("구간 메모") || user.chars().count() > 1_500 {
                return Err(too_big());
            }
            Ok(Reply { text: "ok".into(), ..Default::default() })
        };
        let (req, on_reply) = counter();
        let out = run_summary(&mut call, &|_| {}, &on_reply, "", &t, 5_000).unwrap();
        assert_eq!(out, "ok");
        assert!(req.get() >= 5, "한 번에 → 두 구간 → 다시 반씩, 끝까지: {}회", req.get());
    }

    /// 메모가 커서 합칠 때도 한 번에 보내는 크기가 budget 을 넘지 않아야 한다 (두 메모 2,500자 · budget 4,000)
    #[test]
    fn merging_big_notes_stays_within_budget() {
        let t = transcript(100, 60); // 약 7,500자 → 두 구간
        let mut biggest = 0;
        let mut call = |sys: &str, user: &str, _: &mut dyn FnMut(&str)| {
            let len = user.chars().count();
            biggest = biggest.max(len);
            if len > 4_300 {
                return Err(too_big());
            }
            let text = if user.contains("절반 길이") {
                "나".repeat(1_000)
            } else if user.contains("녹취록:") {
                "가".repeat(2_500)
            } else if sys == SYSTEM {
                "최종".into()
            } else {
                "다".repeat(1_500)
            };
            Ok(Reply { text, ..Default::default() })
        };
        let (req, on_reply) = counter();
        let out = run_summary(&mut call, &|_| {}, &on_reply, "", &t, 4_000).unwrap();
        assert_eq!(out, "최종");
        assert!(biggest <= 4_300, "합칠 때 너무 큰 요청을 보냄: {biggest}자");
        assert_eq!(req.get(), 5, "구간 2 + 줄이기 2 + 최종 1");
    }

    #[test]
    fn rate_limit_waits_then_retries_but_not_for_daily_limits() {
        let mut n = 0;
        let mut call = |_: &str, _: &str, _: &mut dyn FnMut(&str)| {
            n += 1;
            if n == 1 {
                return Err(anyhow::Error::new(ApiError { status: 429, retry_after: Some(0), msg: "rate limit".into() }));
            }
            Ok(Reply { text: "ok".into(), ..Default::default() })
        };
        let (req, on_reply) = counter();
        let out = run_summary(&mut call, &|_| {}, &on_reply, "", "[00:00] a: b\n", 100).unwrap();
        assert_eq!((out.as_str(), req.get()), ("ok", 1), "한도 초과 뒤 다시 보내 성공");
        let mut call = |_: &str, _: &str, _: &mut dyn FnMut(&str)| -> Result<Reply> {
            Err(anyhow::Error::new(ApiError { status: 429, retry_after: Some(3600), msg: "daily limit".into() }))
        };
        let e = run_summary(&mut call, &|_| {}, &|_| {}, "", "[00:00] a: b\n", 100).unwrap_err();
        assert!(e.to_string().contains("429"), "하루 한도는 기다리지 않고 알림: {e}");
    }

    /// 앞 구간이 성공하고 뒤에서 실패해도, 이미 쓴 요청은 사용량에 남아야 한다
    #[test]
    fn usage_is_kept_when_a_later_request_fails() {
        let mut n = 0;
        let mut call = |_: &str, _: &str, _: &mut dyn FnMut(&str)| {
            n += 1;
            if n > 2 {
                return Err(anyhow::Error::new(ApiError { status: 500, retry_after: None, msg: "server error".into() }));
            }
            Ok(Reply { text: "메모".into(), in_tokens: 100, out_tokens: 20, ..Default::default() })
        };
        let (req, on_reply) = counter();
        assert!(run_summary(&mut call, &|_| {}, &on_reply, "", &transcript(200, 60), 4_000).is_err());
        assert_eq!(req.get(), 2, "실패 전에 성공한 두 요청");
    }

    const ANSWER: &str = "### 제목\n출시 일정 회의\n\n### 한 줄 요약\n15일에 출시한다.\n\n### 주요 논의\n- 김 팀장: 일정 확인\n- 이 개발: QA 기간\n\n### 결정 사항\n- 15일 출시\n\n### 할 일\n- [ ] 공지 — 담당: 박 디자인\n\n### 남은 질문\n- 없음\n";

    #[test]
    fn writing_follows_the_sections() {
        let mut w = Writing::new(FINAL_SECTIONS);
        let (mut fracs, mut seen) = (Vec::new(), Vec::new());
        // 몇 글자씩 나눠 받는다 (제목 줄이 조각 사이에서 끊겨도)
        let chars: Vec<char> = ANSWER.chars().collect();
        for piece in chars.chunks(3) {
            if w.push(&piece.iter().collect::<String>()) {
                seen.push(w.section());
            }
            fracs.push(w.frac());
        }
        assert_eq!(seen, ["제목", "한 줄 요약", "주요 논의", "결정 사항", "할 일", "남은 질문"]);
        assert!(fracs.windows(2).all(|p| p[1] >= p[0]), "막대가 되돌아가지 않음");
        let last = *fracs.last().unwrap();
        assert!(last > 0.9 && last < 1.0, "마지막 섹션까지 썼으면 거의 끝: {last}");

        // 다른 모양의 제목 줄 · 본문에 같은 이름이 한 줄로 나와도 되돌아가지 않음
        let mut w = Writing::new(PART_SECTIONS);
        assert!(w.push("## 논의\n- 가\n**결정 사항**:\n"));
        assert_eq!(w.section(), "결정 사항");
        assert!(!w.push("논의\n"));
        assert_eq!(w.section(), "결정 사항");

        // 섹션 제목 없이 쓰는 모델도 조금씩은 오른다 (끝을 모르니 90% 아래)
        let mut w = Writing::new(FINAL_SECTIONS);
        w.push(&"가".repeat(500));
        assert!(w.frac() > 0.1 && w.frac() < 0.9 && w.section().is_empty());
    }

    #[test]
    fn progress_follows_the_answer_being_written() {
        let mut call = |_: &str, _: &str, t: &mut dyn FnMut(&str)| {
            for piece in ["### 제목\n회의\n", "### 주요 논의\n", "- 가\n", "### 할 일\n- [ ] 나\n"] {
                t(piece);
            }
            Ok(Reply { text: "끝".into(), ..Default::default() })
        };
        let seen = std::cell::RefCell::new(Vec::new());
        run_summary(&mut call, &|p| seen.borrow_mut().push(p), &|_| {}, "", "[00:00] a: b\n", 12_000).unwrap();
        let seen = seen.borrow();
        assert!(seen.iter().all(|p| p.writing && (p.done, p.total) == (0, 1)), "{seen:?}");
        let mut sections: Vec<&str> = seen.iter().map(|p| p.section).collect();
        sections.dedup();
        assert_eq!(sections, ["제목", "주요 논의", "할 일"]);
        assert!(seen.windows(2).all(|p| p[1].frac >= p[0].frac));
    }

    #[test]
    fn openai_stream_text_and_usage() {
        let sse = "data: {\"model\":\"gpt-5-mini-2026-08-07\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n\
                   : keep-alive\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"### 논의\\n\"}}]}\n\n\
                   data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"- 가 \"},\"finish_reason\":\"stop\"}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":321,\"completion_tokens\":12}}\n\n\
                   data: [DONE]\n\n";
        let mut got = Vec::new();
        let r = read_openai_stream(sse.as_bytes(), &mut |t| got.push(t.to_string())).unwrap();
        assert_eq!((r.text.as_str(), r.in_tokens, r.out_tokens), ("### 논의\n- 가", 321, 12));
        assert_eq!(r.model, "gpt-5-mini-2026-08-07");
        assert_eq!(got, ["### 논의\n", "- 가 "]);

        // Groq 는 x_groq.usage 에도
        let groq = "data: {\"choices\":[{\"delta\":{\"content\":\"요약\"},\"finish_reason\":\"stop\"}],\"x_groq\":{\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}}\n\ndata: [DONE]\n";
        let r = read_openai_stream(groq.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!((r.in_tokens, r.out_tokens), (9, 2));

        // 도중 오류 · 콘텐츠 필터
        let err = "data: {\"error\":{\"message\":\"rate limited\",\"code\":429}}\n\n";
        let e = read_openai_stream(err.as_bytes(), &mut |_| {}).unwrap_err();
        assert_eq!(e.downcast_ref::<ApiError>().map(|a| a.status), Some(429));
        assert_eq!(wait_for(&e), Some(20), "한도 초과는 기다렸다 다시");
        let filtered = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\ndata: [DONE]\n";
        assert!(read_openai_stream(filtered.as_bytes(), &mut |_| {}).unwrap_err().to_string().contains("콘텐츠 필터"));
    }

    #[test]
    fn anthropic_stream_text_usage_and_refusal() {
        let sse = "event: message_start\n\
                   data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-5-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":1500,\"output_tokens\":1}}}\n\n\
                   event: content_block_start\n\
                   data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
                   event: ping\n\
                   data: {\"type\": \"ping\"}\n\n\
                   event: content_block_delta\n\
                   data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"abc\"}}\n\n\
                   event: content_block_start\n\
                   data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                   event: content_block_delta\n\
                   data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"### 제목\\n\"}}\n\n\
                   event: content_block_delta\n\
                   data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"주간 회의\"}}\n\n\
                   event: message_delta\n\
                   data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":210}}\n\n\
                   event: message_stop\n\
                   data: {\"type\":\"message_stop\"}\n\n";
        let mut got = String::new();
        let r = read_anthropic_stream(sse.as_bytes(), &mut |t| got.push_str(t)).unwrap();
        assert_eq!((r.text.as_str(), r.in_tokens, r.out_tokens), ("### 제목\n주간 회의", 1500, 210));
        assert_eq!((got.as_str(), r.model.as_str()), ("### 제목\n주간 회의", "claude-opus-5-5"));

        // 쓰다가 거절해 다른 모델이 이어 씀 — 글은 이어 붙이고, 사용량은 이어 쓴 모델로
        let fell_back = "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n\
                         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"### 제목\\n\"}}\n\n\
                         data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"fallback\",\"from\":{\"model\":\"claude-opus-5-5\"},\"to\":{\"model\":\"claude-opus-4-8\"}}}\n\n\
                         data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"보안 점검 회의\"}}\n\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":30}}\n\n\
                         data: {\"type\":\"message_stop\"}\n\n";
        let r = read_anthropic_stream(fell_back.as_bytes(), &mut |_| {}).unwrap();
        assert_eq!((r.text.as_str(), r.model.as_str(), r.in_tokens, r.out_tokens), ("### 제목\n보안 점검 회의", "claude-opus-4-8", 12, 30));

        let refused = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n\
                       data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"type\":\"refusal\",\"category\":null,\"explanation\":\"정책\"}},\"usage\":{\"output_tokens\":0}}\n\n\
                       data: {\"type\":\"message_stop\"}\n\n";
        let e = read_anthropic_stream(refused.as_bytes(), &mut |_| {}).unwrap_err().to_string();
        assert!(e.contains("거절") && e.contains("정책"), "{e}");

        let overloaded = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let e = read_anthropic_stream(overloaded.as_bytes(), &mut |_| {}).unwrap_err();
        assert_eq!(e.downcast_ref::<ApiError>().map(|a| a.status), Some(529));
    }

    #[test]
    fn stream_rejections_are_recognized() {
        assert!(mentions_stream(r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#));
        assert!(mentions_stream("'stream' is not supported"));
        assert!(!mentions_stream("upstream connect error"));
        assert!(!mentions_stream("Request too large for model"));
    }

    /// 요청을 하나씩 받아 정해 둔 답을 돌려주는 작은 HTTP 서버 → (주소, 받은 요청 본문)
    fn mock_server(replies: Vec<String>) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for reply in replies {
                let (mut s, _) = l.accept().unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head_end = loop {
                    let n = s.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                    assert!(n > 0, "요청이 중간에 끊김");
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = s.read(&mut chunk).unwrap();
                    assert!(n > 0, "본문이 중간에 끊김");
                    buf.extend_from_slice(&chunk[..n]);
                }
                let _ = tx.send(String::from_utf8_lossy(&buf[head_end..]).to_string());
                s.write_all(reply.as_bytes()).unwrap();
            }
        });
        (base, rx)
    }

    fn http(status: &str, content_type: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn openai_server_without_stream_options_falls_back() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"### 제목\\n\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"주간 회의\"}}]}\n\ndata: [DONE]\n\n";
        let (base, rx) = mock_server(vec![
            http("400 Bad Request", "application/json", r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#),
            http("200 OK", "text/event-stream", sse),
        ]);
        let mut got = String::new();
        let r = openai_compat(&base, None, "m", "s", "u", &mut |t| got.push_str(t)).unwrap();
        assert_eq!((r.text.as_str(), got.as_str()), ("### 제목\n주간 회의", "### 제목\n주간 회의"));
        assert_eq!(r.model, "m", "응답에 모델이 없으면 요청한 모델");
        let first: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        let second: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!((&first["stream"], &first["stream_options"]["include_usage"]), (&json!(true), &json!(true)));
        assert_eq!((&second["stream"], second["stream_options"].is_null()), (&json!(true), true));
    }

    #[test]
    fn openai_server_that_ignores_streaming_still_works() {
        let (base, _rx) = mock_server(vec![http(
            "200 OK",
            "application/json; charset=utf-8",
            r#"{"choices":[{"message":{"content":" 요약 "},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
        )]);
        let mut got = String::new();
        let r = openai_compat(&base, None, "m", "s", "u", &mut |t| got.push_str(t)).unwrap();
        assert_eq!((r.text.as_str(), r.in_tokens, r.out_tokens, got.as_str()), ("요약", 7, 3, "요약"));
    }

    /// 실제 HTTP 로 받는 Ollama 답 → 섹션마다 진행 · 제목 · 사용량까지
    #[test]
    fn summarize_reports_sections_from_a_streaming_server() {
        let mut nd: String = ANSWER
            .split_inclusive('\n')
            .map(|t| json!({ "message": { "content": t }, "done": false }).to_string() + "\n")
            .collect();
        nd += &(json!({ "message": { "content": "" }, "done": true, "prompt_eval_count": 50, "eval_count": 20 }).to_string() + "\n");
        let (base, rx) = mock_server(vec![http("200 OK", "application/x-ndjson", &nd)]);
        let cfg = SummarySettings { provider: "ollama".into(), ollama_url: base, ..Default::default() };
        let doc: SessionDoc = serde_json::from_value(json!({
            "version": 1, "id": "t", "title": "t", "startedAt": "2026-11-16 09:00", "durationMs": 1000, "engine": "x",
            "utterances": [ { "id": 0, "startMs": 0, "endMs": 1000, "speaker": 0, "text": "안녕하세요" } ]
        }))
        .unwrap();
        let seen = std::cell::RefCell::new(Vec::new());
        let used = std::cell::Cell::new((0, 0));
        let flag = std::sync::atomic::AtomicBool::new(false);
        let s = summarize(&cfg, &doc, &|p| seen.borrow_mut().push(p), &|_, r| used.set((r.in_tokens, r.out_tokens)), &flag).unwrap();
        assert_eq!(s.title.as_deref(), Some("출시 일정 회의"));
        assert!(s.text.starts_with("### 한 줄 요약"));
        assert_eq!(used.get(), (50, 20));
        let sent: Value = serde_json::from_str(&rx.recv().unwrap()).unwrap();
        assert_eq!(sent["stream"], json!(true));
        let mut sections: Vec<&str> = seen.borrow().iter().map(|p| p.section).collect();
        sections.dedup();
        assert_eq!(sections, ["제목", "한 줄 요약", "주요 논의", "결정 사항", "할 일", "남은 질문"]);
    }
}
