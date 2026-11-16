//! 회의 기록: 진행 중 자동 저장(JSON + Markdown), 기록 목록, 내보내기

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Result};
use badasugi_core::diarize::{match_voices, KnownVoice, SpeakerVoice, UtteranceVoice};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Utterance {
    pub id: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionDoc {
    pub version: u32,
    /// 파일 이름 (확장자 제외)
    pub id: String,
    pub title: String,
    /// "2026-11-12 14:30"
    pub started_at: String,
    pub duration_ms: u64,
    pub engine: String,
    /// 화자 번호 → 이름 (바꾼 경우만)
    pub speakers: BTreeMap<u32, String>,
    pub utterances: Vec<Utterance>,
    pub summary: Option<String>,
    /// 사람이 요약을 직접 고쳤다 (AI 로 다시 요약하기 전에 물어본다)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub summary_edited: bool,
    /// 회의를 보면서 적은 메모 (사람만 고친다). `[시각]` 으로 시작하는 줄은 그 시각 문장에 단 코멘트다
    /// ([`note_comments`]) — 코멘트는 따로 저장하지 않고 메모 한 곳에만 있다.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// 예전 버전(0.4.9 까지)의 문장 코멘트: 발화 id → 코멘트. 읽을 때 메모의 `[시각]` 줄로 옮기고
    /// ([`SessionDoc::fold_comments`]) 비우므로 새로 저장하는 파일에는 남지 않는다.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub comments: BTreeMap<u64, String>,
    pub audio_file: Option<String>,
    /// 합친 화자: 원래 번호 → 합쳐진 번호 (녹음 중 새로 들어오는 발화에도 적용)
    pub speaker_map: BTreeMap<u32, u32>,
    /// 자동으로 붙인 제목이면 요약 때 AI 가 제안한 제목으로 바꾼다
    pub auto_title: bool,
    /// 파일 받아쓰기라면 원본 파일 이름
    pub source_file: Option<String>,
    /// 파일 받아쓰기라면 원본 파일 전체 경로 (다시 듣기용)
    pub source_path: Option<String>,
    /// 화면에서 지운 발화 id (저장 요청에만 쓰고 파일에는 남기지 않는다)
    #[serde(skip_serializing)]
    pub deleted: Vec<u64>,
    /// 회의 후 화자 다시 정리를 반영할 때마다 1씩 오른다. 이보다 오래된 화면 사본이 늦게 저장되면
    /// 화자 배정 · 이름은 지키고 제목 · 글자 · 삭제만 반영한다 (정리 결과를 덮어쓰지 않게).
    pub speaker_rev: u32,
    /// 회의 후 화자 다시 정리 전(녹음 중에 정해진) 화자 배정 · 이름 — 정리가 마음에 들지 않으면 되돌린다
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_refine: Option<SpeakerSnapshot>,
}

/// 한 시점의 화자 배정 · 이름
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SpeakerSnapshot {
    /// 화자 번호 → 이름
    pub speakers: BTreeMap<u32, String>,
    /// [발화 id, 화자] — 화자가 없는 발화는 뺀다
    pub assign: Vec<(u64, u32)>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    #[serde(flatten)]
    pub meta: SessionMeta,
    /// 찾은 곳 앞뒤 문장
    pub snippet: String,
    pub hits: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub count: usize,
    pub speakers: usize,
    pub preview: String,
}

/// 회의가 끝난 뒤 목소리로 이름을 붙이려면 그 화자가 이만큼(초) 말해야 한다
const NAME_MIN_SECS: f32 = 8.0;

/// 다시 정리를 반영한 결과
#[derive(Debug)]
pub struct Refined {
    /// 화자가 바뀐 문장 수
    pub moved: usize,
    /// 목소리가 있는 화자 수
    pub speakers: usize,
    /// 목소리로 새로 이름을 붙인 화자 수
    pub named: usize,
}

fn resolve_with(map: &BTreeMap<u32, u32>, s: Option<u32>) -> Option<u32> {
    let mut s = s?;
    for _ in 0..16 {
        match map.get(&s) {
            Some(&t) if t != s => s = t,
            _ => break,
        }
    }
    Some(s)
}

pub fn fmt_time(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// 메모 속 코멘트 하나: `[시각]` 으로 시작하는 줄부터 다음 `[시각]` 줄 · 빈 줄 바로 앞까지.
/// (`- [12:34] …` 처럼 목록 표시가 앞에 있어도 된다)
#[derive(Debug, Clone, PartialEq)]
pub struct NoteComment {
    /// 적힌 시각 (ms)
    pub ms: u64,
    /// 메모의 몇째 줄부터 (0부터, `\n` 기준)
    pub from: usize,
    /// 몇째 줄 앞까지
    pub to: usize,
    /// 시각을 뺀 글 (여러 줄이면 줄바꿈으로 잇는다)
    pub text: String,
}

/// 줄이 `[mm:ss]` · `[h:mm:ss]` 로 시작하면 (시각 ms, 시각 뒤의 글)
fn parse_stamp(line: &str) -> Option<(u64, &str)> {
    let mut s = line.trim_start();
    if let Some(r) = s.strip_prefix(['-', '*']) {
        if r.starts_with(char::is_whitespace) {
            s = r.trim_start();
        }
    }
    let r = s.strip_prefix('[')?;
    let end = r.find(']')?;
    let parts: Vec<&str> = r[..end].split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts.iter().any(|p| p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()))
        || parts[parts.len() - 1].len() != 2
    {
        return None;
    }
    let n: Vec<u64> = parts.iter().filter_map(|p| p.parse().ok()).collect();
    let (h, m, sec) = if n.len() == 3 { (n[0], n[1], n[2]) } else { (0, n[0], n[1]) };
    if sec >= 60 || (n.len() == 3 && m >= 60) {
        return None;
    }
    let rest = &r[end + 1..];
    Some(((h * 3600 + m * 60 + sec) * 1000, rest.strip_prefix(' ').unwrap_or(rest)))
}

/// 메모에서 코멘트(`[시각]` 줄)를 찾는다 — 글이 없는 시각 줄은 코멘트가 아니다
pub fn note_comments(notes: &str) -> Vec<NoteComment> {
    let lines: Vec<&str> = notes.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((ms, first)) = parse_stamp(lines[i]) else {
            i += 1;
            continue;
        };
        let from = i;
        let mut text = vec![first.trim_end()];
        i += 1;
        while i < lines.len() && !lines[i].trim().is_empty() && parse_stamp(lines[i]).is_none() {
            text.push(lines[i].trim_end());
            i += 1;
        }
        let text = text.join("\n").trim().to_string();
        if !text.is_empty() {
            out.push(NoteComment { ms, from, to: i, text });
        }
    }
    out
}

/// 메모에 `[시각] 글` 코멘트를 시각 순서에 맞는 자리에 넣는다: 시각이 같거나 이른 코멘트 중 메모에서
/// 마지막 것 바로 뒤, 그런 게 없으면 더 늦은 첫 코멘트 바로 앞, 코멘트가 하나도 없으면 맨 끝.
/// (빈 줄은 코멘트를 끝내므로 글 속 빈 줄은 뺀다)
pub fn insert_note_comment(notes: &str, ms: u64, text: &str) -> String {
    let body: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    let Some(first) = body.first() else {
        return notes.to_string();
    };
    let mut add = vec![format!("[{}] {}", fmt_time(ms), first.trim_start())];
    add.extend(body[1..].iter().map(|s| s.to_string()));
    let blocks = note_comments(notes);
    let at = blocks
        .iter()
        .filter(|b| b.ms <= ms)
        .last()
        .map(|b| b.to)
        .or_else(|| blocks.iter().find(|b| b.ms > ms).map(|b| b.from));
    let mut lines: Vec<String> = if notes.is_empty() {
        Vec::new()
    } else {
        notes.split('\n').map(String::from).collect()
    };
    let at = at.unwrap_or_else(|| {
        // 맨 끝에: 끝 줄바꿈(마지막 빈 줄)은 그대로 끝에 둔다
        if lines.last().is_some_and(|l| l.is_empty()) {
            lines.len() - 1
        } else {
            lines.len()
        }
    });
    lines.splice(at..at, add);
    lines.join("\n")
}

impl SessionDoc {
    pub fn new(engine: String) -> Self {
        let now = chrono::Local::now();
        Self {
            version: 1,
            id: now.format("%Y-%m-%d_%H%M%S").to_string(),
            title: format!("{} 회의", now.format("%m월 %d일 %H:%M")),
            started_at: now.format("%Y-%m-%d %H:%M").to_string(),
            engine,
            auto_title: true,
            ..Default::default()
        }
    }

    /// 합친 화자를 따라가 최종 화자 번호를 돌려준다
    pub fn resolve(&self, s: Option<u32>) -> Option<u32> {
        resolve_with(&self.speaker_map, s)
    }

    /// 회의가 끝난 뒤 다시 정리한 화자를 반영한다.
    ///
    /// - `labels`: [발화 id, 실시간 화자, 다시 정리한 화자]. 사람이 직접 화자를 바꾼 문장은 건드리지 않고,
    ///   녹음 중에 합친 화자(`speaker_map`)는 그대로 지킨다 (다시 정리도 합친 화자를 한 화자로 묶어 계산한다).
    /// - `utterances`: 문장별 목소리 — 화자별 목소리는 지금의 문장 배정대로 모은다 (사람이 옮긴 문장도 따라감)
    /// - 화자가 합쳐져 사라지면 그 이름은 합쳐진 쪽이 이어받는다 (그쪽에 이름이 없을 때).
    /// - `known` 목소리와 닮은 화자에 이름을 붙인다. 녹음 중에 목소리로 붙였던 이름(`auto_named`: 화자 → 이름)은
    ///   그 화자에 그대로 붙어 있을 때만 자동 이름으로 보고, 회의 전체로 다시 보고 아니면 거둔다.
    ///   사람이 고쳤거나 직접 붙인 이름은 그대로.
    /// - 화자 번호를 1부터 빈틈없이 다시 매긴다 (합쳐져 "화자 3" 이 비는 일 없게).
    pub fn apply_refinement(
        &mut self,
        labels: &[[u64; 3]],
        utterances: &[UtteranceVoice],
        auto_named: &BTreeMap<u32, String>,
        known: &[KnownVoice],
    ) -> Refined {
        let before = self.speaker_snapshot();
        let smap = self.speaker_map.clone();
        // 자동으로 붙인 이름 중 지금도 그 화자에 그대로 붙어 있는 것만 자동 이름 (사람이 고쳤으면 사람 이름)
        let mut auto: BTreeMap<u32, String> =
            auto_named.iter().filter(|(l, n)| self.speakers.get(l) == Some(*n)).map(|(l, n)| (*l, n.clone())).collect();
        let map: HashMap<u64, (u32, u32)> = labels.iter().map(|l| (l[0], (l[1] as u32, l[2] as u32))).collect();
        // 화자별로 말이 어디로 옮겨 갔나 (이름 이어받기)
        let mut flow: BTreeMap<u32, BTreeMap<u32, u64>> = BTreeMap::new();
        let mut moved = 0;
        for u in self.utterances.iter_mut() {
            let Some(&(on, re)) = map.get(&u.id) else {
                continue;
            };
            let from = resolve_with(&smap, Some(on));
            if u.speaker != from {
                continue;
            }
            let to = resolve_with(&smap, Some(re));
            if let (Some(f), Some(t)) = (from, to) {
                *flow.entry(f).or_default().entry(t).or_default() += u.end_ms.saturating_sub(u.start_ms);
            }
            if to != u.speaker {
                u.speaker = to;
                moved += 1;
            }
        }
        let used: BTreeSet<u32> = self.utterances.iter().filter_map(|u| u.speaker).collect();
        for (f, dests) in &flow {
            if used.contains(f) {
                continue;
            }
            let Some(name) = self.speakers.get(f).cloned() else {
                continue;
            };
            if let Some((&t, _)) = dests.iter().max_by_key(|x| *x.1) {
                if !self.speakers.contains_key(&t) {
                    // 자동 이름이었다면 이어받은 쪽에서도 자동 이름
                    if auto.get(f) == Some(&name) {
                        auto.insert(t, name.clone());
                    }
                    self.speakers.insert(t, name);
                }
            }
        }
        self.speakers.retain(|k, _| used.contains(k));
        auto.retain(|l, n| self.speakers.get(l) == Some(n));

        // 화자별 목소리: 지금 문장 배정대로 모은다
        let finals = self.speaker_voices(utterances);

        // 목소리로 이름 붙이기 (회의 전체 기준)
        let mut named = 0;
        if !known.is_empty() {
            let matched: BTreeMap<u32, String> = match_voices(&finals, known, NAME_MIN_SECS).into_iter().collect();
            // 사람이 붙인 이름은 다른 화자가 가져가지 못한다
            let manual: BTreeSet<String> =
                self.speakers.iter().filter(|(l, _)| !auto.contains_key(l)).map(|(_, n)| n.clone()).collect();
            for v in &finals {
                let cur = self.speakers.get(&v.speaker).cloned();
                if cur.is_some() && !auto.contains_key(&v.speaker) {
                    continue;
                }
                match matched.get(&v.speaker).filter(|n| !manual.contains(*n)) {
                    Some(n) => {
                        if cur.as_ref() != Some(n) {
                            if cur.is_none() {
                                named += 1;
                            }
                            self.speakers.insert(v.speaker, n.clone());
                        }
                    }
                    None => {
                        if cur.is_some() {
                            self.speakers.remove(&v.speaker);
                        }
                    }
                }
            }
        }

        // 번호 다시 매기기 (사용자가 새로 나눈 100번대 화자는 그대로)
        let renum: BTreeMap<u32, u32> = used
            .iter()
            .filter(|&&l| l < 100)
            .enumerate()
            .map(|(i, &l)| (l, i as u32))
            .collect();
        let re = |l: u32| renum.get(&l).copied().unwrap_or(l);
        for u in self.utterances.iter_mut() {
            u.speaker = u.speaker.map(re);
        }
        self.speakers = std::mem::take(&mut self.speakers).into_iter().map(|(k, v)| (re(k), v)).collect();
        // 모든 문장이 최종 화자로 정리됐으므로 합친 기록은 더 필요 없다
        self.speaker_map.clear();
        self.speaker_rev += 1;
        // 화면에 보이는 것(문장마다 화자 이름)이 바뀌었을 때만 되돌릴 거리를 남긴다
        let shown = |snap: &SpeakerSnapshot| -> Vec<String> {
            let by_id: HashMap<u64, u32> = snap.assign.iter().copied().collect();
            let mut d = SessionDoc {
                speakers: snap.speakers.clone(),
                utterances: self.utterances.clone(),
                ..Default::default()
            };
            for u in d.utterances.iter_mut() {
                u.speaker = by_id.get(&u.id).copied();
            }
            d.utterances.iter().map(|u| d.speaker_name(u.speaker)).collect()
        };
        let changed = shown(&before) != shown(&self.speaker_snapshot());
        self.before_refine = changed.then_some(before);
        Refined {
            moved,
            speakers: finals.len(),
            named,
        }
    }

    /// 지금의 화자 배정 · 이름 (합친 화자는 따라가서)
    pub fn speaker_snapshot(&self) -> SpeakerSnapshot {
        SpeakerSnapshot {
            speakers: self.speakers.clone(),
            assign: self.utterances.iter().filter_map(|u| Some((u.id, self.resolve(u.speaker)?))).collect(),
        }
    }

    /// 회의 후 화자 다시 정리를 되돌린다: 녹음 중에 정해진 화자 배정 · 이름으로. 되돌릴 것이 없으면 false.
    /// 글자 고친 것 · 지운 문장은 그대로 두고, 정리 뒤에 사람이 바꾼 화자 배정 · 이름은 정리 전 것으로 돌아간다.
    pub fn undo_refinement(&mut self) -> bool {
        let Some(snap) = self.before_refine.take() else {
            return false;
        };
        let by_id: HashMap<u64, u32> = snap.assign.iter().copied().collect();
        for u in self.utterances.iter_mut() {
            if let Some(&s) = by_id.get(&u.id) {
                u.speaker = Some(s);
            }
        }
        self.speakers = snap.speakers;
        self.speaker_map.clear();
        // 되돌리기 전에 만든 화면 사본이 늦게 저장돼도 되돌린 배정을 덮지 않게
        self.speaker_rev += 1;
        true
    }

    /// 화자별 목소리: 문장별 목소리를 지금의 화자 배정(합친 화자 포함)대로 모은다.
    /// 기록에 없는(지운) 문장 · 화자가 없는 문장은 뺀다.
    pub fn speaker_voices(&self, utterances: &[UtteranceVoice]) -> Vec<SpeakerVoice> {
        let by_id: HashMap<u64, &UtteranceVoice> = utterances.iter().map(|u| (u.id, u)).collect();
        let mut sums: BTreeMap<u32, (Vec<f32>, f32)> = BTreeMap::new();
        for u in &self.utterances {
            let (Some(spk), Some(v)) = (self.resolve(u.speaker), by_id.get(&u.id)) else {
                continue;
            };
            let e = sums.entry(spk).or_insert_with(|| (vec![0.0; v.embedding.len()], 0.0));
            if e.0.len() != v.embedding.len() {
                continue;
            }
            let secs = v.dur_ms as f32 / 1000.0;
            let w = secs.clamp(0.3, 6.0);
            e.0.iter_mut().zip(&v.embedding).for_each(|(a, b)| *a += b * w);
            e.1 += secs;
        }
        sums.into_iter()
            .map(|(speaker, (mut e, secs))| {
                let n = e.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-8);
                e.iter_mut().for_each(|x| *x /= n);
                SpeakerVoice {
                    speaker,
                    embedding: e,
                    secs,
                }
            })
            .collect()
    }

    /// 이름 붙은 화자의 목소리 (목소리 기억에 넣을 것): (이름, 목소리, 말한 초).
    /// 문장별 목소리를 지금의 화자 배정대로 모으므로, 사람이 문단을 옮기거나 화자를 합친 결과를 따른다.
    pub fn named_voices(&self, utterances: &[UtteranceVoice]) -> Vec<(String, Vec<f32>, f32)> {
        self.speaker_voices(utterances)
            .into_iter()
            .filter_map(|v| Some((self.speakers.get(&v.speaker)?.clone(), v.embedding, v.secs)))
            .collect()
    }

    pub fn speaker_name(&self, s: Option<u32>) -> String {
        match s {
            Some(n) => self
                .speakers
                .get(&n)
                .cloned()
                .unwrap_or_else(|| {
                    // 사용자가 나눈 새 화자(100번부터)는 자동 화자 번호 뒤에 이어 붙인다
                    if n >= 100 {
                        let auto = self
                            .utterances
                            .iter()
                            .filter_map(|u| u.speaker)
                            .filter(|&s| s < 100)
                            .map(|s| s + 1)
                            .max()
                            .unwrap_or(0);
                        format!("화자 {}", auto + n - 99)
                    } else {
                        format!("화자 {}", n + 1)
                    }
                }),
            None => "발화".into(),
        }
    }

    /// 기록으로 남길 내용이 있나: 받아 적은 문장 · 직접 쓴 메모 · 요약
    /// (문장이 하나도 받아 적히지 않았어도 회의 중 쓴 메모는 버리지 않는다)
    pub fn has_content(&self) -> bool {
        !self.utterances.is_empty()
            || !self.notes.trim().is_empty()
            || self.summary.as_deref().is_some_and(|s| !s.trim().is_empty())
    }

    pub fn upsert(&mut self, u: Utterance) {
        self.duration_ms = self.duration_ms.max(u.end_ms);
        match self.utterances.iter_mut().find(|x| x.id == u.id) {
            Some(x) => *x = u,
            None => {
                let pos = self.utterances.partition_point(|x| x.start_ms <= u.start_ms);
                self.utterances.insert(pos, u);
            }
        }
    }

    fn speaker_count(&self) -> usize {
        let mut v: Vec<u32> = self.utterances.iter().filter_map(|u| u.speaker).collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    }

    /// 연속된 같은 화자 발화를 한 문단으로 묶는다 (화자, 시작, 끝, 글, 문단에 든 발화 id)
    fn paragraphs(&self) -> Vec<(Option<u32>, u64, u64, String, Vec<u64>)> {
        let mut out: Vec<(Option<u32>, u64, u64, String, Vec<u64>)> = Vec::new();
        for u in &self.utterances {
            if let Some(last) = out.last_mut() {
                if last.0 == u.speaker && u.start_ms.saturating_sub(last.2) < 15_000 {
                    last.2 = u.end_ms;
                    last.3.push(' ');
                    last.3.push_str(&u.text);
                    last.4.push(u.id);
                    continue;
                }
            }
            out.push((u.speaker, u.start_ms, u.end_ms, u.text.clone(), vec![u.id]));
        }
        out
    }

    /// 메모의 코멘트와 그 코멘트가 붙는 문장 id — 그 시각에 말하던 문장, 말 사이 빈 곳이면 앞뒤 중 가까운 문장.
    /// 시각은 초 단위라 `[20:35]` 는 20:35.000 ~ 20:35.999 를 뜻한다 (문장에서 단 코멘트는 그 문장의 시작 초를
    /// 적으므로 다시 그 문장에 붙는다). 문장이 하나도 없으면 None.
    pub fn anchored_comments(&self) -> Vec<(NoteComment, Option<u64>)> {
        let blocks = note_comments(&self.notes);
        if blocks.is_empty() {
            return Vec::new();
        }
        let mut sorted: Vec<&Utterance> = self.utterances.iter().collect();
        sorted.sort_by_key(|u| (u.start_ms, u.id));
        blocks
            .into_iter()
            .map(|b| {
                let i = sorted.partition_point(|u| u.start_ms < b.ms + 1000);
                let prev = i.checked_sub(1).map(|k| sorted[k]);
                let at = match (prev, sorted.get(i)) {
                    (None, next) => next.map(|u| u.id),
                    (Some(p), None) => Some(p.id),
                    (Some(p), Some(n)) => Some(if p.end_ms >= b.ms || b.ms - p.end_ms <= n.start_ms - b.ms {
                        p.id
                    } else {
                        n.id
                    }),
                };
                (b, at)
            })
            .collect()
    }

    /// 예전 버전의 문장 코멘트(발화 id → 글)를 메모의 `[문장 시각] 글` 줄로 옮긴다 (시각 순서에 맞는 자리로).
    /// 이미 지운 문장의 코멘트 · 빈 코멘트는 버린다.
    pub fn fold_comments(&mut self) {
        if self.comments.is_empty() {
            return;
        }
        let starts: HashMap<u64, u64> = self.utterances.iter().map(|u| (u.id, u.start_ms)).collect();
        let mut items: Vec<(u64, u64, String)> = std::mem::take(&mut self.comments)
            .into_iter()
            .filter(|(_, c)| !c.trim().is_empty())
            .filter_map(|(id, c)| Some((*starts.get(&id)?, id, c)))
            .collect();
        items.sort();
        for (ms, _, c) in items {
            self.notes = insert_note_comment(&self.notes, ms, &c);
        }
    }

    pub fn to_markdown(&self) -> String {
        let mut s = format!("# {}\n\n", self.title);
        // 문장에 붙는 코멘트는 녹취록 그 문단 아래에만 — 메모 칸에는 나머지 줄만 (같은 글이 두 번 나오지 않게)
        let comments = self.anchored_comments();
        let mut by_id: HashMap<u64, Vec<&NoteComment>> = HashMap::new();
        let mut taken = vec![false; self.notes.split('\n').count()];
        for (c, at) in &comments {
            if let Some(id) = at {
                by_id.entry(*id).or_default().push(c);
                taken[c.from..c.to].iter_mut().for_each(|t| *t = true);
            }
        }
        let mut memo: Vec<&str> = Vec::new();
        for (line, _) in self.notes.split('\n').zip(&taken).filter(|(_, t)| !**t) {
            let line = line.trim_end();
            if line.is_empty() && memo.last().is_none_or(|l| l.is_empty()) {
                continue;
            }
            memo.push(line);
        }
        let memo = memo.join("\n");
        s.push_str(&format!(
            "- 일시: {}\n- 길이: {}\n- 참석(화자): {}\n- 엔진: {}\n\n",
            self.started_at,
            fmt_time(self.duration_ms),
            self.speaker_count(),
            self.engine
        ));
        if let Some(sum) = self.summary.as_deref().filter(|s| !s.trim().is_empty()) {
            s.push_str("## 요약\n\n");
            s.push_str(sum.trim());
            s.push_str("\n\n");
        }
        if !memo.trim().is_empty() {
            s.push_str("## 메모\n\n");
            s.push_str(memo.trim());
            s.push_str("\n\n");
        }
        s.push_str("## 녹취록\n\n");
        for (spk, start, _, text, ids) in self.paragraphs() {
            s.push_str(&format!("**{}** `{}`  \n{}\n\n", self.speaker_name(spk), fmt_time(start), text));
            // 문단 안 문장에 붙는 코멘트는 문단 바로 아래 인용으로 (메모에 적힌 시각을 붙여, 문장 순서 · 시각 순으로)
            for id in ids {
                let Some(list) = by_id.get_mut(&id) else {
                    continue;
                };
                list.sort_by_key(|c| (c.ms, c.from));
                for c in list.iter() {
                    let body = c.text.lines().collect::<Vec<_>>().join("  \n> ");
                    s.push_str(&format!("> 💬 `{}` {}\n\n", fmt_time(c.ms), body));
                }
            }
        }
        s
    }

    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for (spk, start, _, text, _) in self.paragraphs() {
            s.push_str(&format!("[{}] {}: {}\n", fmt_time(start), self.speaker_name(spk), text));
        }
        s
    }

    pub fn to_srt(&self) -> String {
        fn ts(ms: u64) -> String {
            format!(
                "{:02}:{:02}:{:02},{:03}",
                ms / 3_600_000,
                (ms / 60_000) % 60,
                (ms / 1000) % 60,
                ms % 1000
            )
        }
        let mut s = String::new();
        for (i, u) in self.utterances.iter().enumerate() {
            s.push_str(&format!(
                "{}\n{} --> {}\n{}: {}\n\n",
                i + 1,
                ts(u.start_ms),
                ts(u.end_ms),
                self.speaker_name(u.speaker),
                u.text
            ));
        }
        s
    }

    pub fn meta(&self) -> SessionMeta {
        let preview: String = self
            .utterances
            .iter()
            .map(|u| u.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect();
        SessionMeta {
            id: self.id.clone(),
            title: self.title.clone(),
            started_at: self.started_at.clone(),
            duration_ms: self.duration_ms,
            count: self.utterances.len(),
            speakers: self.speaker_count(),
            preview,
        }
    }
}

/// 회의 기록 폴더 관리 (기본: 문서\받아쓰기)
pub struct SessionStore {
    pub dir: PathBuf,
    pub current: Option<SessionDoc>,
    dirty: bool,
    last_save: Instant,
    last_md: Instant,
}

/// 녹음 중 자동 저장 간격: 기록 전체를 다시 쓰므로 회의가 길수록(문장이 많을수록) 덜 자주 — 2초에서 최대 10초.
/// (OneDrive 로 동기화되는 문서 폴더에서 몇 초마다 큰 파일을 다시 올리지 않게)
fn autosave_every(utterances: usize) -> Duration {
    Duration::from_millis((2_000 + utterances as u64 * 5).min(10_000))
}

/// 녹음 중 Markdown 사본은 이 간격으로만 (녹음이 끝나면 바로 최종본을 쓴다)
const MD_EVERY: Duration = Duration::from_secs(30);

impl SessionStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            current: None,
            dirty: false,
            last_save: Instant::now(),
            last_md: Instant::now(),
        }
    }

    fn json_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// 새 녹음 파일 경로 (Ogg Opus). 예전 기록의 `.wav` 는 `audio_file` 에 적힌 이름 그대로 읽는다.
    pub fn audio_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.opus"))
    }

    pub fn begin(&mut self, doc: SessionDoc) {
        self.current = Some(doc);
        self.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 자주 불러도 몇 초에 한 번만 실제로 쓴다 ([`autosave_every`]). Markdown 사본은 30초마다.
    pub fn autosave(&mut self) -> Result<()> {
        let n = self.current.as_ref().map_or(0, |d| d.utterances.len());
        if !self.dirty || self.last_save.elapsed() <= autosave_every(n) {
            return Ok(());
        }
        let Some(doc) = self.current.as_ref().filter(|d| d.has_content()) else {
            return Ok(());
        };
        let md = self.last_md.elapsed() >= MD_EVERY;
        let r = write_json(&self.dir, doc).and_then(|_| if md { write_md(&self.dir, doc) } else { Ok(()) });
        // 실패해도 간격만큼 뒤에 다시 시도한다 (매 발화마다 디스크를 두드리지 않게)
        self.last_save = Instant::now();
        if r.is_ok() {
            self.dirty = false;
            if md {
                self.last_md = Instant::now();
            }
        }
        r
    }

    pub fn flush(&mut self) -> Result<()> {
        let Some(doc) = self.current.as_ref() else {
            return Ok(());
        };
        if !doc.has_content() {
            return Ok(());
        }
        write_doc(&self.dir, doc)?;
        self.dirty = false;
        self.last_save = Instant::now();
        self.last_md = Instant::now();
        Ok(())
    }

    /// 기록 목록. 파일 크기 · 수정 시각이 그대로인 기록은 지난번에 읽은 요약을 다시 쓴다
    /// (긴 회의 기록 수백 개를 열 때마다 모두 다시 읽지 않게).
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut cache = META_CACHE.lock();
        let mut seen = BTreeSet::new();
        let mut out: Vec<SessionMeta> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|e| {
                let path = e.path();
                let md = e.metadata().ok()?;
                let stamp = (md.len(), md.modified().ok());
                seen.insert(path.clone());
                if let Some((st, meta)) = cache.get(&path) {
                    if *st == stamp {
                        return Some(meta.clone());
                    }
                }
                let doc = serde_json::from_str::<SessionDoc>(&std::fs::read_to_string(&path).ok()?).ok()?;
                let meta = doc.meta();
                cache.insert(path, (stamp, meta.clone()));
                Some(meta)
            })
            .collect();
        // 지운 기록 · 다른 폴더의 기록은 비운다
        cache.retain(|p, _| seen.contains(p) || p.parent() != Some(self.dir.as_path()));
        out.sort_by(|a, b| b.id.cmp(&a.id));
        out
    }

    pub fn load(&self, id: &str) -> Result<SessionDoc> {
        if let Some(cur) = self.current.as_ref().filter(|c| c.id == id) {
            return Ok(cur.clone());
        }
        let s = std::fs::read_to_string(self.json_path(safe_id(id)?))?;
        let mut doc: SessionDoc = serde_json::from_str(&s)?;
        doc.fold_comments(); // 예전 버전의 문장 코멘트 → 메모의 [시각] 줄
        Ok(doc)
    }

    pub fn save_doc(&mut self, mut doc: SessionDoc) -> Result<()> {
        safe_id(&doc.id)?;
        doc.fold_comments();
        if let Some(cur) = self.current.as_mut().filter(|c| c.id == doc.id) {
            // 녹음 중인 기록: 화면에서 고친 것만 반영하고, 그 사이 새로 들어온 발화는 지킨다
            // 화자 다시 정리 전에 만든 사본이면 화자 배정 · 이름은 정리된 것을 지킨다
            let stale = doc.speaker_rev < cur.speaker_rev;
            cur.title = doc.title.clone();
            cur.auto_title = doc.auto_title;
            if !stale {
                cur.speakers = doc.speakers.clone();
                cur.speaker_map = doc.speaker_map.clone();
            }
            cur.summary = doc.summary.clone();
            cur.summary_edited = doc.summary_edited;
            // 메모(코멘트 포함)는 화면에서만 고친다 → 화면 것을 그대로
            cur.notes = doc.notes.clone();
            // 화면이 지웠다고 알린 발화만 지우고, 그 사이 새로 들어온 발화는 지킨다
            // (긴 회의에서 편집할 때마다 문장 수의 제곱만큼 찾지 않게 id 로 한 번에)
            let deleted: std::collections::HashSet<u64> = doc.deleted.iter().copied().collect();
            let edited: HashMap<u64, &Utterance> = doc.utterances.iter().map(|u| (u.id, u)).collect();
            cur.utterances.retain_mut(|x| {
                if deleted.contains(&x.id) {
                    return false;
                }
                if let Some(u) = edited.get(&x.id) {
                    x.text = u.text.clone();
                    if !stale {
                        x.speaker = u.speaker;
                    }
                }
                true
            });
            // (지운 문장에 붙어 있던 코멘트는 메모의 시각 그대로라 가장 가까운 문장으로 옮겨 붙는다)
            let map = cur.speaker_map.clone();
            if !map.is_empty() {
                let resolved: Vec<Option<u32>> = cur.utterances.iter().map(|u| cur.resolve(u.speaker)).collect();
                for (u, s) in cur.utterances.iter_mut().zip(resolved) {
                    u.speaker = s;
                }
            }
            self.dirty = true;
            return self.flush_any();
        }
        // 이미 지운 기록은 되살리지 않는다 (지운 뒤 화면에 남은 사본을 고친 경우)
        let path = self.json_path(&doc.id);
        let Some(old) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<SessionDoc>(&t).ok())
        else {
            return Err(anyhow!("지운 기록이에요"));
        };
        let mut doc = doc;
        if doc.speaker_rev < old.speaker_rev {
            // 화자 다시 정리 전에 만든 화면 사본이 늦게 저장됨: 화자 배정 · 이름은 저장된(정리된) 것을 지키고
            // 제목 · 글자 · 삭제만 반영한다
            let by_id: HashMap<u64, Option<u32>> = old.utterances.iter().map(|u| (u.id, u.speaker)).collect();
            for u in doc.utterances.iter_mut() {
                if let Some(&s) = by_id.get(&u.id) {
                    u.speaker = s;
                }
            }
            doc.speakers = old.speakers.clone();
            doc.speaker_map = old.speaker_map.clone();
            doc.speaker_rev = old.speaker_rev;
            doc.before_refine = old.before_refine.clone();
        }
        // 화면 사본은 발화가 도착 순서로 쌓여 있을 수 있다 → 시간 순으로 저장 (MD·TXT·SRT 순서)
        doc.utterances.sort_by(|a, b| a.start_ms.cmp(&b.start_ms).then(a.id.cmp(&b.id)));
        doc.duration_ms = doc.duration_ms.max(old.duration_ms);
        write_doc(&self.dir, &doc)
    }

    /// 저장된 기록의 화자 다시 정리를 되돌리고 되돌린 기록을 돌려준다 (녹음 중인 기록은 끝난 뒤에)
    pub fn undo_refinement(&mut self, id: &str) -> Result<SessionDoc> {
        if self.current.as_ref().is_some_and(|c| c.id == id) {
            return Err(anyhow!("녹음이 끝난 뒤에 되돌릴 수 있어요"));
        }
        let mut doc = self.load(id)?;
        if !doc.undo_refinement() {
            return Err(anyhow!("되돌릴 화자 정리가 없어요"));
        }
        write_doc(&self.dir, &doc)?;
        Ok(doc)
    }

    /// 발화를 모두 지운 경우에도 저장 (flush 는 빈 기록을 건너뛴다)
    fn flush_any(&mut self) -> Result<()> {
        let Some(doc) = self.current.as_ref() else {
            return Ok(());
        };
        if !doc.has_content() {
            let p = self.json_path(&doc.id);
            if p.exists() {
                write_doc(&self.dir, doc)?;
            }
            return Ok(());
        }
        self.flush()
    }

    /// 제목·녹취록 전체에서 찾기 (대소문자 무시)
    pub fn search(&self, query: &str) -> Vec<SearchHit> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<SearchHit> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str::<SessionDoc>(&s).ok())
            .filter_map(|d| {
                let in_title = d.title.to_lowercase().contains(&q);
                let mut hits = 0;
                let mut snippet = String::new();
                for u in &d.utterances {
                    if u.text.to_lowercase().contains(&q) {
                        hits += 1;
                        if snippet.is_empty() {
                            snippet = format!("{} · {}", fmt_time(u.start_ms), around(&u.text, &q, 60));
                        }
                    }
                }
                // 직접 적은 메모 · 코멘트에서도 찾는다
                for c in d.comments.values() {
                    if c.to_lowercase().contains(&q) {
                        hits += 1;
                        if snippet.is_empty() {
                            snippet = format!("코멘트 · {}", around(c, &q, 60));
                        }
                    }
                }
                if d.notes.to_lowercase().contains(&q) {
                    hits += 1;
                    if snippet.is_empty() {
                        snippet = format!("메모 · {}", around(&d.notes, &q, 60));
                    }
                }
                if !in_title && hits == 0 {
                    return None;
                }
                Some(SearchHit {
                    meta: d.meta(),
                    snippet,
                    hits,
                })
            })
            .collect();
        out.sort_by(|a, b| b.meta.id.cmp(&a.meta.id));
        out
    }

    /// 딸린 파일부터 지우고 기록(`.json`)은 맨 나중에 — 녹음 파일이 다른 프로그램에 잡혀 있어(OneDrive · 백신 · 재생 중)
    /// 못 지우면 기록이 목록에 그대로 남아 나중에 다시 지울 수 있다 (녹음 파일만 고아로 남지 않게)
    pub fn delete(&mut self, id: &str) -> Result<()> {
        let id = safe_id(id)?;
        for ext in ["opus", "wav", "voices", "md", "json"] {
            let p = self.dir.join(format!("{id}.{ext}"));
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(anyhow!(
                        "{} 파일을 지우지 못했어요 — 다른 프로그램(OneDrive · 백신 · 재생 중인 플레이어)이 쓰고 있을 수 있어요. 잠시 뒤 다시 해 보세요. ({e})",
                        p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    ))
                }
            }
        }
        Ok(())
    }
}

/// 찾은 낱말 주변 n 글자
fn around(text: &str, q_lower: &str, n: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let q: Vec<char> = q_lower.chars().collect();
    // 소문자 변환으로 길이가 달라지는 드문 경우엔 앞부분만
    if lower.len() != chars.len() || q.is_empty() {
        return chars.iter().take(n).collect();
    }
    let pos = lower.windows(q.len()).position(|w| w == q.as_slice()).unwrap_or(0);
    let start = pos.saturating_sub(n / 3);
    let end = (start + n).min(chars.len());
    let mut s: String = chars[start..end].iter().collect();
    if start > 0 {
        s.insert(0, '…');
    }
    if end < chars.len() {
        s.push('…');
    }
    s
}

pub(crate) fn safe_id(id: &str) -> Result<&str> {
    if id.is_empty() || id.contains(['/', '\\', ':', '.']) {
        return Err(anyhow!("잘못된 기록 이름"));
    }
    Ok(id)
}

/// 기록 목록용 요약 캐시: 파일 경로 → ((크기, 수정 시각), 요약)
type MetaCache = HashMap<PathBuf, ((u64, Option<SystemTime>), SessionMeta)>;
static META_CACHE: std::sync::LazyLock<Mutex<MetaCache>> = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 기록(JSON) + 읽기용 Markdown
pub(crate) fn write_doc(dir: &Path, doc: &SessionDoc) -> Result<()> {
    write_json(dir, doc)?;
    write_md(dir, doc)
}

fn write_json(dir: &Path, doc: &SessionDoc) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = dir.join(format!("{}.json", doc.id));
    let tmp = dir.join(format!("{}.json.tmp", doc.id));
    {
        // 디스크에 다 쓴 뒤에 바꿔치기한다 — 갑자기 전원이 꺼져도 빈 파일이 남지 않게
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(doc)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &json)?;
    Ok(())
}

fn write_md(dir: &Path, doc: &SessionDoc) -> Result<()> {
    std::fs::write(dir.join(format!("{}.md", doc.id)), doc.to_markdown())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(id: u64, start: u64, speaker: u32) -> Utterance {
        Utterance {
            id,
            start_ms: start,
            end_ms: start + 4000,
            speaker: Some(speaker),
            text: format!("문장 {id}"),
        }
    }

    fn uv(id: u64, e: [f32; 3]) -> UtteranceVoice {
        let n = e.iter().map(|x| x * x).sum::<f32>().sqrt();
        UtteranceVoice {
            id,
            embedding: e.iter().map(|x| x / n).collect(),
            dur_ms: 4000,
        }
    }

    #[test]
    fn refinement_keeps_manual_edits_and_names_and_renumbers() {
        let mut d = SessionDoc::new("로컬".into());
        // 실시간: 0, 1, 2, 3 — 2는 사실 0과 같은 사람, 3번 화자 문장 하나는 사람이 100번(새 화자)으로 옮겨 둠
        d.utterances = vec![u(0, 0, 0), u(1, 5000, 1), u(2, 10000, 2), u(3, 15000, 3), u(4, 20000, 100), u(5, 25000, 1)];
        d.speakers.insert(2, "김 팀장".into());
        let labels = [[0, 0, 0], [1, 1, 1], [2, 2, 0], [3, 3, 3], [4, 3, 1], [5, 1, 1]];
        let r = d.apply_refinement(&labels, &[], &BTreeMap::new(), &[]);
        assert_eq!(r.moved, 1);
        let spk: Vec<Option<u32>> = d.utterances.iter().map(|u| u.speaker).collect();
        // 0,2 → 0 · 1 → 1 · 3 → 2 (번호를 빈틈없이) · 사람이 옮긴 100번은 그대로
        assert_eq!(spk, vec![Some(0), Some(1), Some(0), Some(2), Some(100), Some(1)]);
        // 사라진 2번 화자의 이름은 합쳐진 0번이 이어받는다
        assert_eq!(d.speakers.get(&0).map(String::as_str), Some("김 팀장"));
    }

    #[test]
    fn refinement_follows_merges_made_during_recording() {
        let mut d = SessionDoc::new("로컬".into());
        // 녹음 중에 사람이 1번을 0번에 합쳐 둠 → 다시 정리도 합친 채로 계산해 둘을 같은 곳(2번)으로 보낸다
        d.speaker_map.insert(1, 0);
        d.utterances = vec![u(0, 0, 0), u(1, 5000, 0), u(2, 10000, 2), u(3, 15000, 0)];
        let labels = [[0, 0, 2], [1, 1, 2], [2, 2, 2], [3, 0, 0]];
        let r = d.apply_refinement(&labels, &[], &BTreeMap::new(), &[]);
        assert_eq!(r.moved, 2);
        // 2번과 합쳐진 0·1 은 함께 0번(다시 매긴 번호)으로, 따로 남은 문장 3은 1번
        assert_eq!(
            d.utterances.iter().map(|u| u.speaker).collect::<Vec<_>>(),
            vec![Some(1), Some(1), Some(1), Some(0)]
        );
        assert!(d.speaker_map.is_empty());
    }

    #[test]
    fn refinement_names_by_voice_but_never_over_a_manual_name() {
        let known = vec![
            KnownVoice {
                name: "김 팀장".into(),
                embedding: vec![1.0, 0.0, 0.0],
            },
            KnownVoice {
                name: "박 대리".into(),
                embedding: vec![0.0, 1.0, 0.0],
            },
            KnownVoice {
                name: "이 과장".into(),
                embedding: vec![0.0, 0.0, 1.0],
            },
        ];
        let mut d = SessionDoc::new("로컬".into());
        d.utterances = vec![u(0, 0, 0), u(1, 5000, 0), u(2, 10000, 1), u(3, 15000, 1), u(4, 20000, 2), u(5, 25000, 2)];
        // 0번: 녹음 중 목소리로 "박 대리"라고 붙였지만 회의 전체로 보면 김 팀장 · 2번: 사람이 직접 "최 부장"
        d.speakers.insert(0, "박 대리".into());
        d.speakers.insert(2, "최 부장".into());
        let voices = [
            uv(0, [1.0, 0.05, 0.0]),
            uv(1, [1.0, 0.0, 0.05]),
            uv(2, [0.0, 1.0, 0.1]),
            uv(3, [0.05, 1.0, 0.0]),
            uv(4, [0.0, 0.0, 1.0]),
            uv(5, [0.0, 0.1, 1.0]),
        ];
        let auto: BTreeMap<u32, String> = [(0, "박 대리".to_string())].into();
        let labels = [[0, 0, 0], [1, 0, 0], [2, 1, 1], [3, 1, 1], [4, 2, 2], [5, 2, 2]];
        let r = d.apply_refinement(&labels, &voices, &auto, &known);
        assert_eq!(d.speakers.get(&0).map(String::as_str), Some("김 팀장"));
        assert_eq!(d.speakers.get(&1).map(String::as_str), Some("박 대리"));
        assert_eq!(d.speakers.get(&2).map(String::as_str), Some("최 부장"));
        assert_eq!(r.named, 1);
        // 이름 붙은 화자의 목소리 → 목소리 기억으로
        let named: Vec<String> = d.named_voices(&voices).into_iter().map(|x| x.0).collect();
        assert_eq!(named, vec!["김 팀장", "박 대리", "최 부장"]);
    }

    #[test]
    fn a_name_the_user_fixed_or_typed_is_never_taken_back() {
        let known = vec![KnownVoice {
            name: "앨리스".into(),
            embedding: vec![0.0, 0.0, 1.0],
        }];
        let mut d = SessionDoc::new("로컬".into());
        d.utterances = vec![u(0, 0, 0), u(1, 5000, 0), u(2, 10000, 1)];
        // 녹음 중 0번에 목소리로 "앨리스"가 붙었는데, 사람이 0번을 "밥"으로 고치고 1번(말이 짧음)에 "앨리스"를 직접 붙임
        d.speakers.insert(0, "밥".into());
        d.speakers.insert(1, "앨리스".into());
        let auto: BTreeMap<u32, String> = [(0, "앨리스".to_string())].into();
        let voices = [uv(0, [1.0, 0.0, 0.0]), uv(1, [1.0, 0.1, 0.0])];
        let labels = [[0, 0, 0], [1, 0, 0], [2, 1, 1]];
        d.apply_refinement(&labels, &voices, &auto, &known);
        assert_eq!(d.speakers.get(&0).map(String::as_str), Some("밥"));
        assert_eq!(d.speakers.get(&1).map(String::as_str), Some("앨리스"));
    }

    #[test]
    fn a_stale_screen_copy_cannot_undo_refinement() {
        let dir = std::env::temp_dir().join(format!("bd-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SessionStore::new(dir.clone());
        let mut d = SessionDoc::new("로컬".into());
        d.id = "2026-11-15_140000".into();
        d.utterances = vec![u(0, 0, 0), u(1, 5000, 1), u(2, 10000, 2)];
        // 화면이 녹음 중에 들고 있던 사본 (정리 전)
        let mut screen = d.clone();
        store.begin(d);
        // 회의가 끝나 다시 정리: 2번은 0번과 같은 사람
        store.current.as_mut().unwrap().apply_refinement(&[[0, 0, 0], [1, 1, 1], [2, 2, 0]], &[], &BTreeMap::new(), &[]);
        // 그 순간 사람이 제목 · 글자를 고친 사본이 늦게 저장됨 (녹음 중 경로)
        screen.title = "고친 제목".into();
        screen.utterances[1].text = "고친 글자".into();
        store.save_doc(screen.clone()).unwrap();
        let cur = store.current.clone().unwrap();
        assert_eq!(cur.utterances.iter().map(|u| u.speaker).collect::<Vec<_>>(), vec![Some(0), Some(1), Some(0)]);
        assert_eq!((cur.title.as_str(), cur.utterances[1].text.as_str()), ("고친 제목", "고친 글자"));
        // 녹음이 끝난 뒤(저장된 기록) 경로도 마찬가지
        store.flush().unwrap();
        store.current = None;
        screen.utterances[0].text = "또 고침".into();
        store.save_doc(screen).unwrap();
        let saved = store.load("2026-11-15_140000").unwrap();
        assert_eq!(saved.utterances.iter().map(|u| u.speaker).collect::<Vec<_>>(), vec![Some(0), Some(1), Some(0)]);
        assert_eq!(saved.utterances[0].text, "또 고침");
        assert_eq!(saved.speaker_rev, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refinement_can_be_undone_back_to_the_live_speakers() {
        let dir = std::env::temp_dir().join(format!("bd-undo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SessionStore::new(dir.clone());
        let mut d = SessionDoc::new("로컬".into());
        d.id = "2026-11-16_135933".into();
        // 녹음 중: 다섯 명, 넷에 이름 · 4번은 사람이 2번에 합쳐 둠
        d.utterances = vec![u(0, 0, 0), u(1, 5000, 1), u(2, 10000, 2), u(3, 15000, 3), u(4, 20000, 2), u(5, 25000, 0)];
        d.speaker_map.insert(4, 2);
        for (k, n) in [(0, "이낙원 실장님"), (1, "가"), (2, "이도현 매니저"), (3, "나")] {
            d.speakers.insert(k, n.into());
        }
        let screen = d.clone();
        store.begin(d);
        // 다시 정리가 모두 둘로 합쳐 버림
        let labels = [[0, 0, 2], [1, 1, 0], [2, 2, 2], [3, 3, 0], [4, 4, 2], [5, 0, 0]];
        store.current.as_mut().unwrap().apply_refinement(&labels, &[], &BTreeMap::new(), &[]);
        store.flush().unwrap();
        store.current = None;
        let refined = store.load("2026-11-16_135933").unwrap();
        assert_eq!(refined.speakers.len(), 2);
        assert!(refined.before_refine.is_some());
        // 정리 전 화면 사본이 늦게 저장돼도 되돌릴 거리는 남는다
        let mut late = screen.clone();
        late.utterances[1].text = "고친 글자".into();
        store.save_doc(late).unwrap();
        assert!(store.load("2026-11-16_135933").unwrap().before_refine.is_some());

        let back = store.undo_refinement("2026-11-16_135933").unwrap();
        assert_eq!(
            back.utterances.iter().map(|u| u.speaker).collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(2), Some(3), Some(2), Some(0)]
        );
        assert_eq!(back.speakers, screen.speakers);
        assert_eq!(back.utterances[1].text, "고친 글자");
        assert!(back.before_refine.is_none());
        assert_eq!(back.speaker_rev, refined.speaker_rev + 1);
        // 되돌리기 전에 들고 있던 화면 사본(정리된 것)이 늦게 저장돼도 되돌린 배정을 덮지 않는다
        store.save_doc(refined).unwrap();
        let saved = store.load("2026-11-16_135933").unwrap();
        assert_eq!(saved.speakers, screen.speakers);
        assert_eq!(saved.utterances[3].speaker, Some(3));
        assert!(store.undo_refinement("2026-11-16_135933").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_to_undo_when_refinement_changed_nothing() {
        let mut d = SessionDoc::new("로컬".into());
        d.utterances = vec![u(0, 0, 0), u(1, 5000, 1)];
        d.speakers.insert(1, "박 대리".into());
        d.apply_refinement(&[[0, 0, 0], [1, 1, 1]], &[], &BTreeMap::new(), &[]);
        assert!(d.before_refine.is_none());
        assert!(!d.undo_refinement());
    }

    #[test]
    fn voices_follow_utterances_moved_by_hand() {
        // 실시간 0번이었던 문장 둘 중 하나를 사람이 새 화자(100번)로 옮기고 이름을 붙임
        let mut d = SessionDoc::new("로컬".into());
        d.utterances = vec![u(0, 0, 0), u(1, 5000, 100), u(2, 10000, 0)];
        d.speakers.insert(0, "김 팀장".into());
        d.speakers.insert(100, "박 대리".into());
        let voices = [uv(0, [1.0, 0.0, 0.0]), uv(1, [0.0, 1.0, 0.0]), uv(2, [1.0, 0.1, 0.0])];
        let named = d.named_voices(&voices);
        let get = |n: &str| named.iter().find(|x| x.0 == n).unwrap();
        // 박 대리의 목소리는 옮긴 문장에서, 김 팀장 목소리에는 박 대리 문장이 섞이지 않는다
        assert!(get("박 대리").1[1] > 0.99);
        assert!(get("김 팀장").1[1] < 0.1);
        assert!((get("김 팀장").2 - 8.0).abs() < 1e-3);
        // 지운 문장의 목소리는 빠진다
        d.utterances.retain(|u| u.id != 1);
        assert!(d.named_voices(&voices).iter().all(|x| x.0 != "박 대리"));
    }

    #[test]
    fn autosave_slows_down_for_long_meetings() {
        assert_eq!(autosave_every(0), Duration::from_secs(2));
        assert!(autosave_every(400) > Duration::from_secs(3));
        assert_eq!(autosave_every(5000), Duration::from_secs(10));
    }

    #[test]
    fn list_reuses_unchanged_records() {
        let dir = std::env::temp_dir().join(format!("bd-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut d = SessionDoc::new("로컬".into());
        d.id = "2026-11-15_090000".into();
        d.utterances = vec![u(0, 0, 0)];
        write_doc(&dir, &d).unwrap();
        let mut store = SessionStore::new(dir.clone());
        assert_eq!(store.list()[0].count, 1);
        d.utterances.push(u(1, 5000, 1));
        write_doc(&dir, &d).unwrap();
        // 크기가 바뀌었으니 다시 읽는다
        assert_eq!(store.list()[0].count, 2);
        store.delete(&d.id).unwrap();
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notes_and_comments_are_saved_while_recording_and_after() {
        let dir = std::env::temp_dir().join(format!("bd-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SessionStore::new(dir.clone());
        let mut d = SessionDoc::new("로컬".into());
        d.id = "2026-11-16_100000".into();
        d.utterances = vec![u(0, 0, 0), u(1, 5000, 1)];
        let mut screen = d.clone();
        store.begin(d);
        // 녹음 중: 화면에서 메모(시각 줄 = 코멘트) · 요약을 고침 (그 사이 새 문장이 들어옴)
        store.current.as_mut().unwrap().upsert(u(2, 10000, 0));
        screen.notes = "결제 일정 다시 확인\n[00:05] 숫자 확인 필요".into();
        screen.summary = Some("직접 쓴 요약".into());
        screen.summary_edited = true;
        store.save_doc(screen.clone()).unwrap();
        let cur = store.current.clone().unwrap();
        assert_eq!(cur.utterances.len(), 3);
        assert_eq!(cur.notes, "결제 일정 다시 확인\n[00:05] 숫자 확인 필요");
        assert_eq!(cur.anchored_comments()[0].1, Some(1));
        assert!(cur.summary_edited);
        // 문장을 지워도 코멘트는 메모에 남아 가장 가까운 문장으로 옮겨 붙는다 (00:05 → 00:00~00:04 문장)
        screen.utterances.retain(|x| x.id != 1);
        screen.deleted = vec![1];
        store.save_doc(screen.clone()).unwrap();
        let cur = store.current.clone().unwrap();
        assert_eq!(cur.anchored_comments()[0].1, Some(0));
        // 녹음이 끝난 뒤(저장된 기록)에도
        store.flush().unwrap();
        store.current = None;
        screen.deleted.clear();
        screen.notes.push_str("\n\n[00:00] 첫 문장\n두 줄\n\n- 다음 회의 때 확인");
        store.save_doc(screen).unwrap();
        let saved = store.load("2026-11-16_100000").unwrap();
        assert!(saved.comments.is_empty());
        let md = saved.to_markdown();
        // 메모 칸에는 시각 없는 줄만, 시각 줄은 녹취록 그 문단 아래에만
        assert!(md.contains("## 메모\n\n결제 일정 다시 확인\n\n- 다음 회의 때 확인\n\n## 녹취록"));
        assert!(md.contains("> 💬 `00:00` 첫 문장  \n> 두 줄\n\n> 💬 `00:05` 숫자 확인 필요"));
        assert_eq!(md.matches("숫자 확인 필요").count(), 1);
        // 요약에 보내는 녹취록에는 메모 · 코멘트가 섞이지 않는다
        assert!(!saved.to_text().contains("첫 문장"));
        // 메모 · 코멘트로도 찾는다
        assert_eq!(store.search("두 줄").len(), 1);
        assert_eq!(store.search("결제 일정").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memo_lines_with_a_time_are_comments() {
        assert_eq!(parse_stamp("[20:35] 인수인계"), Some((1_235_000, "인수인계")));
        assert_eq!(parse_stamp("  - [1:02:03]  둘"), Some((3_723_000, " 둘")));
        assert_eq!(parse_stamp("* [75:00]"), Some((4_500_000, "")));
        for no in ["- [ ] 할 일", "[20:5] 짧음", "[20:61] 초 넘침", "[1:60:00] 분 넘침", "글 [20:35]", "-[20:35] 붙음", "[ab:cd]"] {
            assert_eq!(parse_stamp(no), None, "{no}");
        }
        let notes = "회의 전체 메모\n[00:07] 결제 모듈\n- 금요일 공유\n[00:35] 인덱스\n\n- 피드백 양식\n[00:40]\n\n[00:50]\n이어서 적음";
        let c = note_comments(notes);
        let got: Vec<(u64, usize, usize, &str)> = c.iter().map(|c| (c.ms, c.from, c.to, c.text.as_str())).collect();
        // 빈 줄 · 다음 시각 줄에서 끝나고, 글이 없는 시각 줄([00:40])은 코멘트가 아니다
        assert_eq!(
            got,
            vec![
                (7000, 1, 3, "결제 모듈\n- 금요일 공유"),
                (35000, 3, 4, "인덱스"),
                (50000, 8, 10, "이어서 적음")
            ]
        );
    }

    #[test]
    fn a_memo_comment_sticks_to_the_sentence_spoken_at_that_time() {
        let mut d = SessionDoc::new("로컬".into());
        // 0: 0~4초 · 1: 5.4~9.4초 · 2: 30~34초
        d.utterances = vec![u(0, 0, 0), u(1, 5400, 1), u(2, 30000, 0)];
        d.notes = "[00:05] 문장에서 단 코멘트(시작 초)\n[00:08] 말하는 중\n[00:12] 빈 곳 · 앞이 가까움\n[00:25] 빈 곳 · 뒤가 가까움\n[01:00] 끝 뒤".into();
        let at: Vec<Option<u64>> = d.anchored_comments().into_iter().map(|x| x.1).collect();
        assert_eq!(at, vec![Some(1), Some(1), Some(1), Some(2), Some(2)]);
        // 문장이 아직 없으면(녹음 직후) 붙을 곳이 없다
        d.utterances.clear();
        assert_eq!(d.anchored_comments()[0].1, None);
    }

    #[test]
    fn a_new_comment_goes_into_the_memo_in_time_order() {
        // 메모가 비었으면 그대로 · 끝 줄바꿈은 끝에 둔다
        assert_eq!(insert_note_comment("", 7000, "결제"), "[00:07] 결제");
        assert_eq!(insert_note_comment("자유 메모\n", 7000, "결제"), "자유 메모\n[00:07] 결제\n");
        let notes = "자유 메모\n[00:10] 둘째\n이어짐\n\n[00:30] 넷째\n- 목록";
        // 같거나 이른 코멘트 중 마지막 것 뒤 (그 코멘트의 이어진 줄 뒤)
        assert_eq!(
            insert_note_comment(notes, 20_000, "셋째\n\n여러 줄"),
            "자유 메모\n[00:10] 둘째\n이어짐\n[00:20] 셋째\n여러 줄\n\n[00:30] 넷째\n- 목록"
        );
        // 가장 이르면 첫 코멘트 바로 앞 · 가장 늦으면 마지막 코멘트 뒤
        assert_eq!(insert_note_comment(notes, 1000, "첫째"), "자유 메모\n[00:01] 첫째\n[00:10] 둘째\n이어짐\n\n[00:30] 넷째\n- 목록");
        assert_eq!(insert_note_comment(notes, 40_000, "다섯째"), format!("{notes}\n[00:40] 다섯째"));
        // 넣은 코멘트는 다시 읽어도 같은 글
        let back = note_comments(&insert_note_comment(notes, 20_000, "셋째\n여러 줄"));
        assert_eq!(back[1].text, "셋째\n여러 줄");
        // 빈 글은 넣지 않는다
        assert_eq!(insert_note_comment(notes, 20_000, " \n "), notes);
    }

    #[test]
    fn old_sentence_comments_move_into_the_memo() {
        let old = r#"{"version":1,"id":"2026-11-01_090000","title":"t","notes":"[00:07] 메모에 적은 것\n\n- 자유 메모",
            "utterances":[{"id":0,"startMs":1000,"endMs":5000,"speaker":0,"text":"a"},{"id":1,"startMs":65000,"endMs":70000,"speaker":1,"text":"b"}],
            "comments":{"1":"늦은 코멘트","0":"이른 코멘트","9":"지운 문장","2":"  "}}"#;
        let dir = std::env::temp_dir().join(format!("bd-fold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-11-01_090000.json"), old).unwrap();
        let mut store = SessionStore::new(dir.clone());
        let d = store.load("2026-11-01_090000").unwrap();
        assert!(d.comments.is_empty());
        assert_eq!(d.notes, "[00:01] 이른 코멘트\n[00:07] 메모에 적은 것\n[01:05] 늦은 코멘트\n\n- 자유 메모");
        let at: Vec<Option<u64>> = d.anchored_comments().into_iter().map(|x| x.1).collect();
        assert_eq!(at, vec![Some(0), Some(0), Some(1)]);
        // 저장하면 파일에서도 코멘트 칸이 사라진다
        store.save_doc(d).unwrap();
        let json = std::fs::read_to_string(dir.join("2026-11-01_090000.json")).unwrap();
        assert!(!json.contains("\"comments\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notes_keep_a_meeting_that_has_no_recognized_speech() {
        let dir = std::env::temp_dir().join(format!("bd-notes-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut store = SessionStore::new(dir.clone());
        let mut d = SessionDoc::new("로컬".into());
        d.id = "2026-11-16_110000".into();
        let mut screen = d.clone();
        store.begin(d);
        // 받아 적힌 문장이 하나도 없는 녹음은 남기지 않지만
        store.flush().unwrap();
        assert!(store.load("2026-11-16_110000").is_ok()); // 녹음 중에는 메모리에서 읽힘
        assert!(!dir.join("2026-11-16_110000.json").exists());
        // 메모를 쓰면 문장이 없어도 저장된다
        screen.notes = "인식이 안 돼서 손으로 적음".into();
        store.save_doc(screen).unwrap();
        assert!(dir.join("2026-11-16_110000.json").exists());
        store.current = None;
        assert_eq!(store.load("2026-11-16_110000").unwrap().notes, "인식이 안 돼서 손으로 적음");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_records_without_notes_still_load_and_stay_clean() {
        let old = r#"{"version":1,"id":"2026-11-01_090000","title":"t","utterances":[{"id":0,"startMs":0,"endMs":10,"speaker":0,"text":"a"}]}"#;
        let d: SessionDoc = serde_json::from_str(old).unwrap();
        assert!(d.notes.is_empty() && d.comments.is_empty() && !d.summary_edited);
        // 비어 있으면 파일에 쓰지 않는다 (예전 버전이 읽어도 그대로)
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("notes") && !json.contains("comments") && !json.contains("summaryEdited"));
    }
}
