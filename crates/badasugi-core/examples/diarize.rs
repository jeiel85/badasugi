//! 화자 구분 검증 도구 (음성 인식 없이 화자만)
//!
//! ```text
//! cargo run --release --example diarize -- <speaker.onnx> <회의.wav> [옵션]
//!   --truth <truth.json>       정답 [{startMs, endMs, speaker}] → 실시간 · 다시 정리 정확도 비교
//!   --threshold <0.45>         화자 구분 기준
//!   --max <n>                  최대 화자 수
//!   --known <voices.json>      이름을 아는 목소리 [{name, embedding}] → 목소리로 이름 알아보기
//!   --save-voices <out.json>   다시 정리한 화자 목소리를 정답 이름으로 저장 (다음 회의의 --known 으로)
//!   -v                         조각마다 출력
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;

use badasugi_core::diarize::{match_voices, Diarizer, KnownVoice};
use badasugi_core::speaker::{cosine, DiarizationConfig, SpeakerEncoder};
use badasugi_core::vad::{Segmenter, SegmenterConfig};
use badasugi_core::samples_to_ms;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Truth {
    start_ms: u64,
    end_ms: u64,
    speaker: String,
}

/// 예측 화자 ↔ 정답 화자를 겹친 시간이 큰 쌍부터 하나씩 맞춰, 맞게 붙은 시간의 비율.
/// 조각이 정답 발화 여러 개에 걸치면(놓친 화자 전환) 겹친 시간을 모두 센다 — 가장 큰 겹침만 세면 정확도가 부푼다.
fn accuracy(parts: &[(u64, u64, u32)], truth: &[Truth]) -> (f64, BTreeMap<u32, String>) {
    let mut ov: BTreeMap<(u32, String), u64> = BTreeMap::new();
    let mut total = 0u64;
    for &(s, e, l) in parts {
        for t in truth {
            let o = e.min(t.end_ms).saturating_sub(s.max(t.start_ms));
            if o > 0 {
                *ov.entry((l, t.speaker.clone())).or_default() += o;
                total += o;
            }
        }
    }
    let mut pairs: Vec<_> = ov.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1));
    let (mut used_p, mut used_t) = (std::collections::BTreeSet::new(), std::collections::BTreeSet::new());
    let mut map = BTreeMap::new();
    let mut correct = 0;
    for ((p, t), o) in pairs {
        if !used_p.contains(&p) && !used_t.contains(&t) {
            used_p.insert(p);
            used_t.insert(t.clone());
            map.insert(p, t);
            correct += o;
        }
    }
    (correct as f64 / total.max(1) as f64 * 100.0, map)
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let model = PathBuf::from(&a[0]);
    let wav = PathBuf::from(&a[1]);
    let mut cfg = DiarizationConfig::default();
    let (mut truth_p, mut known_p, mut save_p, mut verbose) = (None, None, None, false);
    let mut i = 2;
    while i < a.len() {
        match a[i].as_str() {
            "--truth" => truth_p = Some(a[i + 1].clone()),
            "--threshold" => cfg.threshold = a[i + 1].parse()?,
            "--max" => cfg.max_speakers = a[i + 1].parse()?,
            "--known" => known_p = Some(a[i + 1].clone()),
            "--save-voices" => save_p = Some(a[i + 1].clone()),
            "-v" => {
                verbose = true;
                i += 1;
                continue;
            }
            o => anyhow::bail!("알 수 없는 옵션 {o}"),
        }
        i += 2;
    }
    let known: Vec<KnownVoice> = match &known_p {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
        None => Vec::new(),
    };
    let truth: Vec<Truth> = match &truth_p {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
        None => Vec::new(),
    };

    let samples = badasugi_core::wav::read_16k_mono(&wav)?;
    let mut seg = Segmenter::new(SegmenterConfig::default())?;
    let mut d = Diarizer::new(SpeakerEncoder::new(&model)?, cfg.clone(), known.clone());
    let mut parts: Vec<(u64, u64, u64, u32)> = Vec::new(); // id, start, end, online
    let mut segs = Vec::new();
    let mut next = 0u64;
    let mut feed = |segs: &mut Vec<_>, d: &mut Diarizer, parts: &mut Vec<_>| {
        for s in segs.drain(..) {
            for p in d.split(s) {
                let id = next;
                next += 1;
                let spk = d.assign(id, &p);
                parts.push((id, samples_to_ms(p.start), samples_to_ms(p.end), spk));
                if let Some(name) = d.recognize(spk) {
                    println!("  [녹음 중] {:>6.1}s 화자{} = {name}", samples_to_ms(p.end) as f32 / 1000.0, spk + 1);
                }
            }
        }
    };
    for chunk in samples.chunks(8000) {
        seg.push(chunk, &mut segs)?;
        feed(&mut segs, &mut d, &mut parts);
    }
    seg.flush(&mut segs);
    feed(&mut segs, &mut d, &mut parts);

    let r = d.finish(&BTreeMap::new());
    let refined: BTreeMap<u64, u32> = r.labels.iter().map(|&(id, _, l)| (id, l)).collect();
    let online: Vec<(u64, u64, u32)> = parts.iter().map(|&(_, s, e, l)| (s, e, l)).collect();
    let after: Vec<(u64, u64, u32)> = parts.iter().map(|&(id, s, e, _)| (s, e, refined[&id])).collect();
    let n = |v: &[(u64, u64, u32)]| v.iter().map(|x| x.2).collect::<std::collections::BTreeSet<_>>().len();

    if verbose {
        for (&(_, s, e, on), &(_, _, re)) in parts.iter().zip(&after) {
            let t = truth
                .iter()
                .max_by_key(|t| e.min(t.end_ms).saturating_sub(s.max(t.start_ms)))
                .map(|t| t.speaker.as_str())
                .unwrap_or("-");
            println!(
                "{:>7.2}~{:>7.2}s 정답 {t} · 실시간 {} → 정리 {}{}",
                s as f32 / 1000.0,
                e as f32 / 1000.0,
                on + 1,
                re + 1,
                if on != re { "  *" } else { "" }
            );
        }
    }
    println!("조각 {}개 · 옮김 {}개", parts.len(), r.moved());
    // 화자 목소리끼리 닮은 정도
    for (x, vx) in r.voices.iter().enumerate() {
        let row: Vec<String> = r.voices.iter().map(|vy| format!("{:>5.2}", cosine(&vx.embedding, &vy.embedding))).collect();
        println!("  화자{:<2} {:>5.1}초 | {}", vx.speaker + 1, vx.secs, row.join(" "));
        let _ = x;
    }
    if !truth.is_empty() {
        let ntruth = truth.iter().map(|t| t.speaker.as_str()).collect::<std::collections::BTreeSet<_>>().len();
        let (a0, _) = accuracy(&online, &truth);
        let (a1, map) = accuracy(&after, &truth);
        println!("정답 화자 {ntruth}명");
        println!("실시간   : 화자 {}명 · 정확도 {a0:.1}%", n(&online));
        println!("다시 정리: 화자 {}명 · 정확도 {a1:.1}%", n(&after));
        if let Some(p) = &save_p {
            let voices: Vec<KnownVoice> = r
                .voices
                .iter()
                .filter_map(|v| {
                    map.get(&v.speaker).map(|name| KnownVoice {
                        name: name.clone(),
                        embedding: v.embedding.clone(),
                    })
                })
                .collect();
            std::fs::write(p, serde_json::to_string(&voices)?)?;
            println!("목소리 {}개 저장 → {p}", voices.len());
        }
        if !known.is_empty() {
            let names = match_voices(&r.voices, &known, 5.0);
            for (spk, name) in &names {
                println!("  [회의 끝] 화자{} = {name} (정답 {})", spk + 1, map.get(spk).map_or("-", |s| s.as_str()));
            }
            for v in &r.voices {
                let sims: Vec<String> = known
                    .iter()
                    .map(|k| format!("{}:{:.2}", k.name, cosine(&v.embedding, &k.embedding)))
                    .collect();
                println!("    화자{} ↔ {}", v.speaker + 1, sims.join(" "));
            }
        }
    }
    Ok(())
}
