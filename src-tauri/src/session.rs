//! 회의 기록: 진행 중 자동 저장(JSON + Markdown), 기록 목록, 내보내기

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
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
        let mut s = s?;
        for _ in 0..16 {
            match self.speaker_map.get(&s) {
                Some(&t) if t != s => s = t,
                _ => break,
            }
        }
        Some(s)
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
}

impl SessionStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            current: None,
            dirty: false,
            last_save: Instant::now(),
        }
    }

    fn json_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub fn audio_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.wav"))
    }

    pub fn begin(&mut self, doc: SessionDoc) {
        self.current = Some(doc);
        self.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 자주 부르면 2초에 한 번만 실제로 쓴다
    pub fn autosave(&mut self) -> Result<()> {
        if self.dirty && self.last_save.elapsed() > Duration::from_secs(2) {
            let r = self.flush();
            if r.is_err() {
                // 실패해도 2초 뒤에 다시 시도한다 (매 발화마다 디스크를 두드리지 않게)
                self.last_save = Instant::now();
            }
            return r;
        }
        Ok(())
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
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionMeta> {
        let mut out: Vec<SessionMeta> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str::<SessionDoc>(&s).ok())
            .map(|d| d.meta())
            .collect();
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
        for ext in ["json", "md", "wav"] {
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

pub(crate) fn write_doc(dir: &Path, doc: &SessionDoc) -> Result<()> {
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
    std::fs::write(dir.join(format!("{}.md", doc.id)), doc.to_markdown())?;
    Ok(())
}
