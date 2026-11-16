//! 한 녹음의 화자 구분: 실시간 배정 · 목소리로 이름 알아보기 · 회의가 끝난 뒤 전체를 보고 다시 정리.
//!
//! - 실시간: 발화마다 화자 전환점을 찾아 쪼개고([`speaker::find_turns`]) 온라인 클러스터링으로 화자를 정한다.
//!   앞부분은 화자 중심이 덜 자란 채로 정해지므로 틀리기 쉽고, 같은 사람이 둘로 나뉘기도 한다.
//! - 회의가 끝나면([`Diarizer::finish`]) 모든 조각의 목소리를 한꺼번에 보고 다시 정리한다:
//!   같은 사람으로 보이는 화자를 합치고, 조각마다 다른 화자에 더 가까우면 옮긴다.
//!   잘못 합치는 것(사람이 사라지고 붙인 이름도 사라짐)이 덜 합치는 것(사람이 한 번 눌러 합치면 됨)보다 훨씬 나쁘므로
//!   보수적으로: 확실히 같은 사람일 때만 합치고, 사람이 이름을 붙인 화자는 지킨다([`SpeakerHints`]).
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

/// 문장(조각) 하나의 목소리 — 사람이 문장을 다른 화자로 옮겨도 목소리가 그 문장을 따라가게 문장별로 넘긴다
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UtteranceVoice {
    /// 발화 id
    pub id: u64,
    /// L2 정규화된 임베딩
    pub embedding: Vec<f32>,
    pub dur_ms: u64,
}

/// 회의 후 다시 정리에 넘기는 사람의 결정 — 다시 정리가 사람이 정한 것을 되돌리지 않게
#[derive(Debug, Clone, Default)]
pub struct SpeakerHints {
    /// 녹음 중에 합친 화자 (실시간 화자 → 합쳐진 화자). 처음부터 한 화자로 묶는다.
    pub merges: BTreeMap<u32, u32>,
    /// 사람이 이름을 붙인 화자 (합친 뒤 화자 번호 → 이름). 이름이 다른 화자끼리는 합치지 않고,
    /// 이름 붙은 화자가 다른 화자에 흡수되어 없어지지 않게 한다.
    /// 사람이 합쳤거나 이름을 붙인 화자에서는 문장을 다른 화자로 빼 가지 않는다 (들어오는 것만 받는다).
    pub names: BTreeMap<u32, String>,
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
    /// 다시 정리한 화자별 목소리
    pub voices: Vec<SpeakerVoice>,
    /// 믿을 만한 조각의 목소리 (문장별)
    pub utterances: Vec<UtteranceVoice>,
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

/// 다시 정리: 두 화자의 문장끼리 평균 닮음이 각 화자 안의 평균 닮음에 이 비율 이상 가까워야 같은 사람으로 합친다.
///
/// 화자 중심끼리의 닮음은 녹음 환경을 많이 탄다 — 노트북 마이크 하나로 한 방에서 녹음한 실제 회의(2026-11-16, 67분)에서는
/// 서로 다른 사람의 중심끼리도 0.55~0.77 닮아, "중심이 0.65 넘게 닮으면 합치기" 로는 다섯 명이 차례로 합쳐졌다.
/// 문장 단위로는 같은 화자 안 0.57~0.59 · 다른 화자끼리 0.32~0.41 로 뚜렷이 갈리므로, 그 비율로 본다
/// (같은 사람이 둘로 나뉜 것이면 1 가까이, 그 회의의 다른 사람끼리는 0.55~0.79).
const SAME_RATIO: f32 = 0.88;
/// 다시 정리: 조각이 다른 화자에 이만큼 더 가까울 때만 옮긴다 (왔다 갔다 하지 않게)
const MOVE_MARGIN: f32 = 0.05;
/// 다시 정리: 믿을 만한 말이 이만큼(초) 쌓인 화자로만 조각을 옮긴다 — 한두 마디뿐인 화자는 중심이 그 한마디라,
/// 옮기기를 되풀이하면 그 화자가 비슷한 문장을 끌어모아 수백 문장짜리 화자로 자란다 (위 회의에서 1.4초 → 271문장)
const SOLID_SECS: f32 = 8.0;
/// 다시 정리: 옮기기를 되풀이하는 최대 횟수
const MOVE_ROUNDS: usize = 4;
/// 다시 정리: 옮기기로 한 화자의 믿을 만한 말이 이 비율 아래로 줄지는 않게 한다 (화자가 통째로 사라지지 않게)
const KEEP_SHARE: f32 = 0.5;
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

    /// 회의 전체를 보고 화자를 다시 정리한다. `hints`: 사람이 녹음 중에 합친 화자 · 이름 붙인 화자
    pub fn finish(&self, hints: &SpeakerHints) -> Refinement {
        refine(&self.samples, &self.cfg, hints)
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
/// (실시간에는 1.2초 남짓한 "네, 알겠습니다" 한마디가 따로 새 화자가 되기도 한다). 사람이 이름을 붙인 화자는 그대로 둔다.
fn weak_merge(cents: &BTreeMap<u32, Centroid>, min_sim: f32, names: &BTreeMap<u32, String>) -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32, f32)> = None;
    for (&w, cw) in cents.iter().filter(|(l, c)| c.secs < WEAK_SECS && !names.contains_key(l)) {
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

/// 처음 화자(사람이 합친 것까지 반영한 실시간 화자) 하나의 믿을 만한 문장 목소리 합.
/// 단위 벡터라 문장끼리 닮음의 합을 바로 구할 수 있다: 화자 안 Σᵢ≠ⱼ xᵢ·xⱼ = |Σx|² − n, 두 화자 사이 Σx·Σy.
struct Pool {
    sum: Vec<f32>,
    n: usize,
    secs: f32,
}

impl Pool {
    /// 화자 안 문장끼리 평균 닮음 (문장이 하나뿐이면 알 수 없음)
    fn within(&self) -> Option<f32> {
        (self.n >= 2).then(|| {
            let n = self.n as f32;
            (cosine(&self.sum, &self.sum) - n) / (n * (n - 1.0))
        })
    }
}

fn pools(samples: &[VoiceSample], origin: &[u32]) -> BTreeMap<u32, Pool> {
    let mut out: BTreeMap<u32, Pool> = BTreeMap::new();
    for (s, &o) in samples.iter().zip(origin) {
        let Some(e) = s.reliable() else {
            continue;
        };
        let p = out.entry(o).or_insert_with(|| Pool {
            sum: vec![0.0; e.len()],
            n: 0,
            secs: 0.0,
        });
        if p.sum.len() != e.len() {
            continue;
        }
        p.sum.iter_mut().zip(e).for_each(|(a, b)| *a += b);
        p.n += 1;
        p.secs += s.dur_ms as f32 / 1000.0;
    }
    out
}

/// 이 회의에서 한 사람의 문장끼리 보통 얼마나 닮나 (녹음 환경마다 다르다) — 문장이 많은 화자일수록 무겁게
fn typical_within(pools: &BTreeMap<u32, Pool>) -> f32 {
    for min_n in [3, 2] {
        let (mut sum, mut n) = (0.0f32, 0.0f32);
        for p in pools.values().filter(|p| p.n >= min_n) {
            if let Some(w) = p.within() {
                sum += w * p.n as f32;
                n += p.n as f32;
            }
        }
        if n > 0.0 {
            return (sum / n).clamp(0.1, 1.0);
        }
    }
    0.6
}

/// 두 처음 화자가 같은 사람인 정도: 서로의 문장끼리 평균 닮음 ÷ 각 화자 안의 평균 닮음 (같은 사람이면 1 가까이).
/// 문장이 하나뿐이거나 유난히 흩어진 화자는 그 회의의 보통 값에 기대어 본다 (아무와도 닮아 보이지 않게).
fn same_ratio(a: &Pool, b: &Pool, typical: f32) -> f32 {
    if a.sum.len() != b.sum.len() || a.n == 0 || b.n == 0 {
        return -1.0;
    }
    let cross = cosine(&a.sum, &b.sum) / (a.n * b.n) as f32;
    let floor = typical * 0.75;
    let wa = a.within().unwrap_or(typical).max(floor);
    let wb = b.within().unwrap_or(typical).max(floor);
    cross / (wa * wb).sqrt()
}

/// 둘을 합칠 때 남길 쪽: 이름 붙은 화자, 아니면 말이 많은 쪽
fn keep_gone(a: u32, b: u32, cents: &BTreeMap<u32, Centroid>, names: &BTreeMap<u32, String>) -> (u32, u32) {
    match (names.contains_key(&a), names.contains_key(&b)) {
        (true, false) => (a, b),
        (false, true) => (b, a),
        _ => {
            let w = |l: u32| cents.get(&l).map_or(0.0, |c| c.weight);
            if w(a) >= w(b) {
                (a, b)
            } else {
                (b, a)
            }
        }
    }
}

/// 회의 전체의 조각을 보고 화자를 다시 정한다. 잘못 합치면 사람이 사라지므로 확실할 때만 바꾼다.
///
/// 0. 사람이 녹음 중에 합친 화자(`hints.merges`)는 처음부터 한 화자로 묶는다 — 다시 정리가 그 결정을 되돌리지 않게
/// 1. 확실히 같은 사람인 화자만 합친다: 문장끼리 평균 닮음이 화자 안의 닮음에 [`SAME_RATIO`] 이상 가깝고,
///    합칠 화자를 이루는 처음 화자끼리 모두 그래야 한다 (합칠수록 중심이 "평균 목소리"가 되어 줄줄이 합쳐지지 않게).
///    이름이 다른 화자끼리는 합치지 않는다. 최대 화자 수를 넘으면 가장 닮은 쌍부터 합친다.
///    말이 [`WEAK_SECS`] 도 안 되는 화자는 웬만큼 닮은 큰 화자에 붙인다.
/// 2. 믿을 만한 조각마다, 자기를 뺀 원래 화자 중심보다 다른 화자 중심에 뚜렷이 가까우면 옮긴다 — 말이 충분히 쌓인
///    화자로만, 한 화자의 말이 절반 아래로 줄지 않게, 몇 번만 되풀이한다. 사람이 합쳤거나 이름 붙인 화자에서는 빼 가지 않는다.
/// 3. 짧은 말로만 이뤄진 화자는 가까운 화자에 붙인다
pub fn refine(samples: &[VoiceSample], cfg: &DiarizationConfig, hints: &SpeakerHints) -> Refinement {
    let merged = |mut l: u32| {
        for _ in 0..16 {
            match hints.merges.get(&l) {
                Some(&t) if t != l => l = t,
                _ => break,
            }
        }
        l
    };
    let origin: Vec<u32> = samples.iter().map(|s| merged(s.online)).collect();
    let mut labels = origin.clone();
    let mut names: BTreeMap<u32, String> = BTreeMap::new();
    for (&l, n) in &hints.names {
        if !n.trim().is_empty() {
            names.entry(merged(l)).or_insert_with(|| n.trim().to_string());
        }
    }
    // 사람이 손댄 화자 (합쳤거나 이름 붙임): 문장을 빼 가지 않는다
    let mut pinned: BTreeSet<u32> = hints.merges.keys().map(|&l| merged(l)).collect();
    pinned.extend(names.keys().copied());
    let ratio_thr = env_f32("BADASUGI_SAME_RATIO", SAME_RATIO);
    let move_margin = env_f32("BADASUGI_MOVE_MARGIN", MOVE_MARGIN);
    let max = cfg.max_speakers as usize;

    // 1. 합치기
    let pools = pools(samples, &origin);
    let typical = typical_within(&pools);
    // 지금 화자 → 그 화자를 이루는 처음 화자들
    let mut members: BTreeMap<u32, Vec<u32>> = pools.keys().map(|&o| (o, vec![o])).collect();
    loop {
        let cents = centroids(samples, &labels);
        let over = max > 0 && members.len() > max;
        let keys: Vec<u32> = members.keys().copied().collect();
        let solid = |g: u32| members[&g].iter().filter(|o| pools[*o].secs >= WEAK_SECS).copied().collect::<Vec<u32>>();
        let mut best: Option<(u32, u32, f32)> = None;
        for (i, &a) in keys.iter().enumerate() {
            let sa = solid(a);
            for &b in &keys[i + 1..] {
                if matches!((names.get(&a), names.get(&b)), (Some(x), Some(y)) if x != y) {
                    continue;
                }
                let sb = solid(b);
                // 말이 적은 화자끼리는 같은 사람인지 따지지 않는다 (아래 약한 화자 붙이기 · 최대 화자 수에서)
                let (xs, ys) = if sa.is_empty() || sb.is_empty() {
                    if !over {
                        continue;
                    }
                    (members[&a].clone(), members[&b].clone())
                } else {
                    (sa.clone(), sb)
                };
                let link = xs
                    .iter()
                    .flat_map(|x| ys.iter().map(move |y| (x, y)))
                    .map(|(x, y)| same_ratio(&pools[x], &pools[y], typical))
                    .fold(f32::INFINITY, f32::min);
                if best.is_none_or(|(_, _, s)| link > s) {
                    best = Some((a, b, link));
                }
            }
        }
        let pair = match best {
            Some((a, b, link)) if link >= ratio_thr || over => {
                log::debug!("화자 다시 정리: {a} · {b} 같은 사람으로 봄 ({link:.2})");
                Some(keep_gone(a, b, &cents, &names))
            }
            _ => weak_merge(&cents, cfg.threshold * WEAK_SIM, &names),
        };
        let Some((keep, gone)) = pair else {
            break;
        };
        log::debug!("화자 다시 정리: {gone} → {keep} 합침");
        labels.iter_mut().filter(|l| **l == gone).for_each(|l| *l = keep);
        if let Some(m) = members.remove(&gone) {
            members.entry(keep).or_default().extend(m);
        }
        if let Some(n) = names.remove(&gone) {
            names.entry(keep).or_insert(n);
        }
        if pinned.remove(&gone) {
            pinned.insert(keep);
        }
    }

    // 2. 옮기기 (한 번에 계산한 중심 기준으로 모두 판단한 뒤 반영)
    let start_secs: BTreeMap<u32, f32> = centroids(samples, &labels).iter().map(|(&l, c)| (l, c.secs)).collect();
    let mut frozen: BTreeSet<u32> = pinned.clone();
    for _ in 0..MOVE_ROUNDS {
        let cents = centroids(samples, &labels);
        let solid: BTreeSet<u32> = cents.iter().filter(|(_, c)| c.secs >= SOLID_SECS).map(|(&l, _)| l).collect();
        if solid.is_empty() {
            break;
        }
        // (조각, 옮길 화자, 얼마나 더 가까운지)
        let mut moves: Vec<(usize, u32, f32)> = Vec::new();
        for (i, s) in samples.iter().enumerate() {
            let cur = labels[i];
            if frozen.contains(&cur) {
                continue;
            }
            let best_other = |e: &[f32]| {
                cents
                    .iter()
                    .filter(|(&l, c)| l != cur && solid.contains(&l) && c.unit.len() == e.len())
                    .map(|(&l, c)| (l, cosine(&c.unit, e)))
                    .max_by(|a, b| a.1.total_cmp(&b.1))
            };
            let Some(e) = s.reliable() else {
                // 짧은 말("네" · "Thanks"): 실시간에는 대개 직전 화자로 붙인다. 목소리가 다른 화자와 확실히 닮았을 때만 옮긴다.
                if let Some(e) = s.emb.as_deref().filter(|e| !e.is_empty() && s.dur_ms >= SHORT_MIN_MS) {
                    let own = cents.get(&cur).filter(|c| c.unit.len() == e.len()).map_or(-1.0, |c| cosine(&c.unit, e));
                    if let Some((to, sim)) = best_other(e) {
                        if sim >= cfg.threshold && sim >= own + SHORT_MOVE_MARGIN {
                            moves.push((i, to, sim - own));
                        }
                    }
                }
                continue;
            };
            // 자기 자신을 뺀 원래 화자 중심과 비교 (혼자뿐인 화자면 비교할 중심이 없음)
            let own = cents.get(&cur).and_then(|c| {
                let w = weight(s.dur_ms);
                let mut v: Vec<f32> = c.sum.iter().zip(e).map(|(a, b)| a - b * w).collect();
                (c.weight - w > 1e-3).then(|| {
                    normalize(&mut v);
                    cosine(&v, e)
                })
            });
            let Some((to, sim)) = best_other(e) else {
                continue;
            };
            match own {
                Some(o) if sim > o + move_margin => moves.push((i, to, sim - o)),
                // 혼자뿐인 화자: 다른 화자로 볼 만큼 닮았을 때만
                None if sim >= cfg.threshold => moves.push((i, to, sim - cfg.threshold)),
                _ => {}
            }
        }
        // 한 화자의 믿을 만한 말이 처음의 절반 아래로 줄지 않게 — 더 확실한 것부터 옮기고, 한도에 닿은 화자는 그만 덜어 낸다
        moves.sort_by(|a, b| b.2.total_cmp(&a.2));
        let mut left: BTreeMap<u32, f32> = cents.iter().map(|(&l, c)| (l, c.secs)).collect();
        let mut applied = 0;
        for &(i, to, _) in &moves {
            let from = labels[i];
            if frozen.contains(&from) {
                continue;
            }
            if samples[i].reliable().is_some() {
                let secs = samples[i].dur_ms as f32 / 1000.0;
                let now = left.get(&from).copied().unwrap_or(0.0);
                let floor = start_secs.get(&from).copied().unwrap_or(now) * KEEP_SHARE;
                if now - secs < floor {
                    frozen.insert(from);
                    continue;
                }
                left.insert(from, now - secs);
                *left.entry(to).or_default() += secs;
            }
            labels[i] = to;
            applied += 1;
        }
        if applied == 0 {
            break;
        }
    }

    // 3. 짧은 말로만 이뤄진 화자 (목소리 중심이 없음): 그 말들의 평균 목소리가 어느 정도 닮은 화자에 붙인다
    let cents = centroids(samples, &labels);
    let orphans: BTreeSet<u32> = labels.iter().copied().filter(|l| !cents.contains_key(l)).collect();
    for o in orphans {
        let over = max > 0 && distinct(&labels) > max;
        if names.contains_key(&o) && !over {
            continue;
        }
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
        let best = cents
            .iter()
            .filter(|(&l, c)| c.unit.len() == mean.len() && !matches!((names.get(&o), names.get(&l)), (Some(x), Some(y)) if x != y))
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
        utterances: samples
            .iter()
            .filter_map(|s| {
                Some(UtteranceVoice {
                    id: s.id,
                    embedding: s.reliable()?.to_vec(),
                    dur_ms: s.dur_ms,
                })
            })
            .collect(),
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
        let r = refine(&s, &DiarizationConfig::default(), &SpeakerHints::default());
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
        let r = refine(&s, &DiarizationConfig::default(), &SpeakerHints::default());
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
        let r = refine(&s, &DiarizationConfig::default(), &SpeakerHints::default());
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
        let r = refine(&s, &cfg, &SpeakerHints::default());
        // 가장 닮은 쌍(0 · 2)이 합쳐진다
        assert_eq!(r.labels[2].2, r.labels[0].2);
        assert_eq!(r.voices.len(), 2);
    }

    #[test]
    fn keeps_speakers_merged_by_hand_together() {
        // 사람이 녹음 중에 1번을 0번에 합쳐 둠. 목소리만 보면 0번 조각들은 (말이 더 많은) 2번과 아주 닮아
        // 2번으로 합쳐지고 1번만 남아, 사람이 합친 0 · 1 이 다시 갈라질 판이다.
        let s = vec![
            sample(0, &[1.0, 0.0, 0.0], 0),
            sample(1, &[0.0, 1.0, 0.0], 1),
            sample(2, &[0.98, 0.0, 0.1], 2),
            sample(3, &[1.0, 0.02, 0.0], 0),
            sample(4, &[0.05, 1.0, 0.0], 1),
            sample(5, &[0.97, 0.05, 0.1], 2),
            sample(6, &[0.99, 0.0, 0.12], 2),
            sample(7, &[0.96, 0.03, 0.08], 2),
        ];
        let split = refine(&s, &DiarizationConfig::default(), &SpeakerHints::default());
        assert_ne!(split.labels[0].2, split.labels[1].2, "제약이 없으면 갈라지는 경우여야 시험이 의미 있다");
        let hints = SpeakerHints {
            merges: [(1, 0)].into(),
            ..Default::default()
        };
        let r = refine(&s, &DiarizationConfig::default(), &hints);
        let l: Vec<u32> = r.labels.iter().map(|x| x.2).collect();
        // 사람이 합친 0 · 1 은 어디로 가든 함께
        assert_eq!(l[0], l[1]);
        assert_eq!(l[0], l[3]);
        assert_eq!(l[0], l[4]);
        // 실시간 화자 번호는 그대로 돌려준다 (사람이 고친 문장을 알아보는 데 쓴다)
        assert_eq!(r.labels[1].1, 1);
        // 문장별 목소리
        assert_eq!(r.utterances.len(), 8);
    }

    /// 시험용 목소리: 방(마이크)의 공통 성분 + 사람마다의 성분 + 문장마다 흔들림
    struct Room {
        seed: u64,
    }

    impl Room {
        const DIM: usize = 24;

        fn noise(&mut self) -> f32 {
            self.seed = self.seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5
        }

        /// `person` 의 문장 하나: 공통 0.45 · 사람 0.15 · 흔들림 0.4 (제곱 비율) — 문장끼리 같은 사람 0.6 · 다른 사람 0.45,
        /// 화자 중심끼리는 다른 사람도 0.75 가까이 닮는다 (한 방에서 마이크 하나로 녹음한 실제 회의와 비슷하게)
        fn voice(&mut self, person: usize) -> Vec<f32> {
            let mut v = vec![0.0f32; Self::DIM];
            v[0] = 0.45f32.sqrt();
            v[1 + person] = 0.15f32.sqrt();
            let mut n: Vec<f32> = (0..Self::DIM - 6).map(|_| self.noise()).collect();
            normalize(&mut n);
            for (i, x) in n.iter().enumerate() {
                v[6 + i] = x * 0.4f32.sqrt();
            }
            normalize(&mut v);
            v
        }
    }

    fn voice_sample(id: u64, emb: Vec<f32>, online: u32) -> VoiceSample {
        VoiceSample {
            id,
            emb: Some(emb),
            dur_ms: 3000,
            online,
        }
    }

    fn mean_unit(s: &[VoiceSample], online: u32) -> Vec<f32> {
        let mut c = vec![0.0f32; Room::DIM];
        for x in s.iter().filter(|x| x.online == online) {
            c.iter_mut().zip(x.emb.as_ref().unwrap()).for_each(|(a, b)| *a += b);
        }
        normalize(&mut c);
        c
    }

    #[test]
    fn distinct_people_in_one_room_are_not_merged_one_after_another() {
        // 네 사람이 한 방에서 마이크 하나로: 화자 중심끼리는 서로 닮아 보이지만(0.65 넘게) 다른 사람이다
        let mut room = Room { seed: 7 };
        let s: Vec<VoiceSample> = (0..48).map(|i| voice_sample(i, room.voice(i as usize % 4), i as u32 % 4)).collect();
        let c0 = mean_unit(&s, 0);
        let c1 = mean_unit(&s, 1);
        assert!(cosine(&c0, &c1) > 0.65, "중심끼리 닮음 {} — 예전 기준(0.65)이면 합쳐지던 경우여야 시험이 의미 있다", cosine(&c0, &c1));
        let r = refine(&s, &DiarizationConfig::default(), &SpeakerHints::default());
        assert_eq!(r.voices.len(), 4);
        assert_eq!(r.moved(), 0);
    }

    #[test]
    fn one_person_split_in_two_in_one_room_is_merged() {
        // 같은 사람(0)이 실시간에 0 · 2 로 나뉨 — 문장 단위로 보면 같은 사람이라 합친다
        let mut room = Room { seed: 11 };
        let s: Vec<VoiceSample> = (0..36)
            .map(|i| {
                let person = i as usize % 3;
                let online = if person == 0 && i >= 18 { 2 } else { person as u32 };
                voice_sample(i, room.voice(person.min(1)), online)
            })
            .collect();
        let r = refine(&s, &DiarizationConfig::default(), &SpeakerHints::default());
        assert_eq!(r.voices.len(), 2);
        let l: BTreeMap<u64, u32> = r.labels.iter().map(|&(id, _, l)| (id, l)).collect();
        assert_eq!(l[&0], l[&18]);
        assert_ne!(l[&0], l[&1]);
    }

    #[test]
    fn differently_named_speakers_are_never_merged() {
        // 목소리로는 같은 사람으로 보이는 0 · 1 (+ 다른 사람 2)
        let mut room = Room { seed: 3 };
        let s: Vec<VoiceSample> = (0..36)
            .map(|i| {
                let online = i as u32 % 3;
                voice_sample(i, room.voice(if online == 2 { 1 } else { 0 }), online)
            })
            .collect();
        let none = refine(&s, &DiarizationConfig::default(), &SpeakerHints::default());
        assert_eq!(none.voices.len(), 2, "이름이 없으면 합쳐지는 경우여야 시험이 의미 있다");
        // 사람이 둘에 다른 이름을 붙였으면 따로 둔다 (문장도 빼 가지 않는다)
        let named = SpeakerHints {
            names: [(0, "김 팀장".to_string()), (1, "박 대리".to_string())].into(),
            ..Default::default()
        };
        let r = refine(&s, &DiarizationConfig::default(), &named);
        assert_eq!(r.voices.len(), 3);
        assert!(r.labels.iter().filter(|x| x.1 < 2).all(|x| x.1 == x.2));
        // 하나에만 이름이 있으면 합치되, 이름 붙은 쪽 번호로 남는다
        let one = SpeakerHints {
            names: [(1, "박 대리".to_string())].into(),
            ..Default::default()
        };
        let r = refine(&s, &DiarizationConfig::default(), &one);
        assert_eq!(r.voices.len(), 2);
        assert!(r.labels.iter().filter(|x| x.1 < 2).all(|x| x.2 == 1));
    }

    #[test]
    fn a_one_line_speaker_does_not_pull_in_other_sentences() {
        // 실시간 0번 화자에 두 사람(A 25문장 · C 6문장)이 섞여 있고, C 와 닮은 한마디(1.4초)가 따로 1번 화자가 됨.
        // 예전에는 1번의 중심이 그 한마디라 C 같은 문장을 끌어오며 커졌다 — 한마디뿐인 화자로는 옮기지 않는다.
        let a = [1.0, 0.0, 0.0];
        let c = [0.0, 0.0, 1.0];
        let mut s: Vec<VoiceSample> = (0..31).map(|i| sample(i, if i % 5 == 4 { &c } else { &a }, 0)).collect();
        s.push(VoiceSample {
            id: 99,
            emb: Some(unit(&[0.05, 0.0, 1.0])),
            dur_ms: 1400,
            online: 1,
        });
        let r = refine(&s, &DiarizationConfig::default(), &SpeakerHints::default());
        assert_eq!(r.labels.iter().filter(|x| x.2 == 1).count(), 1);
    }

    #[test]
    fn moves_never_take_more_than_half_of_a_speaker() {
        // 실시간 0번(10문장) 중 6문장이 1번 사람 목소리 — 옮기더라도 0번의 말이 절반 아래로 줄지는 않는다
        let a = [1.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let mut s: Vec<VoiceSample> = (0..10).map(|i| sample(i, if i < 4 { &a } else { &[0.1, 1.0, 0.05] }, 0)).collect();
        s.extend((10..22).map(|i| sample(i, &b, 1)));
        let r = refine(&s, &DiarizationConfig::default(), &SpeakerHints::default());
        let left = r.labels.iter().filter(|x| x.2 == 0).count();
        assert_eq!(left, 5, "0번에 {left}문장이 남음");
        assert_eq!(r.moved(), 5);
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
