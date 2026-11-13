//! whisper.cpp 단독 속도 측정: cargo run --release --example bench -- <model.bin> <file.wav> [threads]
use std::time::Instant;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let threads: i32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);
    let actx: i32 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
    badasugi_core::perf::disable_power_throttling();
    let samples = badasugi_core::wav::read_16k_mono(args[2].as_ref())?;
    let t = Instant::now();
    let mut cp = WhisperContextParameters::default();
    cp.flash_attn(args.get(5).map(|s| s == "1").unwrap_or(false));
    let ctx = WhisperContext::new_with_params(&args[1], cp)?;
    eprintln!("load {:?}", t.elapsed());
    let mut state = ctx.create_state()?;
    let rounds: usize = std::env::var("BENCH_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    for round in 0..rounds {
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_language(Some("ko"));
        p.set_n_threads(threads);
        p.set_no_timestamps(true);
        if actx > 0 { p.set_audio_ctx(actx); }
        p.set_print_progress(false);
        p.set_print_realtime(false);
        let t = Instant::now();
        state.full(p, &samples)?;
        let text: String = state.as_iter().map(|s| s.to_string()).collect();
        eprintln!("round {round}: {:?} -> {text}", t.elapsed());
    }
    unsafe { whisper_rs_sys_timings(&ctx) };
    Ok(())
}

unsafe fn whisper_rs_sys_timings(_ctx: &WhisperContext) {}
