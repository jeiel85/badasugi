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
    /// - `known` 목소리와 닮은 화자에 이름을 붙인다. 녹음 중에 목소리로 붙였던 이름(`auto_named`)은
    ///   회의 전체로 다시 보고 아니면 거둔다. 사람이 붙인 이름은 그대로.
    /// - 화자 번호를 1부터 빈틈없이 다시 매긴다 (합쳐져 "화자 3" 이 비는 일 없게).
    pub fn apply_refinement(
        &mut self,
        labels: &[[u64; 3]],
        utterances: &[UtteranceVoice],
        auto_named: &BTreeSet<String>,
        known: &[KnownVoice],
    ) -> Refined {
        let smap = self.speaker_map.clone();
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
                    self.speakers.insert(t, name);
                }
            }
        }
        self.speakers.retain(|k, _| used.contains(k));

        // 화자별 목소리: 지금 문장 배정대로 모은다
        let finals = self.speaker_voices(utterances);

        // 목소리로 이름 붙이기 (회의 전체 기준)
        let mut named = 0;
        if !known.is_empty() {
            let matched: BTreeMap<u32, String> = match_voices(&finals, known, NAME_MIN_SECS).into_iter().collect();
            let is_auto = |n: &String| auto_named.contains(n);
            // 사람이 붙인 이름은 다른 화자가 가져가지 못한다
            let manual: BTreeSet<String> = self.speakers.values().filter(|n| !is_auto(n)).cloned().collect();
            for v in &finals {
                let cur = self.speakers.get(&v.speaker).cloned();
                if cur.as_ref().is_some_and(|n| !is_auto(n)) {
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
        Refined {
            moved,
            speakers: finals.len(),
            named,
        }
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

    /// 연속된 같은 화자 발화를 한 문단으로 묶는다
    fn paragraphs(&self) -> Vec<(Option<u32>, u64, u64, String)> {
        let mut out: Vec<(Option<u32>, u64, u64, String)> = Vec::new();
        for u in &self.utterances {
            if let Some(last) = out.last_mut() {
                if last.0 == u.speaker && u.start_ms.saturating_sub(last.2) < 15_000 {
                    last.2 = u.end_ms;
                    last.3.push(' ');
                    last.3.push_str(&u.text);
                    continue;
                }
            }
            out.push((u.speaker, u.start_ms, u.end_ms, u.text.clone()));
        }
        out
    }

    pub fn to_markdown(&self) -> String {
        let mut s = format!("# {}\n\n", self.title);
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
        s.push_str("## 녹취록\n\n");
        for (spk, start, _, text) in self.paragraphs() {
            s.push_str(&format!("**{}** `{}`  \n{}\n\n", self.speaker_name(spk), fmt_time(start), text));
        }
        s
    }

    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for (spk, start, _, text) in self.paragraphs() {
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
        let Some(doc) = self.current.as_ref().filter(|d| !d.utterances.is_empty()) else {
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
        if doc.utterances.is_empty() {
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
        Ok(serde_json::from_str(&s)?)
    }

    pub fn save_doc(&mut self, doc: SessionDoc) -> Result<()> {
        safe_id(&doc.id)?;
        if let Some(cur) = self.current.as_mut().filter(|c| c.id == doc.id) {
            // 녹음 중인 기록: 화면에서 고친 것만 반영하고, 그 사이 새로 들어온 발화는 지킨다
            cur.title = doc.title.clone();
            cur.auto_title = doc.auto_title;
            cur.speakers = doc.speakers.clone();
            cur.speaker_map = doc.speaker_map.clone();
            cur.summary = doc.summary.clone();
            // 화면이 지웠다고 알린 발화만 지우고, 그 사이 새로 들어온 발화는 지킨다
            cur.utterances.retain_mut(|x| {
                if doc.deleted.contains(&x.id) {
                    return false;
                }
                if let Some(u) = doc.utterances.iter().find(|u| u.id == x.id) {
                    x.text = u.text.clone();
                    x.speaker = u.speaker;
                }
                true
            });
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
        // 화면 사본은 발화가 도착 순서로 쌓여 있을 수 있다 → 시간 순으로 저장 (MD·TXT·SRT 순서)
        doc.utterances.sort_by(|a, b| a.start_ms.cmp(&b.start_ms).then(a.id.cmp(&b.id)));
        doc.duration_ms = doc.duration_ms.max(old.duration_ms);
        write_doc(&self.dir, &doc)
    }

    /// 발화를 모두 지운 경우에도 저장 (flush 는 빈 기록을 건너뛴다)
    fn flush_any(&mut self) -> Result<()> {
        let Some(doc) = self.current.as_ref() else {
            return Ok(());
        };
        if doc.utterances.is_empty() {
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

    pub fn delete(&mut self, id: &str) -> Result<()> {
        let id = safe_id(id)?;
        for ext in ["json", "md", "opus", "wav", "voices"] {
            let p = self.dir.join(format!("{id}.{ext}"));
            if p.exists() {
                std::fs::remove_file(p)?;
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
        let r = d.apply_refinement(&labels, &[], &BTreeSet::new(), &[]);
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
        let r = d.apply_refinement(&labels, &[], &BTreeSet::new(), &[]);
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
        let auto: BTreeSet<String> = ["박 대리".to_string()].into();
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
}
