//! 화자 분리: 발화별 화자 임베딩(3D-Speaker CAM++ 등 ONNX) + 온라인 클러스터링.
//!
//! - 발화가 끝날 때마다 임베딩을 뽑아 기존 화자 중심과 코사인 유사도를 비교한다.
//! - 충분히 비슷하면 그 화자, 아니면 새 화자로 등록한다.
//! - 한 발화 안에서 사람이 바뀌는 경우(쉼 없이 말을 받는 경우)를 위해 슬라이딩 윈도로 전환점을 찾아 쪼갠다.

use std::path::Path;

use anyhow::{anyhow, Result};
use ort::session::Session;
use ort::value::Tensor;
use serde::{Deserialize, Serialize};

use crate::fbank::{Fbank, NUM_BINS};
use crate::vad::{SpeechSegment, CHUNK};
use crate::SAMPLE_RATE;

pub struct SpeakerEncoder {
    session: Session,
    input: String,
    fbank: Fbank,
}

impl SpeakerEncoder {
    pub fn new(model: &Path) -> Result<Self> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(4);
        let session = crate::onnx::session_from_file(model, threads)?;
        let input = session
            .inputs()
            .first()
            .map(|o| o.name().to_string())
            .ok_or_else(|| anyhow!("화자 모델 입력이 없음"))?;
        Ok(Self {
            session,
            input,
            fbank: Fbank::default(),
        })
    }

    /// L2 정규화된 화자 임베딩. 너무 짧으면 None.
    pub fn embed(&mut self, samples: &[f32]) -> Result<Option<Vec<f32>>> {
        let (feats, frames) = self.fbank.compute(samples);
        if frames < 30 {
            return Ok(None);
        }
        let t = Tensor::from_array(([1usize, frames, NUM_BINS], feats))?;
        let out = self.session.run(ort::inputs![self.input.as_str() => t])?;
        let (_, e) = out[0].try_extract_tensor::<f32>()?;
        let mut v = e.to_vec();
        normalize(&mut v);
        Ok(Some(v))
    }
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-8 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DiarizationConfig {
    pub enabled: bool,
    /// 같은 화자로 볼 코사인 유사도 기준 (높을수록 화자를 잘게 나눔)
    pub threshold: f32,
    /// 최대 화자 수 (0 = 자동)
    pub max_speakers: u32,
    /// 한 발화 안의 화자 전환 감지
    pub split_turns: bool,
}

impl Default for DiarizationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.45,
            max_speakers: 0,
            split_turns: true,
        }
    }
}

struct Speaker {
    sum: Vec<f32>,
    centroid: Vec<f32>,
    weight: f32,
    /// 짧은 발화로만 만들어진 임시 화자 (목소리 특징이 불안정)
    provisional: bool,
}

pub struct SpeakerTracker {
    cfg: DiarizationConfig,
    speakers: Vec<Speaker>,
    last: Option<u32>,
}

/// 이 길이 미만의 발화는 임베딩이 불안정하므로 새 화자를 만들지 않는다
const RELIABLE_MS: u64 = 1200;

impl SpeakerTracker {
    pub fn new(cfg: DiarizationConfig) -> Self {
        Self {
            cfg,
            speakers: Vec::new(),
            last: None,
        }
    }

    pub fn count(&self) -> usize {
        self.speakers.len()
    }

    fn best(&self, emb: &[f32]) -> Option<(u32, f32)> {
        self.speakers
            .iter()
            .enumerate()
            .map(|(i, s)| (i as u32, cosine(&s.centroid, emb)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// 임베딩을 화자에 배정하고 화자 번호(0부터)를 돌려준다
    pub fn assign(&mut self, emb: Option<&[f32]>, dur_ms: u64) -> u32 {
        let Some(emb) = emb else {
            // 임베딩을 못 뽑을 만큼 짧으면 직전 화자로 본다
            return self.last.unwrap_or_else(|| self.create(None, true));
        };
        let reliable = dur_ms >= RELIABLE_MS;
        let thr = self.cfg.threshold;
        let full = self.cfg.max_speakers > 0 && self.speakers.len() as u32 >= self.cfg.max_speakers;

        let id = match self.best(emb) {
            Some((id, sim)) if sim >= thr => {
                self.update(id, emb, dur_ms, sim);
                id
            }
            Some((id, sim)) if full => {
                if reliable {
                    self.update(id, emb, dur_ms, sim);
                }
                id
            }
            Some((id, sim)) if !reliable => {
                // 짧은 발화: 새 화자를 만들기보다 가까운 쪽/직전 화자로
                if sim >= thr * 0.6 {
                    id
                } else {
                    self.last.unwrap_or(id)
                }
            }
            None if !reliable => self.create(Some(emb), true),
            _ => {
                // 긴 발화인데 맞는 화자가 없을 때: 짧은 발화로 만든 임시 화자가 어느 정도 닮았으면
                // 새 화자를 만드는 대신 그 화자를 이 목소리로 확정한다 (회의 첫마디 "네" 같은 경우)
                match self.closest_provisional(emb, 0.3) {
                    Some(id) => {
                        let s = &mut self.speakers[id as usize];
                        s.sum = emb.to_vec();
                        s.centroid = emb.to_vec();
                        s.weight = 1.0;
                        s.provisional = false;
                        id
                    }
                    None => self.create(Some(emb), false),
                }
            }
        };
        self.last = Some(id);
        id
    }

    fn closest_provisional(&self, emb: &[f32], min_sim: f32) -> Option<u32> {
        self.speakers
            .iter()
            .enumerate()
            .filter(|(_, s)| s.provisional)
            .map(|(i, s)| (i as u32, if s.centroid.len() == emb.len() { cosine(&s.centroid, emb) } else { 1.0 }))
            .filter(|&(_, sim)| sim >= min_sim)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    fn create(&mut self, emb: Option<&[f32]>, provisional: bool) -> u32 {
        let dim = emb.map(|e| e.len()).unwrap_or(0);
        let v = emb.map(|e| e.to_vec()).unwrap_or_else(|| vec![0.0; dim]);
        self.speakers.push(Speaker {
            sum: v.clone(),
            centroid: v,
            weight: 1.0,
            provisional,
        });
        let id = (self.speakers.len() - 1) as u32;
        self.last = Some(id);
        id
    }

    fn update(&mut self, id: u32, emb: &[f32], dur_ms: u64, sim: f32) {
        // 길고 확실한 발화일수록 중심을 많이 움직인다
        let w = (dur_ms as f32 / 1000.0).clamp(0.3, 6.0) * sim.max(0.1);
        let s = &mut self.speakers[id as usize];
        if s.sum.len() != emb.len() {
            s.sum = vec![0.0; emb.len()];
            s.weight = 0.0;
        }
        for (a, b) in s.sum.iter_mut().zip(emb) {
            *a += b * w;
        }
        s.weight += w;
        s.centroid = s.sum.clone();
        normalize(&mut s.centroid);
        if dur_ms >= RELIABLE_MS {
            s.provisional = false;
        }
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

    #[test]
    fn short_first_utterance_does_not_create_phantom_speaker() {
        let mut t = SpeakerTracker::new(DiarizationConfig::default());
        let a_short = unit(&[1.0, 0.5, 0.0]);
        let a_long = unit(&[1.0, 0.1, 0.1]);
        let b_long = unit(&[0.0, 0.2, 1.0]);
        assert_eq!(t.assign(Some(&a_short), 600), 0); // "네" — 임시 화자
        assert_eq!(t.assign(Some(&a_long), 4000), 0); // 같은 사람의 긴 발화가 임시 화자를 확정
        assert_eq!(t.assign(Some(&b_long), 4000), 1); // 다른 사람
        assert_eq!(t.assign(Some(&a_long), 3000), 0);
        assert_eq!(t.count(), 2);
    }
}

/// 한 발화 안에서 화자가 바뀌는 지점(샘플 오프셋)을 찾는다.
pub fn find_turns(enc: &mut SpeakerEncoder, seg: &SpeechSegment, threshold: f32) -> Result<Vec<usize>> {
    const WIN: usize = SAMPLE_RATE as usize * 3 / 2; // 1.5 s
    const HOP: usize = SAMPLE_RATE as usize * 3 / 4; // 0.75 s
    const MIN_PART: usize = SAMPLE_RATE as usize; // 1 s
    let n = seg.samples.len();
    if n < WIN * 2 {
        return Ok(Vec::new());
    }
    let mut embs = Vec::new();
    let mut start = 0;
    while start + WIN <= n {
        match enc.embed(&seg.samples[start..start + WIN])? {
            Some(e) => embs.push(e),
            None => break,
        }
        start += HOP;
    }
    if embs.len() < 3 {
        return Ok(Vec::new());
    }

    // 이진 분할: 경계 왼쪽·오른쪽 창들 사이의 평균 유사도(교차)가 각 쪽 내부 유사도보다
    // 뚜렷이 낮으면 화자가 바뀐 것으로 본다. 순차 클러스터링은 경계에 걸친 창 하나로
    // 두 화자가 이어 붙어 버리므로 쓰지 않는다.
    let n_win = embs.len();
    let mut sim = vec![vec![0.0f32; n_win]; n_win];
    for i in 0..n_win {
        for j in 0..n_win {
            sim[i][j] = cosine(&embs[i], &embs[j]);
        }
    }
    let mut boundaries = Vec::new();
    split_windows(&sim, 0, n_win, threshold, &mut boundaries, 0);
    boundaries.sort_unstable();

    let mut cuts = Vec::new();
    let mut prev_cut = 0usize;
    for k in boundaries {
        // 창 k-1 과 k 사이: 두 창 중심의 가운데
        let center = (k - 1) * HOP + WIN / 2 + HOP / 2;
        let cut = refine_cut(seg, center);
        if cut >= prev_cut + MIN_PART && cut + MIN_PART <= n {
            cuts.push(cut);
            prev_cut = cut;
        }
    }
    Ok(cuts)
}

fn mean(v: &[f32]) -> f32 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f32>() / v.len() as f32
    }
}

/// 창 구간 [lo, hi) 에서 가장 뚜렷한 화자 경계를 찾아 재귀적으로 나눈다
fn split_windows(sim: &[Vec<f32>], lo: usize, hi: usize, threshold: f32, out: &mut Vec<usize>, depth: usize) {
    if hi - lo < 3 || depth > 4 {
        return;
    }
    let mut best: Option<(usize, f32, f32)> = None; // (k, cross, score)
    for k in lo + 1..hi {
        let (l, r) = (k - lo, hi - k);
        // 한쪽이 창 1개뿐이면 경계가 구간 끝에 있을 때만 허용 (짧은 끝자락 화자)
        if l < 1 || r < 1 || (l < 2 && r < 2) {
            continue;
        }
        let mut cross = Vec::new();
        for i in lo..k {
            for j in k..hi {
                // 50% 겹치는 이웃 창끼리는 유사도가 부풀려지므로 뺀다
                if j - i > 1 {
                    cross.push(sim[i][j]);
                }
            }
        }
        if cross.is_empty() {
            continue;
        }
        let mut within = Vec::new();
        for (a, b) in [(lo, k), (k, hi)] {
            for i in a..b {
                for j in i + 1..b {
                    within.push(sim[i][j]);
                }
            }
        }
        let c = mean(&cross);
        let w = if within.is_empty() { 0.7 } else { mean(&within) };
        let score = w - c;
        if best.is_none_or(|(_, _, s)| score > s) {
            best = Some((k, c, score));
        }
    }
    if let Some((k, cross, score)) = best {
        if cross < threshold - 0.03 && score > 0.15 {
            out.push(k);
            split_windows(sim, lo, k, threshold, out, depth + 1);
            split_windows(sim, k, hi, threshold, out, depth + 1);
        }
    }
}

/// ±0.5 s 안에서 음성 확률이 가장 낮은 청크 경계로 옮긴다
fn refine_cut(seg: &SpeechSegment, at: usize) -> usize {
    let c = at / CHUNK;
    let r = (SAMPLE_RATE as usize / 2) / CHUNK;
    let lo = c.saturating_sub(r);
    let hi = (c + r).min(seg.probs.len().saturating_sub(1));
    let mut best = c.min(seg.probs.len().saturating_sub(1));
    let mut best_p = f32::MAX;
    for i in lo..=hi {
        if let Some(&p) = seg.probs.get(i) {
            if p < best_p {
                best_p = p;
                best = i;
            }
        }
    }
    (best * CHUNK).min(seg.samples.len())
}

/// 전환점에서 발화를 쪼갠다
pub fn split_segment(seg: SpeechSegment, cuts: &[usize]) -> Vec<SpeechSegment> {
    if cuts.is_empty() {
        return vec![seg];
    }
    let mut parts = Vec::with_capacity(cuts.len() + 1);
    let mut prev = 0usize;
    let bounds: Vec<usize> = cuts.iter().copied().chain(std::iter::once(seg.samples.len())).collect();
    for b in bounds {
        let pc = prev / CHUNK;
        let bc = b.div_ceil(CHUNK).min(seg.probs.len());
        parts.push(SpeechSegment {
            start: seg.start + prev as u64,
            end: seg.start + b as u64,
            samples: seg.samples[prev..b].to_vec(),
            probs: seg.probs.get(pc..bc).map(|s| s.to_vec()).unwrap_or_default(),
        });
        prev = b;
    }
    parts
}
