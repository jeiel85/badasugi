//! 실시간 번역 (기능 확장 계획 10번) — 요약 · 묻기에 쓰는 AI 로 받아 적은 문장을 번역한다
//!
//! - 문장(발화)마다 번역해 기록에 원문과 따로 둔다 ([`Translation`]). 번역에는 그때의 원문을 같이 적어 두어,
//!   문장을 고치면(더블클릭 · 찾아 바꾸기) 그 문장은 다시 번역할 것으로 본다 ([`pending`]).
//! - 몇 문장씩 묶어 보내고([`next_batch`]), 바로 앞 문장 몇 개를 맥락으로 함께 싣는다 — 번역은 하지 않게.
//!   보내는 줄은 `[n] 화자: 문장`, 받는 줄은 `[n] 번역` ([`parse`]). 번호가 빠진 문장은 다음 묶음에서 다시.
//! - 녹음 중(설정에서 켠 경우)에는 새로 확정된 문장이 쌓이는 대로, 지난 회의는 번역 단추로 남은 문장을 모두.
//!   Ollama 를 고르면 PC 안에서 끝난다 — API 는 묶음마다 요청이 하나씩 간다.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::prompts;
use crate::session::SessionDoc;
use crate::summary::{Chat, Reply, Turn};

/// 번역 단추를 멈췄을 때 (화면이 이 글이면 오류로 알리지 않는다)
pub const CANCELLED_MSG: &str = "번역을 멈췄어요";

/// 번역할 수 있는 언어 (코드, 이름)
pub const LANGS: [(&str, &str); 4] = [("ko", "한국어"), ("en", "영어"), ("ja", "일본어"), ("zh", "중국어(간체)")];

pub fn lang_name(code: &str) -> &'static str {
    LANGS.iter().find(|l| l.0 == code).map_or("한국어", |l| l.1)
}

/// 회의 기록의 번역 — 한 언어로
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Translation {
    /// 번역한 언어 ([`LANGS`] 의 코드)
    pub lang: String,
    /// 문장 id → 번역
    pub items: BTreeMap<u64, TrItem>,
}

/// 문장 하나의 번역 — 번역할 때의 원문(앞뒤 빈칸 뺀 것)을 같이 둔다. 원문이 바뀌면 낡은 번역
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrItem {
    pub src: String,
    pub text: String,
}

impl Translation {
    /// 지금 원문에 맞는 번역 (원문이 바뀌었거나 없으면 None)
    pub fn get(&self, id: u64, text: &str) -> Option<&str> {
        self.items.get(&id).filter(|x| x.src == text.trim()).map(|x| x.text.as_str())
    }
}

/// 한 묶음 최대 문장 수 · 글자 수 (원문 기준) — 너무 크면 번호를 빠뜨리거나 답이 늦다
const MAX_ITEMS: usize = 12;
const MAX_CHARS: usize = 1_800;
/// 맥락으로 싣는 앞 문장 수
const CONTEXT: usize = 3;

/// 번역이 필요한 문장 id (시간 순) — 번역이 없거나, 언어가 다르거나, 원문이 바뀐 것. 빈 문장은 뺀다
pub fn pending(doc: &SessionDoc, lang: &str) -> Vec<u64> {
    let tr = doc.translation.as_ref().filter(|t| t.lang == lang);
    let mut us: Vec<_> = doc.utterances.iter().filter(|u| !u.text.trim().is_empty()).collect();
    us.sort_by_key(|u| (u.start_ms, u.id));
    us.into_iter().filter(|u| tr.and_then(|t| t.get(u.id, &u.text)).is_none()).map(|u| u.id).collect()
}

/// 보낼 묶음 하나: (사용자 글, 묶음에 든 문장 id — 번호 1부터 차례로). `todo` 는 번역할 문장 id (시간 순)
pub fn next_batch(doc: &SessionDoc, todo: &[u64]) -> Option<(String, Vec<u64>)> {
    let mut us: Vec<_> = doc.utterances.iter().filter(|u| !u.text.trim().is_empty()).collect();
    us.sort_by_key(|u| (u.start_ms, u.id));
    let first = *todo.first()?;
    let at = us.iter().position(|u| u.id == first)?;
    let line = |u: &crate::session::Utterance| {
        format!("{}: {}", doc.speaker_name(doc.resolve(u.speaker)), u.text.split_whitespace().collect::<Vec<_>>().join(" "))
    };
    let mut s = String::new();
    if at > 0 {
        s.push_str("[앞 이야기 — 번역하지 마세요]\n");
        for u in &us[at.saturating_sub(CONTEXT)..at] {
            s.push_str(&line(u));
            s.push('\n');
        }
        s.push('\n');
    }
    s.push_str("[번역할 문장]\n");
    let want: std::collections::HashSet<u64> = todo.iter().copied().collect();
    let (mut ids, mut chars) = (Vec::new(), 0);
    for u in &us[at..] {
        if !want.contains(&u.id) {
            continue;
        }
        let l = line(u);
        if !ids.is_empty() && (ids.len() >= MAX_ITEMS || chars + l.chars().count() > MAX_CHARS) {
            break;
        }
        chars += l.chars().count();
        ids.push(u.id);
        s.push_str(&format!("[{}] {l}\n", ids.len()));
    }
    Some((s.trim_end().to_string(), ids))
}

/// 모델이 번역 앞에 붙이기도 하는 머리말 (뒤에 `:` 가 올 때만 뗀다)
const LABELS: [&str; 7] = ["번역", "번역문", "Translation", "translation", "訳", "翻訳", "翻译"];

/// 답 → 번호(1부터) → 번역. `[3] …` · `3. …` · `3) …` 꼴, 번호 없는 줄은 앞 번역에 잇는다.
/// 모델이 화자 이름까지 옮겨 적었으면(`[3] 김 팀장: …`) 뗀다 (`names`: 번호마다 화자 이름)
pub fn parse(reply: &str, names: &[String]) -> BTreeMap<usize, String> {
    let mut out: BTreeMap<usize, String> = BTreeMap::new();
    let mut last: Option<usize> = None;
    for raw in reply.lines() {
        let line = raw.trim().trim_start_matches(['-', '*', '•']).trim();
        if line.is_empty() {
            last = None;
            continue;
        }
        let numbered = (|| {
            let (n, rest) = if let Some(r) = line.strip_prefix('[') {
                let j = r.find(']')?;
                (r[..j].trim().parse::<usize>().ok()?, &r[j + 1..])
            } else {
                let j = line.find(|c: char| !c.is_ascii_digit())?;
                let rest = line[j..].strip_prefix('.').or_else(|| line[j..].strip_prefix(')'))?;
                (line[..j].parse::<usize>().ok()?, rest)
            };
            Some((n, rest.trim_start_matches(':').trim()))
        })();
        match numbered {
            Some((n, text)) if n >= 1 && n <= names.len() && !out.contains_key(&n) => {
                let name = &names[n - 1];
                let mut text = text.strip_prefix(&format!("{name}:")).map(str::trim).unwrap_or(text);
                // 지시문의 말을 머리말로 따라 쓴 것("[1] 번역: Hello." — gpt-5-mini) — 떼고 옮긴 글만
                for label in LABELS {
                    if let Some(rest) = text.strip_prefix(label).map(str::trim_start).and_then(|r| r.strip_prefix([':', '：'])) {
                        text = rest.trim();
                        break;
                    }
                }
                out.insert(n, text.to_string());
                last = Some(n);
            }
            Some(_) => last = None,
            None => {
                if let Some(t) = last.and_then(|n| out.get_mut(&n)) {
                    t.push(' ');
                    t.push_str(line);
                }
            }
        }
    }
    out.retain(|_, t| !t.trim().is_empty());
    out
}

/// 번역 지시 (`{언어}` 자리를 채운다)
pub fn system(lang: &str) -> String {
    prompts::translate().replace("{언어}", lang_name(lang))
}

/// 묶음 하나의 결과
#[derive(Debug, Default)]
pub struct Batch {
    /// 보낸 문장 id
    pub sent: Vec<u64>,
    /// 받은 번역 (문장 id, 그때 원문 · 번역) — 번호가 빠진 문장은 없다
    pub items: Vec<(u64, TrItem)>,
    /// 응답 (사용량)
    pub reply: Reply,
}

/// 번역할 문장(`todo`, 시간 순)에서 한 묶음을 번역한다
pub fn translate_batch(chat: &mut Chat, lang: &str, doc: &SessionDoc, todo: &[u64]) -> Result<Batch> {
    let Some((text, ids)) = next_batch(doc, todo) else {
        return Ok(Batch::default());
    };
    let by_id: std::collections::HashMap<u64, &crate::session::Utterance> = doc.utterances.iter().map(|u| (u.id, u)).collect();
    let names: Vec<String> = ids.iter().map(|id| doc.speaker_name(doc.resolve(by_id[id].speaker))).collect();
    let reply = chat(&system(lang), &[Turn::user(&text)], &mut |_| {})?;
    let got = parse(&reply.text, &names);
    let items = ids
        .iter()
        .enumerate()
        .filter_map(|(i, id)| {
            let t = got.get(&(i + 1))?;
            Some((*id, TrItem { src: by_id[id].text.trim().to_string(), text: t.clone() }))
        })
        .collect();
    Ok(Batch { sent: ids, items, reply })
}

/// 기록의 번역에 넣는다 (언어가 다르면 예전 번역을 버리고 새로)
pub fn merge(slot: &mut Option<Translation>, lang: &str, items: Vec<(u64, TrItem)>) {
    let t = slot.get_or_insert_with(Translation::default);
    if t.lang != lang {
        *t = Translation { lang: lang.to_string(), items: BTreeMap::new() };
    }
    t.items.extend(items);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> SessionDoc {
        serde_json::from_value(json!({
            "version": 1, "id": "m", "title": "t", "speakers": { "1": "Kim" },
            "utterances": [
                { "id": 1, "startMs": 0, "endMs": 1000, "speaker": 0, "text": "Let's start." },
                { "id": 2, "startMs": 1000, "endMs": 2000, "speaker": 1, "text": "  OK,   the release is on Friday. " },
                { "id": 3, "startMs": 2000, "endMs": 3000, "speaker": 0, "text": " " },
                { "id": 4, "startMs": 3000, "endMs": 4000, "speaker": 0, "text": "Any questions?" }
            ],
        }))
        .unwrap()
    }

    #[test]
    fn pending_skips_blank_and_fresh_translations() {
        let mut d = doc();
        assert_eq!(pending(&d, "ko"), vec![1, 2, 4]);
        merge(&mut d.translation, "ko", vec![(2, TrItem { src: "OK,   the release is on Friday.".into(), text: "네, 출시는 금요일이에요.".into() })]);
        assert_eq!(pending(&d, "ko"), vec![1, 4]);
        assert_eq!(pending(&d, "ja"), vec![1, 2, 4], "다른 언어는 처음부터");
        // 원문을 고치면 다시
        d.utterances[1].text = "OK, the release is on Thursday.".into();
        assert_eq!(pending(&d, "ko"), vec![1, 2, 4]);
        // 다른 언어로 넣으면 예전 번역은 버린다
        merge(&mut d.translation, "en", vec![(1, TrItem { src: "Let's start.".into(), text: "Let's start.".into() })]);
        assert_eq!(d.translation.as_ref().unwrap().items.len(), 1);
    }

    #[test]
    fn batch_has_context_numbers_and_speakers() {
        let d = doc();
        let (text, ids) = next_batch(&d, &[2, 4]).unwrap();
        assert_eq!(ids, vec![2, 4]);
        assert_eq!(
            text,
            "[앞 이야기 — 번역하지 마세요]\n화자 1: Let's start.\n\n[번역할 문장]\n[1] Kim: OK, the release is on Friday.\n[2] 화자 1: Any questions?"
        );
        let (text, ids) = next_batch(&d, &[1]).unwrap();
        assert_eq!((text.as_str(), ids), ("[번역할 문장]\n[1] 화자 1: Let's start.", vec![1]));
        assert!(next_batch(&d, &[]).is_none());
    }

    #[test]
    fn batch_stops_at_size_limits() {
        let mut d = doc();
        d.utterances = (0..40)
            .map(|i| crate::session::Utterance { id: i, start_ms: i * 1000, end_ms: i * 1000 + 900, speaker: Some(0), text: format!("문장 {i}") })
            .collect();
        let todo: Vec<u64> = (0..40).collect();
        assert_eq!(next_batch(&d, &todo).unwrap().1.len(), MAX_ITEMS);
        d.utterances[0].text = "가".repeat(3000);
        assert_eq!(next_batch(&d, &todo).unwrap().1, vec![0], "아주 긴 문장도 하나는 보낸다");
    }

    #[test]
    fn parse_numbered_lines() {
        let names = vec!["Kim".to_string(), "화자 1".to_string(), "화자 2".to_string()];
        let got = parse("[1] Kim: 네, 출시는 금요일이에요.\n- [2] 질문 있나요?\n이어진 줄\n\n3. 셋째\n[9] 없는 번호\n[2] 두 번째 답은 무시", &names);
        assert_eq!(got.get(&1).map(String::as_str), Some("네, 출시는 금요일이에요."));
        assert_eq!(got.get(&2).map(String::as_str), Some("질문 있나요? 이어진 줄"));
        assert_eq!(got.get(&3).map(String::as_str), Some("셋째"));
        assert_eq!(got.len(), 3);
        assert!(parse("번역할 문장이 없습니다", &names).is_empty());
        let got = parse("[1] 번역: Hello.\n[2] Translation : Hi.\n[3] 번역은 짧게", &names);
        assert_eq!(got.get(&1).map(String::as_str), Some("Hello."), "머리말은 뗀다");
        assert_eq!(got.get(&2).map(String::as_str), Some("Hi."));
        assert_eq!(got.get(&3).map(String::as_str), Some("번역은 짧게"), "':' 가 없으면 글 그대로");
        assert!(parse("[1]   ", &names).is_empty(), "빈 번역은 다음에 다시");
    }

    /// 실제 AI 로 저장된 회의 하나를 처음부터 끝까지 번역해 본다 (수동 시험 — 요청이 실제로 간다):
    /// `BD_TR_DOC`=회의 기록(.json) · `BD_TR_LANG`(기본 en) · `BD_SETTINGS`(없으면 앱 설정) ·
    /// `BD_TR_OLLAMA`=모델 이름이면 설정 대신 이 PC 의 Ollama 로.
    /// `cargo test -p badasugi translate_a_saved_meeting -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn translate_a_saved_meeting() {
        let doc_path = std::env::var("BD_TR_DOC").expect("BD_TR_DOC 에 회의 기록(.json) 경로");
        let settings = std::env::var_os("BD_SETTINGS").map(std::path::PathBuf::from).unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("APPDATA").expect("APPDATA")).join("com.badasugi.desktop").join("settings.json")
        });
        let mut cfg = crate::settings::Settings::load(&settings).summary;
        if let Ok(m) = std::env::var("BD_TR_OLLAMA") {
            cfg.provider = "ollama".into();
            cfg.ollama_model = m;
        }
        let lang = std::env::var("BD_TR_LANG").unwrap_or_else(|_| "en".into());
        let mut d: SessionDoc = serde_json::from_str(&std::fs::read_to_string(&doc_path).unwrap()).unwrap();
        d.translation = None;
        let (provider, mut chat) = crate::summary::chat_caller(&cfg).unwrap();
        let started = std::time::Instant::now();
        let mut misses: std::collections::HashMap<u64, u32> = Default::default();
        let mut batches = 0;
        loop {
            let todo: Vec<u64> = pending(&d, &lang).into_iter().filter(|u| misses.get(u).copied().unwrap_or(0) < 2).collect();
            if todo.is_empty() {
                break;
            }
            let b = translate_batch(&mut chat, &lang, &d, &todo).unwrap();
            batches += 1;
            eprintln!("[{provider} {}] 보냄 {} · 받음 {} · 입력 {} · 출력 {}", b.reply.model, b.sent.len(), b.items.len(), b.reply.in_tokens, b.reply.out_tokens);
            let got: std::collections::HashSet<u64> = b.items.iter().map(|x| x.0).collect();
            for s in b.sent.iter().filter(|s| !got.contains(s)) {
                *misses.entry(*s).or_default() += 1;
            }
            merge(&mut d.translation, &lang, b.items);
        }
        for u in &d.utterances {
            let t = d.translation.as_ref().and_then(|t| t.get(u.id, &u.text)).unwrap_or("(번역 없음)");
            eprintln!("{}\n  → {t}", u.text.trim());
        }
        let left = pending(&d, &lang).len();
        eprintln!("묶음 {batches}개 · {:.1}초 · 못 한 문장 {left}개", started.elapsed().as_secs_f64());
    }

    #[test]
    fn batch_round_trip_with_a_fake_model() {
        let d = doc();
        let mut chat: Chat = Box::new(|sys: &str, turns: &[Turn], _: &mut dyn FnMut(&str)| {
            assert!(sys.contains("한국어"));
            assert!(turns[0].text.contains("[2] 화자 1: Any questions?"));
            Ok(Reply { text: "[1] 네, 출시는 금요일이에요.".into(), model: "fake".into(), ..Default::default() })
        });
        let b = translate_batch(&mut chat, "ko", &d, &[2, 4]).unwrap();
        assert_eq!(b.reply.model, "fake");
        assert_eq!(b.sent, vec![2, 4]);
        assert_eq!(
            b.items,
            vec![(2, TrItem { src: "OK,   the release is on Friday.".into(), text: "네, 출시는 금요일이에요.".into() })],
            "빠진 [2] 는 다음에 다시"
        );
    }
}
