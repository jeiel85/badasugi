//! 목소리 기억: 회의에서 이름을 붙인 화자의 목소리 특징을 `voices.json` 에 모아 두었다가,
//! 다음 회의에서 같은 목소리가 들리면 그 이름을 자동으로 붙인다.
//!
//! - 저장하는 것은 화자 임베딩(목소리 특징값, 숫자 192개)뿐이다. 녹음 · 녹취록은 넣지 않는다. 이 PC 밖으로 나가지 않는다.
//! - 이름마다 "어느 회의에서 온 목소리인지"를 따로 둔다 → 같은 회의를 다시 저장해도 두 번 세지 않고,
//!   이름을 바꾸거나 회의 기록을 지우면 그 회의에서 온 목소리만 빠진다.
//! - 회의마다의 화자 목소리는 기록 폴더의 `<id>.voices` 에 둔다 (회의가 끝난 뒤에 이름을 붙여도 기억할 수 있게).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use badasugi_core::diarize::{KnownVoice, SpeakerVoice};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// 이름 하나에 기억해 두는 최근 회의 수
const MAX_SOURCES: usize = 8;
/// 이보다 말이 적은(초) 화자의 목소리는 기억하지 않는다 (특징이 불안정)
pub const MIN_ENROLL_SECS: f32 = 5.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    embedding: Vec<f32>,
    secs: f32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Entry {
    name: String,
    /// 회의 기록 id → 그 회의에서의 목소리
    sources: BTreeMap<String, Source>,
}

impl Entry {
    /// 회의별 목소리를 말한 길이만큼(최대 2분) 무게를 두어 합친 중심
    fn centroid(&self) -> Option<Vec<f32>> {
        let dim = self.sources.values().next()?.embedding.len();
        let mut sum = vec![0.0f32; dim];
        for s in self.sources.values().filter(|s| s.embedding.len() == dim) {
            let w = s.secs.clamp(1.0, 120.0);
            sum.iter_mut().zip(&s.embedding).for_each(|(a, b)| *a += b * w);
        }
        let n = sum.iter().map(|x| x * x).sum::<f32>().sqrt();
        (n > 1e-6).then(|| sum.iter().map(|x| x / n).collect())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Book {
    version: u32,
    voices: Vec<Entry>,
}

/// 설정 화면에 보여 줄 기억한 목소리
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceInfo {
    pub name: String,
    /// 이 목소리를 들은 회의 수
    pub meetings: usize,
    /// 기억한 말의 길이 (초)
    pub secs: f32,
}

pub struct VoiceBook {
    path: PathBuf,
    book: Mutex<Book>,
}

/// 저장할 때 소수 넷째 자리까지 (파일 크기 절반 · 정확도 영향 없음)
fn round(v: &[f32]) -> Vec<f32> {
    v.iter().map(|x| (x * 10_000.0).round() / 10_000.0).collect()
}

/// 사용자가 붙인 이름인가 ("화자 3" 같은 기본 이름은 기억하지 않는다)
pub fn is_real_name(name: &str) -> bool {
    let n = name.trim();
    if n.is_empty() {
        return false;
    }
    let default = n.strip_prefix("화자").map(str::trim).is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()));
    !default && n != "발화"
}

impl VoiceBook {
    pub fn load(path: PathBuf) -> Self {
        let book = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            book: Mutex::new(book),
        }
    }

    fn save(&self, book: &Book) {
        let r = (|| -> anyhow::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(book)?)?;
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if let Err(e) = r {
            log::warn!("목소리 기억 저장 실패: {e}");
        }
    }

    /// 다음 회의에서 알아볼 목소리들
    pub fn known(&self) -> Vec<KnownVoice> {
        self.book
            .lock()
            .voices
            .iter()
            .filter_map(|e| {
                Some(KnownVoice {
                    name: e.name.clone(),
                    embedding: e.centroid()?,
                })
            })
            .collect()
    }

    pub fn list(&self) -> Vec<VoiceInfo> {
        let mut v: Vec<VoiceInfo> = self
            .book
            .lock()
            .voices
            .iter()
            .map(|e| VoiceInfo {
                name: e.name.clone(),
                meetings: e.sources.len(),
                secs: e.sources.values().map(|s| s.secs).sum(),
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// 이 회의에서 이름 붙은 목소리로 바꿔 넣는다 (이 회의에서 왔던 목소리는 먼저 모두 뺀다).
    /// `named`: (이름, 목소리, 말한 초)
    pub fn sync_session(&self, id: &str, named: &[(String, Vec<f32>, f32)]) {
        let mut book = self.book.lock();
        // 녹취록 글자를 고칠 때마다 불리므로, 실제로 바뀐 것이 있을 때만 파일에 쓴다
        let before = serde_json::to_string(&book.voices).unwrap_or_default();
        for e in book.voices.iter_mut() {
            e.sources.remove(id);
        }
        for (name, emb, secs) in named {
            let name = name.trim();
            if !is_real_name(name) || *secs < MIN_ENROLL_SECS || emb.is_empty() {
                continue;
            }
            let idx = match book.voices.iter().position(|e| e.name == name) {
                Some(i) => i,
                None => {
                    book.voices.push(Entry {
                        name: name.to_string(),
                        sources: BTreeMap::new(),
                    });
                    book.voices.len() - 1
                }
            };
            let e = &mut book.voices[idx];
            e.sources.insert(
                id.to_string(),
                Source {
                    embedding: round(emb),
                    secs: *secs,
                },
            );
            // 오래된 회의부터 덜어 낸다 (id 는 시각 순)
            while e.sources.len() > MAX_SOURCES {
                let oldest = e.sources.keys().next().cloned();
                if let Some(k) = oldest {
                    e.sources.remove(&k);
                }
            }
        }
        book.voices.retain(|e| !e.sources.is_empty());
        if serde_json::to_string(&book.voices).unwrap_or_default() != before {
            book.version = 1;
            self.save(&book);
        }
    }

    /// 지운 회의에서 온 목소리를 뺀다
    pub fn forget_session(&self, id: &str) {
        self.sync_session(id, &[]);
    }

    pub fn forget(&self, name: &str) {
        let mut book = self.book.lock();
        book.voices.retain(|e| e.name != name);
        self.save(&book);
    }

    pub fn forget_all(&self) {
        let mut book = self.book.lock();
        book.voices.clear();
        self.save(&book);
        let _ = std::fs::remove_file(&self.path);
    }
}

// ───────────── 회의별 목소리 (`<id>.voices`) ─────────────

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct SessionVoices {
    version: u32,
    voices: Vec<SpeakerVoice>,
}

pub fn voices_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.voices"))
}

pub fn write_session_voices(dir: &Path, id: &str, voices: &[SpeakerVoice]) -> anyhow::Result<()> {
    let data = SessionVoices {
        version: 1,
        voices: voices
            .iter()
            .map(|v| SpeakerVoice {
                speaker: v.speaker,
                embedding: round(&v.embedding),
                secs: (v.secs * 10.0).round() / 10.0,
            })
            .collect(),
    };
    std::fs::create_dir_all(dir)?;
    std::fs::write(voices_path(dir, id), serde_json::to_vec(&data)?)?;
    Ok(())
}

pub fn read_session_voices(dir: &Path, id: &str) -> Vec<SpeakerVoice> {
    std::fs::read_to_string(voices_path(dir, id))
        .ok()
        .and_then(|s| serde_json::from_str::<SessionVoices>(&s).ok())
        .map(|v| v.voices)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bd-voices-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("voices.json")
    }

    #[test]
    fn default_names_are_not_remembered() {
        assert!(!is_real_name("화자 3"));
        assert!(!is_real_name("화자3"));
        assert!(!is_real_name("  "));
        assert!(is_real_name("김 팀장"));
        assert!(is_real_name("화자 김"));
    }

    #[test]
    fn same_meeting_is_counted_once_and_can_be_renamed_or_forgotten() {
        let p = tmp("sync");
        let book = VoiceBook::load(p.clone());
        let v = vec![1.0, 0.0, 0.0];
        book.sync_session("2026-11-15_100000", &[("김 팀장".into(), v.clone(), 30.0)]);
        book.sync_session("2026-11-15_100000", &[("김 팀장".into(), v.clone(), 30.0)]);
        assert_eq!(book.list()[0].meetings, 1);
        // 다른 회의에서 또 들음
        book.sync_session("2026-11-16_100000", &[("김 팀장".into(), vec![0.9, 0.1, 0.0], 20.0)]);
        assert_eq!(book.list()[0].meetings, 2);
        // 첫 회의에서 이름을 바꿈 → 그 회의의 목소리는 새 이름으로
        book.sync_session("2026-11-15_100000", &[("박 대리".into(), v.clone(), 30.0)]);
        let l = book.list();
        assert_eq!(l.len(), 2);
        assert_eq!(l.iter().find(|x| x.name == "김 팀장").unwrap().meetings, 1);
        // 파일로 남았다가 다시 읽힌다
        let again = VoiceBook::load(p.clone());
        assert_eq!(again.known().len(), 2);
        // 회의 기록을 지우면 그 회의에서 온 목소리만 빠진다
        again.forget_session("2026-11-16_100000");
        assert_eq!(again.list().iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["박 대리"]);
        again.forget_all();
        assert!(again.known().is_empty());
    }

    #[test]
    fn short_or_default_named_voices_are_skipped() {
        let book = VoiceBook::load(tmp("skip"));
        book.sync_session(
            "a",
            &[("화자 1".into(), vec![1.0, 0.0], 60.0), ("짧은 사람".into(), vec![0.0, 1.0], 2.0)],
        );
        assert!(book.list().is_empty());
    }
}
