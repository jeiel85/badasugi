//! 모든 회의에 묻기 — 쌓인 회의 기록에서 질문과 관련 있는 회의를 골라, 요약에 쓰는 AI(설정 › 요약)로 묻는다
//!
//! - 후보 회의: 질문(+ 바로 앞 질문)의 두 글자 묶음이 제목 · 요약 · 메모 · 녹취록에 얼마나 겹치는지(드문 말일수록
//!   무겁게, 제목 > 요약 · 메모 · 묻고 답한 것 > 녹취록)로 점수를 매겨 위에서부터. 겹치는 회의가 없으면
//!   ("지난주 회의 정리해 줘") 기간 안의 최근 회의부터
//! - 회의 수는 제공자별 한 번에 보낼 크기([`summary::chunk_budget`])에 맞춰 2~8개, 회의마다 자리를 점수에 비례해
//!   나누고 회의에 묻기와 같은 방법([`ask::context`])으로 싣는다 — 요약 · 메모 + 질문과 겹치는 녹취록 문장
//! - 근거는 `[회의 2 · 12:34]` (요약 · 메모만이면 `[회의 2]`) — 화면이 눌러서 그 회의의 그 문장으로 간다
//! - 묻고 답한 것은 기록에 저장하지 않는다 (회의 하나에 묶이지 않는다 — 앱을 다시 켜면 비워진다)

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::ask::{self, Event, Focus};
use crate::prompts;
use crate::session::SessionDoc;
use crate::settings::SummarySettings;
use crate::summary::{self, Reply, Turn};

/// 한 번에 싣는 회의 수 (보낼 크기가 작은 제공자는 더 적게)
const MAX_MEETINGS: usize = 8;
const MIN_MEETINGS: usize = 2;
/// 회의 하나에 이만큼(글자)은 주고 싶다 — 보낼 크기 / 이 값 = 실을 회의 수
const PER_MEETING: usize = 2_000;
/// 앞선 질문 · 답은 이만큼까지 이어 보낸다 (최근 것)
const HISTORY: usize = 4;
/// 너무 크다고 거절되면 반씩 줄이되 이보다 작게는 줄이지 않는다
const MIN_BUDGET: usize = 2_000;

/// 실은 회의 (답의 `[회의 n · …]` 번호 → 기록)
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub n: usize,
    pub id: String,
    pub title: String,
    pub started_at: String,
}

/// 받은 답
#[derive(Debug)]
pub struct Answer {
    pub text: String,
    pub model: String,
    pub sources: Vec<Source>,
    /// 기간 안의 회의 수 (그중 `sources` 만 실었다)
    pub considered: usize,
    /// 질문과 겹치는 회의가 없어 최근 회의를 실었다
    pub recent: bool,
}

/// 회의마다 질문과 얼마나 맞는지
fn scores(docs: &[SessionDoc], query: &str) -> Vec<f64> {
    let q = ask::grams(query);
    if q.is_empty() {
        return vec![0.0; docs.len()];
    }
    // (제목, 요약 · 메모 · 묻고 답한 것, 녹취록) 의 두 글자 묶음
    let sets: Vec<[HashSet<String>; 3]> = docs
        .iter()
        .map(|d| {
            let mut side = String::new();
            for s in [d.summary.as_deref().unwrap_or(""), d.notes.as_str()] {
                side.push_str(s);
                side.push('\n');
            }
            for x in &d.qa {
                side.push_str(&x.q);
                side.push('\n');
                side.push_str(&x.a);
                side.push('\n');
            }
            let said: String = d.utterances.iter().map(|u| u.text.as_str()).collect::<Vec<_>>().join("\n");
            [ask::grams(&d.title), ask::grams(&side), ask::grams(&said)]
        })
        .collect();
    let n = docs.len() as f64;
    let mut df: HashMap<&str, f64> = HashMap::new();
    for g in &q {
        df.insert(g, sets.iter().filter(|s| s.iter().any(|x| x.contains(g))).count() as f64);
    }
    // 모든 회의에 있는 말("회의" · "정리")은 가볍게, 회의가 하나뿐이어도 0 이 되지 않게
    let idf = |g: &str| ((n + 1.0) / (df[g] + 0.5)).ln().max(0.0);
    sets.iter()
        .map(|[title, side, said]| {
            q.iter()
                .map(|g| {
                    let w = 3.0 * title.contains(g) as u8 as f64 + 2.0 * side.contains(g) as u8 as f64 + said.contains(g) as u8 as f64;
                    w * idf(g)
                })
                .sum()
        })
        .collect()
}

/// 실을 회의 (번호 순 = 날짜 순) · 회의마다 자리 비율 · 최근 회의로 골랐는지.
/// `docs` 는 최근 것이 앞
fn choose(docs: &[SessionDoc], query: &str, budget: usize) -> (Vec<(usize, f64)>, bool) {
    let max = (budget / PER_MEETING).clamp(MIN_MEETINGS, MAX_MEETINGS);
    let s = scores(docs, query);
    let top = s.iter().cloned().fold(0.0, f64::max);
    let mut picked: Vec<(usize, f64)>;
    let recent = top < 1e-6;
    if recent {
        picked = (0..docs.len().min(max)).map(|i| (i, 1.0)).collect();
    } else {
        let mut order: Vec<usize> = (0..docs.len()).filter(|&i| s[i] >= top * 0.15).collect();
        order.sort_by(|&a, &b| s[b].total_cmp(&s[a]).then(a.cmp(&b)));
        order.truncate(max);
        picked = order.into_iter().map(|i| (i, s[i])).collect();
    }
    // 날짜 순(오래된 것 먼저)으로 번호를 매긴다 — 답이 회의 순서대로 정리하기 쉽게
    picked.sort_by(|a, b| docs[a.0].started_at.cmp(&docs[b.0].started_at).then(b.0.cmp(&a.0)));
    let total: f64 = picked.iter().map(|p| p.1).sum::<f64>().max(1e-9);
    // 자리: 점수에 비례하되 한 회의가 반을 넘지 않게, 가장 작은 몫도 고르게 나눈 몫의 절반은
    let even = 1.0 / picked.len().max(1) as f64;
    let mut shares: Vec<(usize, f64)> =
        picked.iter().map(|&(i, sc)| (i, (sc / total).clamp(even / 2.0, 0.5_f64.max(even)))).collect();
    let sum: f64 = shares.iter().map(|p| p.1).sum::<f64>().max(1e-9);
    for p in &mut shares {
        p.1 /= sum;
    }
    (shares, recent)
}

/// 앞 `n` 글자 (잘랐으면 끝에 …)
fn clip(s: &str, n: usize) -> String {
    let mut c: String = s.chars().take(n).collect();
    if c.len() < s.len() {
        c.push('…');
    }
    c
}

/// 회의 기록 덩어리 + 실은 회의
fn records(docs: &[SessionDoc], query: &str, budget: usize, considered: usize) -> (String, Vec<Source>, bool) {
    let (shares, recent) = choose(docs, query, budget);
    let how = if recent {
        "질문과 겹치는 말이 있는 회의가 없어 최근 회의부터"
    } else {
        "질문과 관련 있어 보이는 회의 위주로"
    };
    let mut text = format!("다음은 회의 기록 {}개입니다 (기록 {considered}개 중 {how} 골랐어요). 회의마다 번호가 있어요.\n", shares.len());
    let mut sources = Vec::new();
    for (k, &(i, share)) in shares.iter().enumerate() {
        let d = &docs[i];
        let room = ((budget as f64) * share) as usize;
        let ctx = ask::context(d, &Focus::Question(query), room.max(600));
        text.push_str(&format!("\n# 회의 {}\n{}\n", k + 1, ctx.text.trim_end()));
        sources.push(Source { n: k + 1, id: d.id.clone(), title: d.title.clone(), started_at: d.started_at.clone() });
    }
    (text, sources, recent)
}

/// 모든 회의에 묻는다. `docs` 는 물어볼 기간 안의 기록(최근 것이 앞), `past` 는 이 대화에서 앞서 묻고 받은 것
#[allow(clippy::too_many_arguments)]
pub fn ask_all(
    cfg: &SummarySettings,
    docs: &[SessionDoc],
    question: &str,
    past: &[(String, String)],
    on_event: &mut dyn FnMut(Event),
    on_usage: &dyn Fn(&str, &Reply),
    cancel: &AtomicBool,
) -> Result<Answer> {
    let (provider, mut chat) = summary::chat_caller(cfg)?;
    let budget = summary::chunk_budget(&provider);
    ask_all_with(&mut *chat, &provider, budget, docs, question, past, on_event, on_usage, cancel)
}

#[allow(clippy::too_many_arguments)]
fn ask_all_with(
    chat: &mut dyn FnMut(&str, &[Turn], &mut dyn FnMut(&str)) -> Result<Reply>,
    provider: &str,
    mut budget: usize,
    docs: &[SessionDoc],
    question: &str,
    past: &[(String, String)],
    on_event: &mut dyn FnMut(Event),
    on_usage: &dyn Fn(&str, &Reply),
    cancel: &AtomicBool,
) -> Result<Answer> {
    let question = question.trim();
    if question.is_empty() {
        bail!("물어볼 내용을 적어 주세요");
    }
    let docs: Vec<&SessionDoc> = docs.iter().filter(|d| d.has_content()).collect();
    if docs.is_empty() {
        bail!("물어볼 회의 기록이 없어요");
    }
    let docs: Vec<SessionDoc> = docs.into_iter().cloned().collect();
    let past: Vec<(String, String)> =
        past.iter().skip(past.len().saturating_sub(HISTORY)).map(|(q, a)| (clip(q, 500), clip(a, 1_500))).collect();
    // 회의를 고를 때는 바로 앞 질문도 함께 본다 ("그중 기한 있는 것만")
    let query = match past.last() {
        Some((q, _)) => format!("{question}\n{q}"),
        None => question.to_string(),
    };
    let mut waits = 0;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(anyhow::Error::new(summary::AskCancelled));
        }
        let past_now = ask::recent_within(&past, budget / 4);
        let talk = past_now.iter().map(|(q, a)| q.chars().count() + a.chars().count()).sum::<usize>()
            + question.chars().count();
        let (head, sources, recent) = records(&docs, &query, budget.saturating_sub(talk).max(budget / 2), docs.len());
        let mut turns = vec![Turn { user: true, text: &head, cache: true }];
        for (q, a) in &past_now {
            turns.push(Turn::user(q));
            turns.push(Turn { user: false, text: a, cache: false });
        }
        turns.push(Turn::user(question));
        let wrote = Cell::new(false);
        let res = chat(prompts::ask_all(), &turns, &mut |t| {
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
                return Ok(Answer { text, model: r.model, sources, considered: docs.len(), recent });
            }
            Err(e) => e,
        };
        if wrote.get() {
            on_event(Event::Restart);
        }
        if summary::too_large(&e) && budget > MIN_BUDGET {
            budget = (budget / 2).max(MIN_BUDGET);
            log::info!("모든 회의에 묻기가 너무 크다고 거절됨 — {budget}자로 줄여 다시");
            continue;
        }
        match summary::wait_for(&e) {
            Some(w) if w <= summary::MAX_WAIT_SECS && waits < 4 => {
                waits += 1;
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
    use serde_json::json;

    fn meeting(id: &str, title: &str, day: &str, lines: &[&str], summary: Option<&str>) -> SessionDoc {
        let utterances: Vec<_> = lines
            .iter()
            .enumerate()
            .map(|(i, t)| json!({ "id": i, "startMs": i as u64 * 20_000, "endMs": i as u64 * 20_000 + 3_000, "speaker": i % 2, "text": t }))
            .collect();
        serde_json::from_value(json!({
            "version": 1, "id": id, "title": title, "startedAt": format!("{day} 10:00"), "durationMs": 600_000, "engine": "x",
            "speakers": { "0": "김 팀장", "1": "이 개발" }, "utterances": utterances, "summary": summary
        }))
        .unwrap()
    }

    /// 최근 것이 앞
    fn docs() -> Vec<SessionDoc> {
        vec![
            meeting("d4", "주간 회의", "2026-11-19", &["이번 주 일정 이야기", "배포는 금요일"], None),
            meeting("d3", "보안 점검 회의", "2026-11-12", &["VPN 은 월 12만 원에 도입", "트래픽 따라 비용이 달라짐"], Some("### 결정 사항\n- VPN 도입")),
            meeting("d2", "디자인 리뷰", "2026-11-05", &["온보딩 시안 두 개", "색을 조금 밝게"], None),
            meeting("d1", "예산 회의", "2026-10-29", &["VPN 비용은 다음 분기 예산에", "서버 비용 검토"], None),
        ]
    }

    #[test]
    fn meetings_that_match_the_question_come_first_in_date_order() {
        let d = docs();
        let (picked, recent) = choose(&d, "VPN 비용 얼마라고 했지?", 40_000);
        assert!(!recent);
        let ids: Vec<&str> = picked.iter().map(|p| d[p.0].id.as_str()).collect();
        assert!(ids.contains(&"d3") && ids.contains(&"d1"), "{ids:?}");
        assert!(!ids.contains(&"d2"), "관련 없는 회의는 빼요 {ids:?}");
        // 번호는 날짜 순 (오래된 것 먼저)
        assert!(ids.iter().position(|x| *x == "d1") < ids.iter().position(|x| *x == "d3"), "{ids:?}");
        let s = scores(&d, "VPN 비용");
        assert!(s[1] > s[3] && s[3] > 0.0 && s[2] == 0.0, "요약에도 있는 회의가 더 높게 {s:?}");
        let sum: f64 = picked.iter().map(|p| p.1).sum();
        assert!((sum - 1.0).abs() < 1e-6);
    }

    #[test]
    fn unrelated_questions_fall_back_to_recent_meetings_and_small_budgets_take_fewer() {
        let d = docs();
        let (picked, recent) = choose(&d, "zzz", 40_000);
        assert!(recent);
        assert_eq!(picked.len(), 4);
        // Groq(4천 자)는 두 회의까지 — 최근 두 회의, 날짜 순
        let (picked, _) = choose(&d, "zzz", 4_000);
        let ids: Vec<&str> = picked.iter().map(|p| d[p.0].id.as_str()).collect();
        assert_eq!(ids, ["d3", "d4"]);
    }

    #[test]
    fn records_are_numbered_with_sources() {
        let d = docs();
        let (text, sources, _) = records(&d, "VPN 비용", 40_000, 4);
        assert!(text.starts_with("다음은 회의 기록 2개입니다 (기록 4개 중 질문과 관련 있어 보이는 회의 위주로 골랐어요)"), "{text}");
        assert!(text.contains("# 회의 1\n회의 제목: 예산 회의\n일시: 2026-10-29 10:00"), "{text}");
        assert!(text.contains("# 회의 2\n회의 제목: 보안 점검 회의") && text.contains("[00:00] 김 팀장: VPN 은 월 12만 원에 도입"), "{text}");
        assert_eq!(
            sources,
            [
                Source { n: 1, id: "d1".into(), title: "예산 회의".into(), started_at: "2026-10-29 10:00".into() },
                Source { n: 2, id: "d3".into(), title: "보안 점검 회의".into(), started_at: "2026-11-12 10:00".into() },
            ]
        );
    }

    #[test]
    fn asks_with_the_records_then_past_turns() {
        let d = docs();
        let mut seen: Vec<(bool, String)> = Vec::new();
        let mut chat = |s: &str, turns: &[Turn], t: &mut dyn FnMut(&str)| {
            assert_eq!(s, prompts::ask_all());
            seen = turns.iter().map(|x| (x.user, x.text.to_string())).collect();
            t("월 12만 원 [회의 2 · 00:00]");
            Ok(Reply { text: "월 12만 원 [회의 2 · 00:00]".into(), model: "m".into(), ..Default::default() })
        };
        let past = vec![("VPN 은 언제 도입했어?".to_string(), "11월 [회의 1]".to_string())];
        let mut got = String::new();
        let a = ask_all_with(
            &mut chat,
            "anthropic",
            40_000,
            &d,
            " 비용은? ",
            &past,
            &mut |e| {
                if let Event::Text(t) = e {
                    got.push_str(t)
                }
            },
            &|_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!((a.text.as_str(), got.as_str(), a.considered, a.recent), ("월 12만 원 [회의 2 · 00:00]", "월 12만 원 [회의 2 · 00:00]", 4, false));
        assert_eq!(seen.len(), 4);
        assert!(seen[0].0 && seen[0].1.contains("# 회의 1"), "기록이 맨 앞");
        assert_eq!(seen[1], (true, "VPN 은 언제 도입했어?".to_string()));
        assert_eq!(seen[3], (true, "비용은?".to_string()));
        // "비용은?" 만으로는 VPN 회의를 고르기 어렵다 — 앞 질문(VPN)도 함께 보고 고른다
        assert!(a.sources.iter().any(|s| s.id == "d3"), "{:?}", a.sources);
    }

    #[test]
    fn nothing_to_ask_sends_nothing() {
        let mut calls = 0;
        let mut chat = |_: &str, _: &[Turn], _: &mut dyn FnMut(&str)| {
            calls += 1;
            Ok(Reply::default())
        };
        let empty = vec![meeting("e", "빈 회의", "2026-11-19", &[], None)];
        assert!(ask_all_with(&mut chat, "groq", 4_000, &empty, "질문", &[], &mut |_| {}, &|_, _| {}, &AtomicBool::new(false)).is_err());
        assert!(ask_all_with(&mut chat, "groq", 4_000, &docs(), "  ", &[], &mut |_| {}, &|_, _| {}, &AtomicBool::new(false)).is_err());
        assert_eq!(calls, 0);
    }
}
