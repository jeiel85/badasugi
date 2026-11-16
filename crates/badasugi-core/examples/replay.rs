//! 녹음이 끝난 회의 기록으로 녹음 중의 화자 구분(실시간 배정)을 다시 재현한다 — 회의 후 다시 정리가 화자를
//! 잘못 합쳤을 때 정리 전 배정을 되살리거나, 다시 정리를 녹음 한 건으로 시험하는 데 쓴다.
//!
//! ```text
//! cargo run --release --example replay -- <speaker.onnx> <기록.json> <녹음.opus> [옵션]
//!   --voices <voices.json>   문장별 목소리 [{id, ms, e:[..]}] (기록 옆 .voices 를 풀어 둔 것) — 있으면 그 목소리를 쓴다
//!   --threshold <0.45>       화자 구분 기준
//!   --name <화자>=<이름>      녹음 중에 이름을 붙여 둔 화자 (여러 번) — 다시 정리가 이름 붙은 화자를 지키는지 볼 때
//!   --out <out.json>         문장마다 [id, 실시간 화자, 다시 정리한 화자] 저장
//! ```
//!
//! 문장의 시작 · 끝(기록의 startMs · endMs)은 녹음 중에 화자를 정한 조각 그대로이고 id 는 그 순서이므로,
//! 같은 구간의 목소리를 같은 순서로 [`SpeakerTracker`] 에 넣으면 녹음 중 배정이 다시 나온다.
//! 저장된 문장 목소리(믿을 만한 조각)는 녹음 원음에서 뽑은 것이라 그대로 쓰고, 짧은 조각만 녹음 파일에서 다시 뽑는다.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use badasugi_core::decode::AudioFile;
use badasugi_core::diarize::{refine, SpeakerHints, VoiceSample};
use badasugi_core::speaker::{cosine, DiarizationConfig, SpeakerEncoder, SpeakerTracker};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Utt {
    id: u64,
    start_ms: u64,
    end_ms: u64,
    speaker: Option<u32>,
}

#[derive(Deserialize)]
struct Doc {
    utterances: Vec<Utt>,
}

#[derive(Deserialize)]
struct Voice {
    id: u64,
    ms: u64,
    e: Vec<f32>,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let model = PathBuf::from(&a[0]);
    let doc: Doc = serde_json::from_str(&std::fs::read_to_string(&a[1])?)?;
    let audio = PathBuf::from(&a[2]);
    let mut cfg = DiarizationConfig::default();
    let (mut voices_p, mut out_p) = (None, None);
    let mut hints = SpeakerHints::default();
    let mut i = 3;
    while i < a.len() {
        match a[i].as_str() {
            "--voices" => voices_p = Some(a[i + 1].clone()),
            "--threshold" => cfg.threshold = a[i + 1].parse()?,
            "--out" => out_p = Some(a[i + 1].clone()),
            "--name" => {
                let (k, v) = a[i + 1].split_once('=').ok_or_else(|| anyhow::anyhow!("--name <화자>=<이름>"))?;
                hints.names.insert(k.parse()?, v.to_string());
            }
            o => anyhow::bail!("알 수 없는 옵션 {o}"),
        }
        i += 2;
    }
    let stored: HashMap<u64, Voice> = match &voices_p {
        Some(p) => serde_json::from_str::<Vec<Voice>>(&std::fs::read_to_string(p)?)?
            .into_iter()
            .map(|v| (v.id, v))
            .collect(),
        None => HashMap::new(),
    };

    let t = std::time::Instant::now();
    let mut f = AudioFile::open(&audio)?;
    let mut pcm = Vec::new();
    while f.read(&mut pcm)? {}
    eprintln!("녹음 {}초 디코딩 ({}ms)", pcm.len() / 16000, t.elapsed().as_millis());

    let mut enc = SpeakerEncoder::new(&model)?;
    let mut utts = doc.utterances;
    utts.sort_by_key(|u| u.id);
    let mut tracker = SpeakerTracker::new(cfg.clone());
    let mut samples = Vec::new();
    let (mut sims, mut fresh_n) = (Vec::new(), 0);
    for u in &utts {
        let lo = (u.start_ms * 16) as usize;
        let hi = ((u.end_ms * 16) as usize).min(pcm.len());
        let dur_ms = u.end_ms.saturating_sub(u.start_ms);
        let fresh = if lo < hi { enc.embed(&pcm[lo..hi])? } else { None };
        let emb = match stored.get(&u.id) {
            Some(v) => {
                if let Some(fr) = &fresh {
                    sims.push(cosine(fr, &v.e));
                }
                if v.ms != dur_ms {
                    eprintln!("문장 {}: 길이가 다름 (기록 {dur_ms}ms · 목소리 {}ms)", u.id, v.ms);
                }
                Some(v.e.clone())
            }
            None => {
                fresh_n += 1;
                fresh
            }
        };
        let online = tracker.assign(emb.as_deref(), dur_ms);
        samples.push(VoiceSample {
            id: u.id,
            emb,
            dur_ms,
            online,
        });
    }
    if !sims.is_empty() {
        sims.sort_by(|a, b| a.total_cmp(b));
        eprintln!(
            "녹음 파일에서 다시 뽑은 목소리 ↔ 저장된 목소리 닮음: 중앙 {:.3} · 하위 10% {:.3} ({}개) · 다시 뽑아 쓴 짧은 조각 {fresh_n}개",
            sims[sims.len() / 2],
            sims[sims.len() / 10],
            sims.len()
        );
    }

    let count = |f: &dyn Fn(&VoiceSample) -> u32| {
        let mut m: BTreeMap<u32, (usize, f32)> = BTreeMap::new();
        for s in &samples {
            let e = m.entry(f(s)).or_default();
            e.0 += 1;
            e.1 += s.dur_ms as f32 / 1000.0;
        }
        m
    };
    println!("실시간 화자 (문장 수, 초): {:?}", count(&|s| s.online));
    print_similarity(&samples);
    let r = refine(&samples, &cfg, &hints);
    let re: HashMap<u64, u32> = r.labels.iter().map(|&(id, _, l)| (id, l)).collect();
    println!("다시 정리 (문장 수, 초): {:?} · 옮김 {}", count(&|s| re[&s.id]), r.moved());

    // 지금 기록의 화자와 비교 (다시 정리한 결과가 같은지 — 번호는 달라도 됨)
    let mut pair: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for u in &utts {
        if let Some(s) = u.speaker {
            *pair.entry((re[&u.id], s)).or_default() += 1;
        }
    }
    println!("(다시 정리, 지금 기록) 문장 수: {pair:?}");
    let mut on_vs_doc: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for (u, s) in utts.iter().zip(&samples) {
        if let Some(d) = u.speaker {
            *on_vs_doc.entry((s.online, d)).or_default() += 1;
        }
    }
    println!("(실시간, 지금 기록) 문장 수: {on_vs_doc:?}");

    if let Some(p) = out_p {
        let rows: Vec<[u64; 3]> = r.labels.iter().map(|&(id, a, b)| [id, a as u64, b as u64]).collect();
        std::fs::write(p, serde_json::to_string(&rows)?)?;
    }
    Ok(())
}

/// 실시간 화자끼리 얼마나 닮았나: 화자 중심끼리의 닮음 / 문장 목소리끼리 평균 닮음 (대각선은 같은 화자 안)
fn print_similarity(samples: &[VoiceSample]) {
    let mut groups: BTreeMap<u32, Vec<(&[f32], u64)>> = BTreeMap::new();
    for s in samples {
        if let Some(e) = s.emb.as_deref().filter(|_| s.dur_ms >= badasugi_core::speaker::RELIABLE_MS) {
            groups.entry(s.online).or_default().push((e, s.dur_ms));
        }
    }
    groups.retain(|_, g| g.len() >= 5);
    let cent: BTreeMap<u32, Vec<f32>> = groups
        .iter()
        .map(|(&k, g)| {
            let mut c = vec![0.0f32; g[0].0.len()];
            for (e, ms) in g {
                let w = (*ms as f32 / 1000.0).clamp(0.3, 6.0);
                c.iter_mut().zip(*e).for_each(|(a, b)| *a += b * w);
            }
            let n = c.iter().map(|x| x * x).sum::<f32>().sqrt();
            c.iter_mut().for_each(|x| *x /= n);
            (k, c)
        })
        .collect();
    let keys: Vec<u32> = groups.keys().copied().collect();
    println!("화자 중심끼리 닮음:");
    for a in &keys {
        let row: Vec<String> = keys.iter().map(|b| format!("{:.2}", cosine(&cent[a], &cent[b]))).collect();
        println!("  {a:>2}: {}", row.join(" "));
    }
    println!("문장 목소리끼리 평균 닮음:");
    for a in &keys {
        let row: Vec<String> = keys
            .iter()
            .map(|b| {
                let (ga, gb) = (&groups[a], &groups[b]);
                let (mut sum, mut n) = (0.0f64, 0usize);
                for (i, x) in ga.iter().enumerate() {
                    for (j, y) in gb.iter().enumerate() {
                        if a == b && i == j {
                            continue;
                        }
                        sum += cosine(x.0, y.0) as f64;
                        n += 1;
                    }
                }
                format!("{:.2}", sum / n.max(1) as f64)
            })
            .collect();
        println!("  {a:>2}: {}", row.join(" "));
    }
    println!("자기 화자 중심과의 평균 닮음:");
    for a in &keys {
        let m: f32 = groups[a].iter().map(|(e, _)| cosine(e, &cent[a])).sum::<f32>() / groups[a].len() as f32;
        println!("  {a:>2}: {m:.2}");
    }
}
