//! 화자 전환 진단: cargo run --release --example turns -- <speaker.onnx> <file.wav> <start_s> <end_s>
use badasugi_core::speaker::{cosine, SpeakerEncoder};

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let mut enc = SpeakerEncoder::new(a[1].as_ref())?;
    let all = badasugi_core::wav::read_16k_mono(a[2].as_ref())?;
    let (s, e): (f32, f32) = (a[3].parse()?, a[4].parse()?);
    let seg = &all[(s * 16000.0) as usize..((e * 16000.0) as usize).min(all.len())];
    let (win, hop) = (24000usize, 12000usize);
    let mut embs = Vec::new();
    let mut t = 0;
    while t + win <= seg.len() {
        embs.push((t, enc.embed(&seg[t..t + win])?.unwrap()));
        t += hop;
    }
    for (i, (ti, ei)) in embs.iter().enumerate() {
        let row: Vec<String> = embs.iter().map(|(_, ej)| format!("{:>5.2}", cosine(ei, ej))).collect();
        println!("{:>5.2}s {}", s + *ti as f32 / 16000.0, row.join(" "));
        let _ = i;
    }
    Ok(())
}
