//! 한 녹음의 화자 구분: 실시간 배정 · 목소리로 이름 알아보기 · 회의가 끝난 뒤 전체를 보고 다시 정리.
//!
//! - 실시간: 발화마다 화자 전환점을 찾아 쪼개고([`speaker::find_turns`]) 온라인 클러스터링으로 화자를 정한다.
//!   앞부분은 화자 중심이 덜 자란 채로 정해지므로 틀리기 쉽고, 같은 사람이 둘로 나뉘기도 한다.
//! - 회의가 끝나면([`Diarizer::finish`]) 모든 조각의 목소리를 한꺼번에 보고 다시 정리한다:
//!   같은 사람으로 보이는 화자를 합치고, 조각마다 다른 화자에 더 가까우면 옮긴다.
//! - 이름을 붙여 둔 목소리([`KnownVoice`])가 있으면, 말이 충분히 쌓인 화자를 그 목소리와 비교해 이름을 알아본다.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::speaker::{self, cosine, normalize, DiarizationConfig, SpeakerEncoder, SpeakerTracker, RELIABLE_MS};
use crate::vad::SpeechSegment;

/// 이름을 아는 목소리 (지난 회의에서 사용자가 이름을 붙인 화자)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownVoice {
    pub name: String,
    /// L2 정규화된 목소리 중심
    pub embedding: Vec<f32>,
}

/// 한 화자의 목소리 (회의가 끝난 뒤 다시 정리한 결과)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerVoice {
    pub speaker: u32,
    /// L2 정규화된 목소리 중심
    pub embedding: Vec<f32>,
    /// 중심을 만든 믿을 만한 말의 길이 (초)
    pub secs: f32,
}

/// 다시 정리할 때 쓰는 조각 정보
#[derive(Debug, Clone)]
pub struct VoiceSample {
    pub id: u64,
    pub emb: Option<Vec<f32>>,
    pub dur_ms: u64,
    /// 실시간으로 정한 화자
    pub online: u32,
}

impl VoiceSample {
    fn reliable(&self) -> Option<&[f32]> {
        self.emb.as_deref().filter(|e| !e.is_empty() && self.dur_ms >= RELIABLE_MS)
    }
}

/// 회의가 끝난 뒤 다시 정리한 결과
#[derive(Debug, Clone, Default)]
pub struct Refinement {
    /// (발화 id, 실시간 화자, 다시 정리한 화자) — 모든 조각
    pub labels: Vec<(u64, u32, u32)>,
    pub voices: Vec<SpeakerVoice>,
}

impl Refinement {
    /// 화자가 바뀐 조각 수
    pub fn moved(&self) -> usize {
        self.labels.iter().filter(|(_, a, b)| a != b).count()
    }
}

/// 이 정도는 닮아야 등록된 목소리의 이름을 붙인다 (화자 중심끼리 비교: 같은 사람 0.7~0.9, 다른 사람 0.4 이하)
pub const VOICE_MATCH: f32 = 0.6;
/// 가장 닮은 이름과 두 번째 이름의 차이가 이보다 작으면 헷갈리는 것으로 보고 붙이지 않는다
const VOICE_MARGIN: f32 = 0.08;
/// 녹음 중에는 믿을 만한 말이 이만큼(초) 쌓인 화자부터 이름을 알아본다 (중심이 덜 자라 틀리지 않게)
pub const LIVE_NAME_SECS: f32 = 10.0;

/// 다시 정리: 화자 중심끼리 이만큼(구분 기준 + 여유) 닮으면 같은 사람으로 합친다
const MERGE_MARGIN: f32 = 0.2;
/// 다시 정리: 조각이 다른 화자에 이만큼 더 가까울 때만 옮긴다 (왔다 갔다 하지 않게)
const MOVE_MARGIN: f32 = 0.05;
/// 다시 정리: 짧은 말도 이 길이(ms) 이상이면 목소리를 보고 옮길 수 있다
const SHORT_MIN_MS: u64 = 600;
/// 다시 정리: 짧은 말은 목소리가 흔들리므로 다른 화자와 이만큼 더 닮아야 옮긴다
const SHORT_MOVE_MARGIN: f32 = 0.15;
/// 다시 정리: 믿을 만한 말이 이보다 적은(초) 화자는 "약한 화자"
const WEAK_SECS: f32 = 4.0;
/// 약한 화자는 구분 기준의 이 비율만큼만 닮아도 큰 화자에 합친다 (다른 사람끼리는 보통 0.3 아래)
const WEAK_SIM: f32 = 0.75;

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub struct Diarizer {
    enc: SpeakerEncoder,
    tracker: SpeakerTracker,
    cfg: DiarizationConfig,
    known: Vec<KnownVoice>,
    /// 녹음 중 목소리로 이름을 붙인 화자
    named: BTreeMap<u32, String>,
    samples: Vec<VoiceSample>,
}

impl Diarizer {
    pub fn new(enc: SpeakerEncoder, cfg: DiarizationConfig, known: Vec<KnownVoice>) -> Self {
        Self {
            enc,
            tracker: SpeakerTracker::new(cfg.clone()),
            cfg,
            known,
            named: BTreeMap::new(),
            samples: Vec::new(),
        }
    }

    /// 한 발화 안에서 화자가 바뀌면 그 지점에서 쪼갠다
    pub fn split(&mut self, seg: SpeechSegment) -> Vec<SpeechSegment> {
        if !self.cfg.split_turns {
            return vec![seg];
        }
        match speaker::find_turns(&mut self.enc, &seg, self.cfg.threshold) {
            Ok(cuts) => speaker::split_segment(seg, &cuts),
            Err(e) => {
                log::warn!("화자 전환 감지 실패: {e}");
                vec![seg]
            }
        }
    }

    /// 조각의 화자를 정한다 (회의가 끝난 뒤 다시 정리하려고 목소리도 남긴다)
    pub fn assign(&mut self, id: u64, part: &SpeechSegment) -> u32 {
        let emb = self.enc.embed(&part.samples).unwrap_or_else(|e| {
            log::warn!("화자 임베딩 실패: {e}");
            None
        });
        let dur_ms = part.duration_ms();
        let online = self.tracker.assign(emb.as_deref(), dur_ms);
        self.samples.push(VoiceSample { id, emb, dur_ms, online });
        online
    }

    /// 이 화자의 말이 충분히 쌓였으면 등록된 목소리와 비교해 이름을 알아본다 (처음 알아본 때 한 번만)
    pub fn recognize(&mut self, speaker: u32) -> Option<String> {
        if self.known.is_empty() || self.named.contains_key(&speaker) {
            return None;
        }
        let (centroid, secs) = self.tracker.voice(speaker)?;
        if secs < LIVE_NAME_SECS {
            return None;
        }
        let taken: BTreeSet<&str> = self.named.values().map(String::as_str).collect();
        let k = best_known(centroid, &self.known, &taken)?;
        let name = self.known[k].name.clone();
        self.named.insert(speaker, name.clone());
        Some(name)
    }

    /// 조각이 하나라도 있었나
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 회의 전체를 보고 화자를 다시 정리한다
    pub fn finish(&self) -> Refinement {
        refine(&self.samples, &self.cfg)
    }
}

/// 이 목소리와 가장 닮은 등록 목소리 (충분히 닮았고 두 번째와 뚜렷이 다를 때만)
fn best_known(voice: &[f32], known: &[KnownVoice], taken: &BTreeSet<&str>) -> Option<usize> {
    let mut sims: Vec<(usize, f32)> = known
        .iter()
        .enumerate()
        .filter(|(_, k)| k.embedding.len() == voice.len())
        .map(|(i, k)| (i, cosine(voice, &k.embedding)))
        .collect();
    sims.sort_by(|a, b| b.1.total_cmp(&a.1));
    let &(best, sim) = sims.first()?;
    let second = sims.get(1).map_or(-1.0, |s| s.1);
    (sim >= VOICE_MATCH && sim - second >= VOICE_MARGIN && !taken.contains(known[best].name.as_str())).then_some(best)
}

/// 여러 화자를 등록 목소리와 한꺼번에 맞춘다 → (화자, 이름). 한 이름은 가장 닮은 화자 하나에만 붙인다.
/// `min_secs` 보다 말이 적은 화자는 목소리가 불안정하므로 건너뛴다.
pub fn match_voices(voices: &[SpeakerVoice], known: &[KnownVoice], min_secs: f32) -> Vec<(u32, String)> {
    let none = BTreeSet::new();
    let mut cands: Vec<(f32, u32, usize)> = voices
        .iter()
        .filter(|v| v.secs >= min_secs)
        .filter_map(|v| {
            let k = best_known(&v.embedding, known, &none)?;
            Some((cosine(&v.embedding, &known[k].embedding), v.speaker, k))
        })
        .collect();
    cands.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut used_spk = BTreeSet::new();
    let mut used_name = BTreeSet::new();
    let mut out = Vec::new();
    for (_, spk, k) in cands {
        if used_spk.insert(spk) && used_name.insert(k) {
            out.push((spk, known[k].name.clone()));
        }
    }
    out.sort_by_key(|x| x.0);
    out
}

/// 화자별 목소리 중심 (믿을 만한 조각만, 긴 조각일수록 무겁게)
struct Centroid {
    sum: Vec<f32>,
    unit: Vec<f32>,
    weight: f32,
    secs: f32,
}

fn weight(dur_ms: u64) -> f32 {
    (dur_ms as f32 / 1000.0).clamp(0.3, 6.0)
}

fn centroids(samples: &[VoiceSample], labels: &[u32]) -> BTreeMap<u32, Centroid> {
    let mut out: BTreeMap<u32, Centroid> = BTreeMap::new();
    for (s, &l) in samples.iter().zip(labels) {
        let Some(e) = s.reliable() else {
            continue;
        };
        let w = weight(s.dur_ms);
        let c = out.entry(l).or_insert_with(|| Centroid {
            sum: vec![0.0; e.len()],
            unit: Vec::new(),
            weight: 0.0,
            secs: 0.0,
        });
        if c.sum.len() != e.len() {
            continue;
        }
        for (a, b) in c.sum.iter_mut().zip(e) {
            *a += b * w;
        }
        c.weight += w;
        c.secs += s.dur_ms as f32 / 1000.0;
    }
    for c in out.values_mut() {
        c.unit = c.sum.clone();
        normalize(&mut c.unit);
    }
    out
}

/// 말이 아주 적은 화자(짧은 말 한두 마디)는 목소리 중심이 불안정하다 — 웬만큼 닮은 큰 화자가 있으면 그쪽으로
/// (실시간에는 1.2초 남짓한 "네, 알겠습니다" 한마디가 따로 새 화자가 되기도 한다)
fn weak_merge(cents: &BTreeMap<u32, Centroid>, min_sim: f32) -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32, f32)> = None;
    for (&w, cw) in cents.iter().filter(|(_, c)| c.secs < WEAK_SECS) {
        for (&s, cs) in cents.iter().filter(|(&l, c)| l != w && c.secs >= WEAK_SECS && c.unit.len() == cw.unit.len()) {
            let sim = cosine(&cw.unit, &cs.unit);
            if sim >= min_sim && best.is_none_or(|b| sim > b.2) {
                best = Some((s, w, sim));
            }
        }
    }
    best.map(|(keep, gone, _)| (keep, gone))
}

fn distinct(labels: &[u32]) -> usize {
    labels.iter().collect::<BTreeSet<_>>().len()
}

/// 회의 전체의 조각을 보고 화자를 다시 정한다.
///
/// 1. 화자 중심끼리 아주 닮았으면 같은 사람으로 합친다 (최대 화자 수를 넘으면 가장 닮은 쌍부터 합친다)
/// 2. 믿을 만한 조각마다, 자기를 뺀 원래 화자 중심보다 다른 화자 중심에 뚜렷이 가까우면 옮긴다
/// 3. 1~2를 바뀌는 것이 없을 때까지 되풀이하고, 짧은 말로만 이뤄진 화자는 가까운 화자에 붙인다
pub fn refine(samples: &[VoiceSample], cfg: &DiarizationConfig) -> Refinement {
    let mut labels: Vec<u32> = samples.iter().map(|s| s.online).collect();
    let merge_thr = (cfg.threshold + env_f32("BADASUGI_MERGE_MARGIN", MERGE_MARGIN)).min(0.95);
    let move_margin = env_f32("BADASUGI_MOVE_MARGIN", MOVE_MARGIN);
    let max = cfg.max_speakers as usize;

    for _ in 0..12 {
        let mut changed = false;
        // 1. 합치기
        loop {
            let cents = centroids(samples, &labels);
            let over = max > 0 && distinct(&labels) > max;
            let mut best: Option<(u32, u32, f32)> = None;
            let keys: Vec<u32> = cents.keys().copied().collect();
            for (i, &a) in keys.iter().enumerate() {
                for &b in &keys[i + 1..] {
                    if cents[&a].unit.len() != cents[&b].unit.len() {
                        continue;
                    }
                    let sim = cosine(&cents[&a].unit, &cents[&b].unit);
                    if best.is_none_or(|(_, _, s)| sim > s) {
                        best = Some((a, b, sim));
                    }
                }
            }
            let Some((a, b, sim)) = best else {
                break;
            };
            let (keep, gone) = if sim >= merge_thr || over {
                // 말이 적은 쪽을 많은 쪽으로
                if cents[&a].weight >= cents[&b].weight {
                    (a, b)
                } else {
                    (b, a)
                }
            } else if let Some(pair) = weak_merge(&cents, cfg.threshold * WEAK_SIM) {
                pair
            } else {
                break;
            };
            log::debug!("화자 다시 정리: {gone} → {keep} 합침");
            labels.iter_mut().filter(|l| **l == gone).for_each(|l| *l = keep);
            changed = true;
        }

        // 2. 옮기기 (한 번에 계산한 중심 기준으로 모두 판단한 뒤 반영)
        let cents = centroids(samples, &labels);
        if cents.len() < 2 {
            if !changed {
                break;
            }
            continue;
        }
        let mut moves = Vec::new();
        for (i, s) in samples.iter().enumerate() {
            let Some(e) = s.reliable() else {
                // 짧은 말("네" · "Thanks"): 실시간에는 대개 직전 화자로 붙인다. 목소리가 다른 화자와 확실히 닮았을 때만 옮긴다.
                if let Some(e) = s.emb.as_deref().filter(|e| !e.is_empty() && s.dur_ms >= SHORT_MIN_MS) {
                    let cur = labels[i];
                    let own = cents.get(&cur).filter(|c| c.unit.len() == e.len()).map_or(-1.0, |c| cosine(&c.unit, e));
                    let best = cents
                        .iter()
                        .filter(|(&l, c)| l != cur && c.unit.len() == e.len())
                        .map(|(&l, c)| (l, cosine(&c.unit, e)))
                        .max_by(|a, b| a.1.total_cmp(&b.1));
                    if let Some((to, sim)) = best {
                        if sim >= cfg.threshold && sim >= own + SHORT_MOVE_MARGIN {
                            moves.push((i, to));
                        }
                    }
                }
                continue;
            };
            let cur = labels[i];
            // 자기 자신을 뺀 원래 화자 중심과 비교 (혼자뿐인 화자면 비교할 중심이 없음)
            let own = cents.get(&cur).and_then(|c| {
                let w = weight(s.dur_ms);
                let mut v: Vec<f32> = c.sum.iter().zip(e).map(|(a, b)| a - b * w).collect();
                (c.weight - w > 1e-3).then(|| {
                    normalize(&mut v);
                    cosine(&v, e)
                })
            });
            let Some((best, sim)) = cents
                .iter()
                .filter(|(&l, c)| l != cur && c.unit.len() == e.len())
                .map(|(&l, c)| (l, cosine(&c.unit, e)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
            else {
                continue;
            };
            let better = match own {
                Some(o) => sim > o + move_margin,
                // 혼자뿐인 화자: 다른 화자로 볼 만큼 닮았을 때만
                None => sim >= cfg.threshold,
            };
            if better {
                moves.push((i, best));
            }
        }
        for &(i, to) in &moves {
            labels[i] = to;
        }
        if !changed && moves.is_empty() {
            break;
        }
    }

    // 3. 짧은 말로만 이뤄진 화자 (목소리 중심이 없음): 그 말들의 평균 목소리가 어느 정도 닮은 화자에 붙인다
    let cents = centroids(samples, &labels);
    let orphans: BTreeSet<u32> = labels.iter().copied().filter(|l| !cents.contains_key(l)).collect();
    for o in orphans {
        let mut mean: Vec<f32> = Vec::new();
        for (s, _) in samples.iter().zip(&labels).filter(|(_, &l)| l == o) {
            if let Some(e) = s.emb.as_deref().filter(|e| !e.is_empty()) {
                if mean.is_empty() {
                    mean = vec![0.0; e.len()];
                }
                if mean.len() == e.len() {
                    mean.iter_mut().zip(e).for_each(|(a, b)| *a += b);
                }
            }
        }
        if mean.is_empty() {
            continue;
        }
        normalize(&mut mean);
        let over = max > 0 && distinct(&labels) > max;
        let best = cents
            .iter()
            .filter(|(_, c)| c.unit.len() == mean.len())
            .map(|(&l, c)| (l, cosine(&c.unit, &mean)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((to, sim)) = best {
            if over || sim >= cfg.threshold * 0.6 {
                labels.iter_mut().filter(|l| **l == o).for_each(|l| *l = to);
            }
        }
    }

    let voices = centroids(samples, &labels)
        .into_iter()
        .map(|(speaker, c)| SpeakerVoice {
            speaker,
            embedding: c.unit,
            secs: c.secs,
        })
        .collect();
    Refinement {
        labels: samples.iter().zip(&labels).map(|(s, &l)| (s.id, s.online, l)).collect(),
        voices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let mut v = v.to_vec();
        normalize(&mut v);
        v
    }

    fn sample(id: u64, v: &[f32], online: u32) -> VoiceSample {
        VoiceSample {
            id,
            emb: Some(unit(v)),
            dur_ms: 3000,
            online,
        }
    }

    #[test]
    fn merges_one_person_split_in_two() {
        // 같은 사람(0과 2)이 실시간으로는 둘로 나뉨, 1은 다른 사람
        let s = vec![
            sample(0, &[1.0, 0.05, 0.0], 0),
            sample(1, &[0.0, 0.1, 1.0], 1),
            sample(2, &[0.95, 0.12, 0.02], 2),
            sample(3, &[1.0, 0.0, 0.05], 0),
            sample(4, &[0.98, 0.1, 0.0], 2),
            sample(5, &[0.05, 0.0, 1.0], 1),
        ];
        let r = refine(&s, &DiarizationConfig::default());
        let l: Vec<u32> = r.labels.iter().map(|x| x.2).collect();
        assert_eq!(l[0], l[2]);
        assert_eq!(l[0], l[4]);
        assert_ne!(l[0], l[1]);
        assert_eq!(r.voices.len(), 2);
    }

    #[test]
    fn moves_early_mistake_to_the_right_speaker() {
        // 회의 초반, 1번 사람의 조각(2)이 중심이 덜 자란 0번에 잘못 붙음
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let s = vec![
            sample(0, &a, 0),
            sample(1, &[0.6, 0.5, 0.0], 0),
            sample(2, &[0.1, 1.0, 0.05], 0),
            sample(3, &b, 1),
            sample(4, &[0.05, 1.0, 0.0], 1),
            sample(5, &a, 0),
            sample(6, &b, 1),
        ];
        let r = refine(&s, &DiarizationConfig::default());
        assert_eq!(r.labels[2], (2, 0, 1));
        assert_eq!(r.labels[0].2, 0);
        assert_eq!(r.moved(), 1);
    }

    #[test]
    fn keeps_distinct_speakers_apart() {
        let s = vec![
            sample(0, &[1.0, 0.0, 0.0], 0),
            sample(1, &[0.0, 1.0, 0.0], 1),
            sample(2, &[0.0, 0.0, 1.0], 2),
            sample(3, &[1.0, 0.1, 0.0], 0),
        ];
        let r = refine(&s, &DiarizationConfig::default());
        assert_eq!(r.moved(), 0);
        assert_eq!(r.voices.len(), 3);
    }

    #[test]
    fn respects_max_speakers() {
        let s = vec![
            sample(0, &[1.0, 0.0, 0.0], 0),
            sample(1, &[0.0, 1.0, 0.0], 1),
            sample(2, &[0.3, 0.0, 1.0], 2),
        ];
        let cfg = DiarizationConfig {
            max_speakers: 2,
            ..Default::default()
        };
        let r = refine(&s, &cfg);
        // 가장 닮은 쌍(0 · 2)이 합쳐진다
        assert_eq!(r.labels[2].2, r.labels[0].2);
        assert_eq!(r.voices.len(), 2);
    }

    #[test]
    fn names_voices_one_to_one() {
        let known = vec![
            KnownVoice {
                name: "김 팀장".into(),
                embedding: unit(&[1.0, 0.0, 0.0]),
            },
            KnownVoice {
                name: "박 대리".into(),
                embedding: unit(&[0.0, 1.0, 0.0]),
            },
        ];
        let v = |speaker, e: &[f32], secs| SpeakerVoice {
            speaker,
            embedding: unit(e),
            secs,
        };
        let voices = vec![
            v(0, &[0.95, 0.1, 0.1], 30.0),
            // 김 팀장과도 닮았지만 0번이 더 닮았다 → 한 이름은 한 사람에게만
            v(1, &[0.9, 0.2, 0.2], 30.0),
            v(2, &[0.1, 1.0, 0.0], 30.0),
            // 아무와도 닮지 않음
            v(3, &[0.0, 0.0, 1.0], 30.0),
            // 말이 너무 적음
            v(4, &[0.0, 1.0, 0.0], 2.0),
        ];
        let m = match_voices(&voices, &known, 5.0);
        assert_eq!(m, vec![(0, "김 팀장".to_string()), (2, "박 대리".to_string())]);
    }

    #[test]
    fn ambiguous_voice_gets_no_name() {
        let known = vec![
            KnownVoice {
                name: "가".into(),
                embedding: unit(&[1.0, 0.0]),
            },
            KnownVoice {
                name: "나".into(),
                embedding: unit(&[0.95, 0.3]),
            },
        ];
        let v = SpeakerVoice {
            speaker: 0,
            embedding: unit(&[1.0, 0.15]),
            secs: 30.0,
        };
        assert!(match_voices(&[v], &known, 5.0).is_empty());
    }
}
