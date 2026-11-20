//! 회의에 묻기 — 요약에 쓰는 LLM(설정 › 요약)으로 이 회의 내용을 묻고 답을 받는다
//!
//! 회의 기록(제목 · 화자 · AI 요약 · 메모 · 녹취록)을 한 덩어리로 앞에 두고, 앞선 질문 · 답과 새 질문을 잇는다.
//! - Claude 는 회의 기록 덩어리를 프롬프트 캐시에 둬서, 몇 분 안에 다시 물으면 녹취록을 싸게 다시 읽는다
//! - 회의 기록이 제공자별 한 번에 보낼 크기([`summary::chunk_budget`])를 넘으면 녹취록은 질문과 겹치는 말이 많은
//!   문단과 그 앞뒤를 골라 싣고, 남는 자리는 회의 전체에서 고르게 채운다 (요약 · 메모는 늘 싣는다)
//! - 그래도 너무 크다고 거절되면 반씩 줄여 다시 보낸다
//! - 지금까지 따라잡기: 받아 적은 마지막 말에서 몇 분 앞까지만 녹취록에 싣고, 앞선 질문 · 답 없이 정리하라는
//!   지시([`prompts::catch_up`])를 보낸다 — 회의가 길어도 그 구간만 보내 빠르고 싸다

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Result};

use crate::prompts;
use crate::session::{fmt_time, SessionDoc};
use crate::settings::SummarySettings;
use crate::summary::{self, Reply, Turn};

/// 앞선 질문 · 답을 이만큼까지 이어 보낸다 (최근 것)
const HISTORY: usize = 6;
/// 앞선 답 하나는 이 글자까지만 이어 보낸다
const HISTORY_ANSWER_CHARS: usize = 1_500;
/// 앞선 질문 하나는 이 글자까지만
const HISTORY_QUESTION_CHARS: usize = 500;
/// 너무 크다고 거절되면 반씩 줄이되 이보다 작게는 줄이지 않는다
const MIN_BUDGET: usize = 2_000;

/// 묻는 동안 화면에 알릴 것
#[derive(Debug, PartialEq)]
pub enum Event<'a> {
    /// 답 글 조각
    Text(&'a str),
    /// 받던 답을 버리고 처음부터 다시 받는다 (너무 크다고 거절 · 한도 초과로 다시 보냄)
    Restart,
    /// 요청 한도 때문에 이만큼(초) 기다렸다 다시 보낸다 (0 이면 다시 보냄)
    Wait(u64),
}

/// 받은 답
#[derive(Debug)]
pub struct Answer {
    pub text: String,
    pub model: String,
    /// 회의가 길어 녹취록 일부만 보고 답했다
    pub partial: bool,
}

/// 앞에 두는 회의 기록 덩어리
pub(crate) struct Context {
    pub(crate) text: String,
    partial: bool,
}

/// 두 글자 묶음 (글자 · 숫자가 이어진 곳 안에서, 소문자로) — 한국어는 조사가 붙어도 앞 두 글자가 겹친다
pub(crate) fn grams(s: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for word in s.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = word.to_lowercase().chars().collect();
        for w in chars.windows(2) {
            out.insert(w.iter().collect());
        }
    }
    out
}

/// 녹취록 문단 중 실을 것 (시간 순 번호). 질문과 겹치는 말이 많은(드문 말일수록 무겁게) 문단 → 그 앞뒤 →
/// 남는 자리는 회의 전체에서 고르게. 다 합쳐 `budget` 글자 안쪽.
/// `keep_end` 면 맨 먼저 끝(가장 최근 말)부터 자리의 절반까지 이어서 싣는다 — 따라잡기는 답의 첫 줄이 "지금 무슨
/// 이야기 중"이라, 고르게 솎다가 마지막 말이 빠지면 지금 바뀐 주제 · 방금 정한 것을 놓친다
fn pick(lines: &[&str], query: &str, budget: usize, keep_end: bool) -> Vec<usize> {
    let n = lines.len();
    let sets: Vec<HashSet<String>> = lines.iter().map(|l| grams(l)).collect();
    let q = grams(query);
    let mut df: HashMap<&str, usize> = HashMap::new();
    for g in &q {
        df.insert(g, sets.iter().filter(|s| s.contains(g)).count());
    }
    // 모든 문단에 있는 말(조사 · "라고" 같은 것)은 0 에 가깝게
    let idf = |g: &str| ((n as f64 + 1.0) / (df[g] as f64 + 1.0)).ln();
    let score: Vec<f64> = sets.iter().map(|s| q.iter().filter(|g| s.contains(*g)).map(|g| idf(g)).sum()).collect();

    let len = |i: usize| lines[i].chars().count() + 1;
    let mut chosen = vec![false; n];
    let mut used = 0;
    let mut take = |i: usize, chosen: &mut Vec<bool>| -> bool {
        if chosen[i] {
            return true;
        }
        if used + len(i) > budget {
            return false;
        }
        chosen[i] = true;
        used += len(i);
        true
    };
    if keep_end {
        let mut start = n;
        let mut tail = 0;
        while start > 0 && tail + len(start - 1) <= budget / 2 {
            start -= 1;
            tail += len(start);
        }
        for i in start..n {
            take(i, &mut chosen);
        }
    }
    let mut order: Vec<usize> = (0..n).filter(|&i| score[i] > 1e-9).collect();
    order.sort_by(|&a, &b| score[b].total_cmp(&score[a]).then(a.cmp(&b)));
    for i in order {
        if !take(i, &mut chosen) {
            continue;
        }
        // 앞뒤 문단 — 묻는 말에 답하는 말이 바로 다음에 오기 쉽다
        if i + 1 < n {
            take(i + 1, &mut chosen);
        }
        if i > 0 {
            take(i - 1, &mut chosen);
        }
    }
    // 남는 자리: 회의 흐름을 알 수 있게 나머지에서 고르게
    let rest: Vec<usize> = (0..n).filter(|&i| !chosen[i]).collect();
    let rest_len: usize = rest.iter().map(|&i| len(i)).sum();
    let room = budget.saturating_sub(chosen.iter().enumerate().filter(|(_, c)| **c).map(|(i, _)| len(i)).sum());
    if !rest.is_empty() && room > 0 {
        let step = rest_len.div_ceil(room).max(1);
        for (k, &i) in rest.iter().enumerate() {
            if k % step == 0 {
                take(i, &mut chosen);
            }
        }
    }
    (0..n).filter(|&i| chosen[i]).collect()
}

/// 골라 실을 녹취록 조각 하나는 아무리 작아도 이만큼(글자)까지는 담는다
const MIN_UNIT: usize = 200;

/// 골라 실을 녹취록 조각: 문장(발화)마다 "[시각] 화자: 글" 한 줄, `cap` 글자를 넘으면 잘라서 (자른 조각에도 같은 시각 · 화자)
fn units(doc: &SessionDoc, cap: usize) -> Vec<String> {
    let mut us: Vec<_> = doc.utterances.iter().filter(|u| !u.text.trim().is_empty()).collect();
    us.sort_by_key(|u| (u.start_ms, u.id));
    let mut out = Vec::new();
    for u in us {
        let head = format!("[{}] {}: ", fmt_time(u.start_ms), doc.speaker_name(u.speaker));
        let room = cap.saturating_sub(head.chars().count()).max(MIN_UNIT / 2);
        let chars: Vec<char> = u.text.trim().chars().collect();
        for (k, piece) in chars.chunks(room).enumerate() {
            let piece: String = piece.iter().collect();
            out.push(if k == 0 { format!("{head}{piece}") } else { format!("{head}(이어서) {piece}") });
        }
    }
    out
}

/// 따라잡기로 보낼 것: 그 구간 문장만 남긴 회의 기록, 녹취록이 시작하는 시각(앞을 잘랐을 때), 정리하라는 지시
struct CatchUp {
    doc: SessionDoc,
    from: Option<u64>,
    text: String,
}

/// 받아 적은 마지막 말에서 `mins` 분 앞부터 (그 시각에 걸친 문장까지). 회의가 그보다 짧으면 처음부터
fn catch_up(doc: &SessionDoc, mins: u32) -> Result<CatchUp> {
    let said = |u: &crate::session::Utterance| !u.text.trim().is_empty();
    let end = doc.utterances.iter().filter(|u| said(u)).map(|u| u.end_ms).max();
    let Some(end) = end else { bail!("아직 받아 적은 말이 없어요") };
    let mins = mins.max(1);
    let cut = end.saturating_sub(mins as u64 * 60_000);
    let mut d = doc.clone();
    d.utterances.retain(|u| said(u) && u.end_ms > cut);
    let whole = d.utterances.len() == doc.utterances.iter().filter(|u| said(u)).count();
    let from = d.utterances.iter().map(|u| u.start_ms).min().filter(|_| !whole);
    let range = match from {
        Some(ms) => format!("지난 {mins}분([{}]부터) 동안", fmt_time(ms)),
        None => "회의 처음부터 지금까지".to_string(),
    };
    Ok(CatchUp { doc: d, from, text: prompts::catch_up().replace("{범위}", &range) })
}

/// 녹취록이 길어 골라 실을 때 무엇에 맞추나
pub(crate) enum Focus<'a> {
    /// 질문 — 질문과 겹치는 말이 많은 곳과 그 앞뒤 위주로
    Question(&'a str),
    /// 따라잡기 — 끝(가장 최근 말)을 먼저. `from` 이 있으면 녹취록이 그 시각부터라고 밝힌다(앞부분은 일부러 뺐다)
    Recent { from: Option<u64> },
}

/// 회의 기록 덩어리. 다 실으면 `budget` 글자를 넘을 때 녹취록은 `focus` 에 맞춰 골라 싣는다
pub(crate) fn context(doc: &SessionDoc, focus: &Focus, budget: usize) -> Context {
    let (query, from, recent) = match *focus {
        Focus::Question(q) => (q, None, false),
        Focus::Recent { from } => ("", from, true),
    };
    let mut head = format!("회의 제목: {}\n일시: {}", doc.title, doc.started_at);
    if doc.duration_ms > 0 {
        head.push_str(&format!(" · 길이 {}", fmt_time(doc.duration_ms)));
    }
    let names = doc.speaker_names();
    if !names.is_empty() {
        head.push_str(&format!("\n화자: {}", names.join(", ")));
    }
    head.push_str("\n\n");
    // 요약 · 메모도 자리에 맞춰 — 직접 고친 요약이나 메모가 아주 길면 그것만으로 한 번에 보낼 크기를 넘어,
    // 너무 크다고 거절돼 자리를 줄여 다시 보내도 계속 거절된다 (자리가 줄면 함께 준다)
    if let Some(sum) = doc.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        head.push_str(&format!("## AI 요약 (참고용)\n{}\n\n", clip(sum.trim(), (budget / 3).max(MIN_UNIT))));
    }
    if !doc.notes.trim().is_empty() {
        head.push_str(&format!(
            "## 메모 (회의 중 사람이 적은 것 — [12:34] 로 시작하는 줄은 그 시각 문장에 단 코멘트)\n{}\n\n",
            clip(doc.notes.trim(), (budget / 5).max(MIN_UNIT))
        ));
    }
    let transcript = doc.to_text();
    if transcript.trim().is_empty() {
        head.push_str("## 녹취록\n(아직 받아 적은 문장이 없어요)\n");
        return Context { text: head, partial: false };
    }
    let since = from.map(|ms| format!("[{}]부터", fmt_time(ms)));
    let head_len = head.chars().count();
    if head_len + transcript.chars().count() <= budget {
        let title = match &since {
            Some(s) => format!("## 녹취록 ({s} — 그 앞은 싣지 않았어요)"),
            None => "## 녹취록".to_string(),
        };
        return Context { text: format!("{head}{title}\n{transcript}"), partial: false };
    }
    // 요약 · 메모가 아주 길어도 녹취록 자리는 절반은 남긴다
    let room = budget.saturating_sub(head_len).max(budget / 2);
    // 고를 때는 문단이 아니라 문장(발화) 단위로, 그래도 긴 것은 잘라서 — 한 사람이 길게 이어 말한 문단이
    // 자리보다 크면 통째로 빠져 녹취록이 하나도 실리지 않는다
    let owned = units(doc, (room / 4).max(MIN_UNIT));
    let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
    let keep = pick(&lines, query, room, recent);
    let mut body = String::new();
    let mut next = 0;
    for &i in &keep {
        if i > next {
            body.push_str("…\n");
        }
        body.push_str(lines[i]);
        body.push('\n');
        next = i + 1;
    }
    if next < lines.len() {
        body.push_str("…\n");
    }
    let title = match (&since, recent) {
        (Some(s), _) => format!(
            "## 녹취록 ({s}, 그 가운데 일부 — 길어서 가장 최근 말을 먼저, 나머지는 고르게 골라 실었어요. '…' 줄은 건너뛴 곳)"
        ),
        (None, true) => {
            "## 녹취록 (일부 — 길어서 가장 최근 말을 먼저, 나머지는 고르게 골라 실었어요. '…' 줄은 건너뛴 곳)".to_string()
        }
        (None, false) => {
            "## 녹취록 (일부 — 회의가 길어 질문과 관련 있어 보이는 부분 위주로 골라 실었어요. '…' 줄은 건너뛴 곳)"
                .to_string()
        }
    };
    Context { text: format!("{head}{title}\n{body}"), partial: true }
}

/// 앞 `n` 글자 (잘랐으면 끝에 …)
fn clip(s: &str, n: usize) -> String {
    let mut c: String = s.chars().take(n).collect();
    if c.len() < s.len() {
        c.push('…');
    }
    c
}

/// 앞선 질문 · 답 중 최근 것부터 합쳐 `cap` 글자 안쪽까지 (오래된 것부터 차례로). 가장 최근 답은 넘치면
/// 잘라서라도 넣는다 — 이어 묻는 말("그건 누가 해?")은 바로 앞 답을 가리키기 쉽다
pub(crate) fn recent_within(past: &[(String, String)], cap: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut used = 0;
    for (q, a) in past.iter().rev() {
        let n = q.chars().count() + a.chars().count();
        if used + n > cap {
            let room = cap.saturating_sub(q.chars().count());
            if out.is_empty() && room >= 100 {
                out.push((q.clone(), clip(a, room)));
            }
            break;
        }
        used += n;
        out.push((q.clone(), a.clone()));
    }
    out.reverse();
    out
}

/// 이 회의에 묻는다. 답은 받는 대로 `on_event` 로, 사용량은 요청이 성공할 때마다 `on_usage(제공자, 응답)` 로.
/// `recent_mins` 가 있으면 지금까지 따라잡기 — 받아 적은 마지막 말에서 그 분 앞까지만 정리한다
/// (`question` 은 기록에 남길 이름 — "지난 10분 정리" — 이고 모델에게는 따라잡기 지시가 간다)
#[allow(clippy::too_many_arguments)]
pub fn ask(
    cfg: &SummarySettings,
    doc: &SessionDoc,
    question: &str,
    recent_mins: Option<u32>,
    on_event: &mut dyn FnMut(Event),
    on_usage: &dyn Fn(&str, &Reply),
    cancel: &AtomicBool,
) -> Result<Answer> {
    let (provider, mut chat) = summary::chat_caller(cfg)?;
    let budget = summary::chunk_budget(&provider);
    ask_with(&mut *chat, &provider, budget, doc, question, recent_mins, on_event, on_usage, cancel)
}

#[allow(clippy::too_many_arguments)]
fn ask_with(
    chat: &mut dyn FnMut(&str, &[Turn], &mut dyn FnMut(&str)) -> Result<Reply>,
    provider: &str,
    mut budget: usize,
    doc: &SessionDoc,
    question: &str,
    recent_mins: Option<u32>,
    on_event: &mut dyn FnMut(Event),
    on_usage: &dyn Fn(&str, &Reply),
    cancel: &AtomicBool,
) -> Result<Answer> {
    let question = question.trim();
    if question.is_empty() {
        bail!("물어볼 내용을 적어 주세요");
    }
    if !doc.has_content() {
        bail!("아직 물어볼 회의 내용이 없어요");
    }
    // 따라잡기: 그 구간만 남긴 회의 기록 + 정리하라는 지시. 앞선 질문 · 답은 잇지 않는다 (따로 서는 정리 —
    // 보낼 글이 작아 빠르고 싸다)
    let catch = recent_mins.map(|m| catch_up(doc, m)).transpose()?;
    let (doc, question) = match &catch {
        Some(c) => (&c.doc, c.text.as_str()),
        None => (doc, question),
    };
    // 앞선 질문 · 답 (최근 것) — "그건 누가 하기로 했어?" 처럼 이어 묻는 말을 알아듣게
    let all_past: Vec<(String, String)> = if catch.is_some() {
        Vec::new()
    } else {
        doc.qa
            .iter()
            .skip(doc.qa.len().saturating_sub(HISTORY))
            .map(|x| (clip(&x.q, HISTORY_QUESTION_CHARS), clip(&x.a, HISTORY_ANSWER_CHARS)))
            .collect()
    };
    // 녹취록이 길면: 질문은 바로 앞 질문도 함께 보고 겹치는 곳 위주로, 따라잡기는 가장 최근 말부터
    let query = match all_past.last() {
        Some((q, _)) => format!("{question}\n{q}"),
        None => question.to_string(),
    };
    let focus = match &catch {
        Some(c) => Focus::Recent { from: c.from },
        None => Focus::Question(&query),
    };
    let mut waits = 0;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(summary::AskCancelled));
        }
        // 앞선 대화도 한 번에 보낼 크기 안에서 — 자리의 1/3 까지 최근 것부터, 나머지를 회의 기록에
        // (너무 크다고 거절돼 자리를 줄이면 앞선 대화도 함께 준다)
        let past = recent_within(&all_past, budget / 3);
        let talk = past.iter().map(|(q, a)| q.chars().count() + a.chars().count()).sum::<usize>()
            + question.chars().count();
        let ctx = context(doc, &focus, budget.saturating_sub(talk).max(budget / 4));
        let head = format!("다음은 이 회의의 기록입니다.\n\n{}", ctx.text);
        // 따라잡기 구간은 다시 쓸 일이 드물어 캐시에 두지 않는다 (캐시에 쓰는 값이 더 든다)
        let mut turns = vec![Turn { user: true, text: &head, cache: catch.is_none() }];
        for (q, a) in &past {
            turns.push(Turn::user(q));
            turns.push(Turn { user: false, text: a, cache: false });
        }
        turns.push(Turn::user(question));
        let wrote = Cell::new(false);
        let res = chat(prompts::ask(), &turns, &mut |t| {
            wrote.set(true);
            on_event(Event::Text(t));
        });
        let e = match res {
            Ok(r) => {
                on_usage(provider, &r);
                let text = r.text.trim().to_string();
                if text.is_empty() {
                    bail!("답이 비어 있어요. 다시 물어봐 주세요");
                }
                return Ok(Answer { text, model: r.model, partial: ctx.partial });
            }
            Err(e) => e,
        };
        if wrote.get() {
            on_event(Event::Restart);
        }
        if summary::too_large(&e) && budget > MIN_BUDGET {
            budget = (budget / 2).max(MIN_BUDGET);
            log::info!("묻기 요청이 너무 크다고 거절됨 — 회의 기록을 {budget}자로 줄여 다시");
            continue;
        }
        match summary::wait_for(&e) {
            Some(w) if w <= summary::MAX_WAIT_SECS && waits < 4 => {
                waits += 1;
                log::info!("묻기 요청 한도 — {w}초 기다렸다 다시");
                on_event(Event::Wait(w));
                for _ in 0..=w {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(anyhow::Error::new(summary::AskCancelled));
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                on_event(Event::Wait(0));
            }
            _ => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::QaItem;
    use crate::summary::ApiError;
    use serde_json::json;

    fn doc(lines: &[(u64, u32, &str)]) -> SessionDoc {
        let utterances: Vec<_> = lines
            .iter()
            .enumerate()
            .map(|(i, (s, spk, t))| json!({ "id": i, "startMs": s * 1000, "endMs": s * 1000 + 3000, "speaker": spk, "text": t }))
            .collect();
        serde_json::from_value(json!({
            "version": 1, "id": "t", "title": "AI 리포트 회의", "startedAt": "2026-11-19 10:00", "durationMs": 600_000, "engine": "x",
            "speakers": { "0": "이낙원 실장님", "1": "김성현 매니저" },
            "utterances": utterances
        }))
        .unwrap()
    }

    fn meeting() -> SessionDoc {
        // 화자가 번갈아 말해 문단이 문장마다 나뉜다
        let mut v = Vec::new();
        for i in 0..80u64 {
            v.push((i * 20, (i % 2) as u32, "오늘은 리포트 화면 배치와 일정 이야기를 이어서 했습니다"));
        }
        v[25] = (500, 1, "VPN 은 월 12만 원에 도입했고 트래픽에 따라 비용이 달라질 수 있어요");
        v[26] = (520, 0, "그럼 비용은 다음 달에 다시 보죠");
        doc(&v)
    }

    #[test]
    fn short_meetings_are_sent_whole() {
        let d = meeting();
        let c = context(&d, &Focus::Question("VPN 비용"), 1_000_000);
        assert!(!c.partial);
        assert!(c.text.contains("화자: 김성현 매니저, 이낙원 실장님") || c.text.contains("화자: 이낙원 실장님, 김성현 매니저"));
        assert!(c.text.contains("[08:20] 김성현 매니저: VPN 은 월 12만 원"));
        assert!(!c.text.contains('…'));
    }

    #[test]
    fn long_meetings_keep_the_lines_that_match_the_question() {
        let d = meeting();
        let c = context(&d, &Focus::Question("VPN 비용이 얼마였지?"), 600);
        assert!(c.partial);
        assert!(c.text.contains("VPN 은 월 12만 원"), "{}", c.text);
        // 바로 뒤 문단(답하는 말)도 함께
        assert!(c.text.contains("그럼 비용은 다음 달에"), "{}", c.text);
        assert!(c.text.contains("…\n"));
        // 녹취록 부분이 자리 안쪽
        let body = c.text.split("## 녹취록").nth(1).unwrap();
        assert!(body.chars().count() < 600 + 200, "{}", body.chars().count());
    }

    /// 아주 긴 요약 · 메모도 자리에 맞춰 잘린다 (메모만 있는 회의도) — 줄여 다시 보내면 함께 준다
    #[test]
    fn long_summaries_and_notes_are_bounded() {
        let mut d = meeting();
        d.summary = Some("요약 ".repeat(3_000));
        d.notes = "메모 ".repeat(3_000);
        for budget in [4_000, 2_000] {
            let n = context(&d, &Focus::Question("VPN"), budget).text.chars().count();
            assert!(n <= budget + 300, "{budget}: {n}");
        }
        let mut only_notes = doc(&[]);
        only_notes.notes = "메모 ".repeat(3_000);
        let c = context(&only_notes, &Focus::Question("VPN"), 2_000);
        assert!(c.text.chars().count() <= 2_000 && c.text.contains("메모 메모"), "{}", c.text.chars().count());
    }

    /// 한 사람이 쉬지 않고 길게 말해 문단 하나가 자리보다 커도 녹취록이 통째로 빠지지 않는다
    #[test]
    fn one_long_speech_is_cut_into_pieces_not_dropped() {
        let mut v = Vec::new();
        for i in 0..60u64 {
            v.push((i * 5, 0, "오늘 발표는 매출 분석 화면 구성과 일정에 관한 이야기입니다"));
        }
        v[40] = (200, 0, "VPN 은 월 12만 원에 도입했습니다");
        let d = doc(&v);
        assert_eq!(d.to_text().lines().count(), 1, "문단 하나로 묶인다");
        let c = context(&d, &Focus::Question("VPN 비용은?"), 800);
        assert!(c.partial);
        assert!(c.text.contains("[03:20] 이낙원 실장님: VPN 은 월 12만 원"), "{}", c.text);

        // 나뉘지 않은 아주 긴 발화 하나도 잘라서, 질문과 맞는 조각을 싣는다
        let long = format!("{}VPN 은 월 12만 원{}", "가".repeat(3000), "나".repeat(3000));
        let d = doc(&[(0, 0, long.as_str())]);
        let c = context(&d, &Focus::Question("VPN 비용은?"), 800);
        let body = c.text.split("## 녹취록").nth(1).unwrap();
        assert!(body.contains("VP"), "{body}");
        assert!(body.contains("(이어서)"));
        assert!(body.chars().count() < 800, "{}", body.chars().count());
    }

    #[test]
    fn picked_lines_stay_in_order_and_fill_the_room_evenly() {
        let lines: Vec<String> = (0..20).map(|i| format!("[00:{i:02}] 화자: 일반 이야기 {i}")).collect();
        let mut lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        lines[10] = "[00:10] 화자: 예산 승인은 금요일까지";
        let keep = pick(&lines, "예산 승인 언제?", 140, false);
        assert!(keep.windows(2).all(|w| w[0] < w[1]));
        assert!(keep.contains(&10) && keep.contains(&9) && keep.contains(&11), "{keep:?}");
        let used: usize = keep.iter().map(|&i| lines[i].chars().count() + 1).sum();
        assert!(used <= 140);
        // 맞는 말이 없어도 처음 몇 줄만이 아니라 회의 전체에서 고른다
        let keep = pick(&lines, "zzz", 100, false);
        assert!(keep.first() == Some(&0) && *keep.last().unwrap() >= 10, "{keep:?}");
    }

    /// 따라잡기: 자리가 모자라면 가장 최근 말부터 자리의 절반을 이어서 싣고, 남는 자리는 앞에서 고르게
    #[test]
    fn catch_up_keeps_the_latest_lines_when_short_of_room() {
        let lines: Vec<String> = (0..30).map(|i| format!("[00:{i:02}] 화자: 같은 길이의 이야기")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let each = lines[0].chars().count() + 1;
        // 고르게만 솎으면 0, 2, …, 28 이 남아 마지막 말(29)이 빠진다
        assert!(!pick(&lines, "", each * 15, false).contains(&29));
        let keep = pick(&lines, "", each * 15, true);
        assert!((23..30).all(|i| keep.contains(&i)), "끝 일곱 줄이 이어서 {keep:?}");
        assert!(keep.contains(&0) && keep.len() == 15, "남는 자리는 앞에서 고르게 {keep:?}");
        assert!(keep.windows(2).all(|w| w[0] < w[1]));
    }

    fn reply(text: &str) -> Result<Reply> {
        Ok(Reply { text: text.into(), model: "m".into(), in_tokens: 5, out_tokens: 2, ..Default::default() })
    }

    #[test]
    fn asks_with_the_record_first_then_past_questions() {
        let mut d = meeting();
        d.qa.push(QaItem { q: "VPN 은 얼마야?".into(), a: "월 12만 원이에요 [08:20]".into(), ..Default::default() });
        let mut seen: Vec<(bool, String, bool)> = Vec::new();
        let mut chat = |s: &str, turns: &[Turn], t: &mut dyn FnMut(&str)| {
            assert_eq!(s, prompts::ask());
            seen = turns.iter().map(|x| (x.user, x.text.to_string(), x.cache)).collect();
            t("다음 달에 ");
            t("다시 봐요 [08:40]");
            reply("다음 달에 다시 봐요 [08:40]")
        };
        let mut got = String::new();
        let used = Cell::new(0);
        let a = ask_with(
            &mut chat,
            "anthropic",
            100_000,
            &d,
            "  그건 언제 다시 봐?  ",
            None,
            &mut |e| {
                if let Event::Text(t) = e {
                    got.push_str(t)
                }
            },
            &|p, r| {
                assert_eq!(p, "anthropic");
                used.set(r.in_tokens)
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((a.text.as_str(), a.model.as_str(), a.partial), ("다음 달에 다시 봐요 [08:40]", "m", false));
        assert_eq!((got.as_str(), used.get()), ("다음 달에 다시 봐요 [08:40]", 5));
        assert_eq!(seen.len(), 4);
        assert!(seen[0].0 && seen[0].2 && seen[0].1.contains("## 녹취록"), "회의 기록이 맨 앞, 캐시에");
        assert_eq!((seen[1].0, seen[1].1.as_str()), (true, "VPN 은 얼마야?"));
        assert_eq!((seen[2].0, seen[2].2), (false, false));
        assert_eq!((seen[3].0, seen[3].1.as_str(), seen[3].2), (true, "그건 언제 다시 봐?", false));
    }

    /// 앞선 질문 · 답이 길어도 요청 전체가 한 번에 보낼 크기 안쪽 — 최근 것부터 남고, 줄여 다시 보내면 함께 준다
    #[test]
    fn past_answers_fit_in_the_request_budget() {
        let mut d = meeting();
        for i in 0..6 {
            d.qa.push(QaItem { q: format!("질문 {i}"), a: "긴 답 ".repeat(100), ..Default::default() });
        }
        let total = |turns: &[Turn]| turns.iter().map(|t| t.text.chars().count()).sum::<usize>();
        let mut sent = Vec::new();
        let mut chat = |_: &str, turns: &[Turn], _: &mut dyn FnMut(&str)| {
            sent.push((total(turns), turns.iter().filter(|t| !t.user).count(), turns[turns.len() - 3].text.to_string()));
            if sent.len() == 1 {
                return Err(anyhow::Error::new(ApiError { status: 413, retry_after: None, msg: "Request too large".into() }));
            }
            reply("답")
        };
        ask_with(&mut chat, "groq", 4_000, &d, "VPN 비용?", None, &mut |_| {}, &|_, _| {}, &AtomicBool::new(false)).unwrap();
        // 첫 요청: 4천 자 자리 안쪽(머리말 · '…' 줄 몇 개 여유), 앞선 답은 1/3 자리만큼 최근 것만
        assert!(sent[0].0 <= 4_000 + 300, "{}", sent[0].0);
        assert!(sent[0].1 >= 1 && sent[0].1 < 6, "{}", sent[0].1);
        assert_eq!(sent[0].2, "질문 5", "가장 최근 질문이 마지막 앞선 대화");
        // 다시 보낼 때는 앞선 대화까지 함께 줄어든다
        assert!(sent[1].0 <= 2_000 + 300 && sent[1].1 < sent[0].1, "{:?}", (sent[1].0, sent[1].1));

        // 가장 최근 답 하나가 자리보다 길면 잘라서라도 넣는다
        let past = vec![("앞 질문".to_string(), "가".repeat(100)), ("바로 앞 질문".to_string(), "나".repeat(2_000))];
        let kept = recent_within(&past, 500);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "바로 앞 질문");
        assert!(kept[0].1.chars().count() <= 500 && kept[0].1.ends_with('…'));
    }

    #[test]
    fn too_large_requests_shrink_and_start_over() {
        let d = meeting();
        let mut sizes = Vec::new();
        let mut chat = |_: &str, turns: &[Turn], t: &mut dyn FnMut(&str)| {
            sizes.push(turns[0].text.chars().count());
            if sizes.len() == 1 {
                t("받다가");
                return Err(anyhow::Error::new(ApiError { status: 413, retry_after: None, msg: "Request too large".into() }));
            }
            reply("월 12만 원이에요")
        };
        let mut events = Vec::new();
        let a = ask_with(
            &mut chat,
            "groq",
            4_000,
            &d,
            "VPN 비용?",
            None,
            &mut |e| events.push(format!("{e:?}")),
            &|_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(a.partial);
        assert_eq!(sizes.len(), 2);
        assert!(sizes[1] < sizes[0], "{sizes:?}");
        assert_eq!(events, ["Text(\"받다가\")", "Restart"]);
    }

    #[test]
    fn cancelled_or_empty_questions_send_nothing() {
        let d = meeting();
        let mut calls = 0;
        let mut chat = |_: &str, _: &[Turn], _: &mut dyn FnMut(&str)| {
            calls += 1;
            reply("x")
        };
        let e = ask_with(&mut chat, "groq", 4_000, &d, "질문", None, &mut |_| {}, &|_, _| {}, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(e.to_string(), summary::ASK_CANCELLED_MSG);
        let e = ask_with(&mut chat, "groq", 4_000, &d, "   ", None, &mut |_| {}, &|_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(e.to_string().contains("물어볼 내용"));
        let empty = doc(&[]);
        assert!(ask_with(&mut chat, "groq", 4_000, &empty, "질문", None, &mut |_| {}, &|_, _| {}, &AtomicBool::new(false)).is_err());
        assert_eq!(calls, 0);
    }

    /// 따라잡기: 마지막 말에서 10분 앞부터만 싣고, 앞선 질문 · 답 없이 따라잡기 지시로 묻는다
    #[test]
    fn catch_up_sends_only_the_recent_part() {
        // 20초마다 한 문장, 마지막 문장은 26:20 ~ 26:23
        let mut d = meeting();
        d.qa.push(QaItem { q: "VPN 은 얼마야?".into(), a: "월 12만 원이에요 [08:20]".into(), ..Default::default() });
        let seen: std::cell::RefCell<Vec<(String, bool)>> = Default::default();
        let mut chat = |s: &str, turns: &[Turn], _: &mut dyn FnMut(&str)| {
            assert_eq!(s, prompts::ask(), "시스템 프롬프트는 회의에 묻기와 같다 (근거 시각 · 화자 규칙)");
            *seen.borrow_mut() = turns.iter().map(|t| (t.text.to_string(), t.cache)).collect();
            reply("**지금**: 일정 이야기 중 [26:20]")
        };
        let a = ask_with(
            &mut chat,
            "anthropic",
            100_000,
            &d,
            "지난 10분 정리",
            Some(10),
            &mut |_| {},
            &|_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((a.text.as_str(), a.partial), ("**지금**: 일정 이야기 중 [26:20]", false));
        assert_eq!(seen.borrow().len(), 2, "회의 기록 + 지시만 — 앞선 질문 · 답은 잇지 않는다");
        let (record, cache) = seen.borrow()[0].clone();
        assert!(!cache, "따라잡기 구간은 캐시에 두지 않는다");
        // 26:23 에서 10분 앞(16:23) 뒤에 끝나는 문장부터: 16:20 문장(16:23 에 끝남)은 빠지고 16:40 부터
        assert!(record.contains("## 녹취록 ([16:40]부터 — 그 앞은 싣지 않았어요)"), "{record}");
        assert!(record.contains("[16:40] ") && record.contains("[26:20] "), "{record}");
        assert!(!record.contains("[16:20] ") && !record.contains("VPN 은 월 12만 원"), "{record}");
        let ask = seen.borrow()[1].0.clone();
        assert!(ask.starts_with("회의에 늦게 들어왔거나") && ask.contains("지난 10분([16:40]부터) 동안"), "{ask}");
        assert!(!ask.contains("{범위}") && !ask.contains("지난 10분 정리"), "기록에 남길 이름은 보내지 않는다");

        // 회의가 10분보다 짧으면 처음부터 다
        let short = doc(&[(0, 0, "시작할게요"), (90, 1, "네 오늘 안건은 두 가지예요")]);
        ask_with(&mut chat, "groq", 4_000, &short, "지난 10분 정리", Some(10), &mut |_| {}, &|_, _| {}, &AtomicBool::new(false))
            .unwrap();
        assert!(seen.borrow()[0].0.contains("## 녹취록\n[00:00] 이낙원 실장님: 시작할게요"), "{}", seen.borrow()[0].0);
        assert!(seen.borrow()[1].0.contains("회의 처음부터 지금까지 오간"), "{}", seen.borrow()[1].0);

        // 지난 10분이 한 번에 보낼 크기(Groq 4천 자)보다 길어도 마지막 말은 꼭 실린다
        let mut v = Vec::new();
        for i in 0..400u64 {
            v.push((i * 2, (i % 2) as u32, "오늘은 리포트 화면 배치와 일정 이야기를 이어서 했습니다"));
        }
        v[399] = (798, 1, "방금 정했어요 배포는 다음 주 화요일로 미룹니다");
        let long = doc(&v);
        ask_with(&mut chat, "groq", 4_000, &long, "지난 10분 정리", Some(10), &mut |_| {}, &|_, _| {}, &AtomicBool::new(false))
            .unwrap();
        let record = seen.borrow()[0].0.clone();
        assert!(record.contains("[13:18] 김성현 매니저: 방금 정했어요 배포는 다음 주 화요일로"), "{record}");
        assert!(record.contains("[03:20]부터, 그 가운데 일부 — 길어서 가장 최근 말을 먼저"), "{record}");
        assert!(record.chars().count() < 4_000 + 300, "{}", record.chars().count());

        // 받아 적은 말 없이 메모만 있으면 보내지 않고 알린다
        let mut memo_only = doc(&[]);
        memo_only.notes = "안건: 일정".into();
        let e = ask_with(&mut chat, "groq", 4_000, &memo_only, "지난 10분 정리", Some(10), &mut |_| {}, &|_, _| {}, &AtomicBool::new(false))
            .unwrap_err();
        assert!(e.to_string().contains("받아 적은 말이 없어요"), "{e}");
    }
}
