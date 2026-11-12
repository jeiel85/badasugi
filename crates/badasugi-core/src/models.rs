//! 모델 목록과 내려받기 (진행률 · 취소 · 이어받기 없이 .part 후 교체)

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ModelKind {
    Whisper,
    Speaker,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSpec {
    pub id: &'static str,
    pub kind: ModelKind,
    pub label: &'static str,
    pub note: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub size: u64,
}

macro_rules! hf {
    ($f:literal) => {
        concat!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/", $f)
    };
}

pub const WHISPER_MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "large-v3-turbo-q5_0",
        kind: ModelKind::Whisper,
        label: "Large v3 Turbo (q5)",
        note: "추천 · 빠르고 한국어 정확도가 높음",
        file: "ggml-large-v3-turbo-q5_0.bin",
        url: hf!("ggml-large-v3-turbo-q5_0.bin"),
        size: 574_041_195,
    },
    ModelSpec {
        id: "large-v3-turbo-q8_0",
        kind: ModelKind::Whisper,
        label: "Large v3 Turbo (q8)",
        note: "조금 더 정확 · 메모리 더 사용",
        file: "ggml-large-v3-turbo-q8_0.bin",
        url: hf!("ggml-large-v3-turbo-q8_0.bin"),
        size: 874_188_075,
    },
    ModelSpec {
        id: "large-v3-q5_0",
        kind: ModelKind::Whisper,
        label: "Large v3 (q5)",
        note: "최고 정확도 · 느림 (GPU 권장)",
        file: "ggml-large-v3-q5_0.bin",
        url: hf!("ggml-large-v3-q5_0.bin"),
        size: 1_081_140_203,
    },
    ModelSpec {
        id: "medium-q5_0",
        kind: ModelKind::Whisper,
        label: "Medium (q5)",
        note: "중간 사양 PC",
        file: "ggml-medium-q5_0.bin",
        url: hf!("ggml-medium-q5_0.bin"),
        size: 539_212_467,
    },
    ModelSpec {
        id: "small-q5_1",
        kind: ModelKind::Whisper,
        label: "Small (q5)",
        note: "저사양 PC · 정확도 낮음",
        file: "ggml-small-q5_1.bin",
        url: hf!("ggml-small-q5_1.bin"),
        size: 190_085_487,
    },
];

pub const SPEAKER_MODEL: ModelSpec = ModelSpec {
    id: "campplus-zh-en",
    kind: ModelKind::Speaker,
    label: "화자 인식 (3D-Speaker CAM++)",
    note: "화자 분리용 · 28MB",
    file: "3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
    url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
    size: 28_281_164,
};

impl ModelSpec {
    pub fn path_in(&self, dir: &Path) -> PathBuf {
        dir.join(self.file)
    }

    pub fn is_ready(&self, dir: &Path) -> bool {
        std::fs::metadata(self.path_in(dir))
            .map(|m| m.len() == self.size || (self.size == 0 && m.len() > 0))
            .unwrap_or(false)
    }
}

pub fn find_whisper(id: &str) -> Option<&'static ModelSpec> {
    WHISPER_MODELS.iter().find(|m| m.id == id)
}

/// 모델 폴더에 사용자가 직접 넣은 ggml 모델까지 포함한 목록
pub fn custom_whisper_files(dir: &Path) -> Vec<PathBuf> {
    let known: Vec<&str> = WHISPER_MODELS.iter().map(|m| m.file).collect();
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("ggml-") && name.ends_with(".bin") && !known.contains(&name)
        })
        .collect();
    out.sort();
    out
}

/// 내려받기. `progress(받은 바이트, 전체 바이트)` 를 주기적으로 부른다.
pub fn download(
    spec: &ModelSpec,
    dir: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let dest = spec.path_in(dir);
    if spec.is_ready(dir) {
        progress(spec.size, spec.size);
        return Ok(dest);
    }
    let part = dir.join(format!("{}.part", spec.file));
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(None)
        .user_agent(concat!("badasugi/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut resp = client.get(spec.url).send()?;
    if !resp.status().is_success() {
        bail!("다운로드 실패: HTTP {}", resp.status().as_u16());
    }
    let total = resp.content_length().unwrap_or(spec.size);
    let mut file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&part)?);
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    let mut last_report = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            bail!("다운로드를 취소했습니다");
        }
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        if last_report.elapsed() >= Duration::from_millis(150) {
            progress(done, total);
            last_report = std::time::Instant::now();
        }
    }
    file.flush()?;
    drop(file);
    if spec.size > 0 && done != spec.size {
        let _ = std::fs::remove_file(&part);
        return Err(anyhow!("파일 크기가 맞지 않습니다 ({done} / {} 바이트). 다시 시도해 주세요.", spec.size));
    }
    std::fs::rename(&part, &dest)?;
    progress(done, total);
    Ok(dest)
}
