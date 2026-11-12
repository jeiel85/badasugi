//! ONNX Runtime 공용 환경. (Silero VAD · 화자 임베딩 모델이 공유)

use std::path::Path;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use ort::environment::Environment;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;

static ENV: OnceLock<Environment> = OnceLock::new();
static ENV_INIT: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// 프로세스당 하나뿐인 ORT 환경을 돌려준다.
pub fn env() -> Result<&'static Environment> {
    if let Some(env) = ENV.get() {
        return Ok(env);
    }
    let _guard = ENV_INIT.lock();
    if let Some(env) = ENV.get() {
        return Ok(env);
    }
    let env = ort::init()
        .with_name("badasugi")
        .build()
        .map_err(|e| anyhow!("ONNX Runtime 초기화 실패: {e}"))?;
    Ok(ENV.get_or_init(|| env))
}

fn builder(threads: usize) -> Result<ort::session::builder::SessionBuilder> {
    let b = Session::builder(env()?).map_err(|e| anyhow!("{e}"))?;
    let b = b
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow!("{e}"))?;
    let b = b.with_intra_threads(threads.max(1)).map_err(|e| anyhow!("{e}"))?;
    let b = b.with_inter_threads(1).map_err(|e| anyhow!("{e}"))?;
    Ok(b)
}

pub fn session_from_memory(bytes: &[u8], threads: usize) -> Result<Session> {
    builder(threads)?
        .commit_from_memory(bytes)
        .map_err(|e| anyhow!("ONNX 모델 로드 실패: {e}"))
}

pub fn session_from_file(path: &Path, threads: usize) -> Result<Session> {
    builder(threads)?
        .commit_from_file(path)
        .map_err(|e| anyhow!("ONNX 모델 로드 실패({}): {e}", path.display()))
}
