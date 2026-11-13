//! 모델 목록과 내려받기 (진행률 · 취소 · 이어받기 없이 .part 후 교체)

use sha2::{Digest, Sha256};
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
    /// 원본 파일의 SHA-256 (내려받은 뒤 확인 — 손상·바꿔치기 방지)
    pub sha256: &'static str,
}

macro_rules! hf {
    ($f:literal) => {
        // main 대신 고정 커밋: 올라온 파일이 바뀌어도 해시(sha256)와 같은 버전을 받는다
        concat!("https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/", $f)
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
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
    },
    ModelSpec {
        id: "large-v3-turbo-q8_0",
        kind: ModelKind::Whisper,
        label: "Large v3 Turbo (q8)",
        note: "조금 더 정확 · 메모리 더 사용",
        file: "ggml-large-v3-turbo-q8_0.bin",
        url: hf!("ggml-large-v3-turbo-q8_0.bin"),
        size: 874_188_075,
        sha256: "317eb69c11673c9de1e1f0d459b253999804ec71ac4c23c17ecf5fbe24e259a1",
    },
    ModelSpec {
        id: "large-v3-q5_0",
        kind: ModelKind::Whisper,
        label: "Large v3 (q5)",
        note: "최고 정확도 · 느림 (GPU 권장)",
        file: "ggml-large-v3-q5_0.bin",
        url: hf!("ggml-large-v3-q5_0.bin"),
        size: 1_081_140_203,
        sha256: "d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1",
    },
    ModelSpec {
        id: "medium-q5_0",
        kind: ModelKind::Whisper,
        label: "Medium (q5)",
        note: "중간 사양 PC",
        file: "ggml-medium-q5_0.bin",
        url: hf!("ggml-medium-q5_0.bin"),
        size: 539_212_467,
        sha256: "19fea4b380c3a618ec4723c3eef2eb785ffba0d0538cf43f8f235e7b3b34220f",
    },
    ModelSpec {
        id: "small-q5_1",
        kind: ModelKind::Whisper,
        label: "Small (q5)",
        note: "저사양 PC · 정확도 낮음",
        file: "ggml-small-q5_1.bin",
        url: hf!("ggml-small-q5_1.bin"),
        size: 190_085_487,
        sha256: "ae85e4a935d7a567bd102fe55afc16bb595bdb618e11b2fc7591bc08120411bb",
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
        sha256: "aa3cfc16963a10586a9393f5035d6d6b57e98d358b347f80c2a30bf4f00ceba2",
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
        // reqwest blocking 의 timeout 은 연결·읽기 한 번마다 적용된다 → 큰 파일도 괜찮고, 멈춘 연결은 60초 뒤 끊긴다
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("badasugi/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut resp = client.get(spec.url).send()?;
    if !resp.status().is_success() {
        bail!("다운로드 실패: HTTP {}", resp.status().as_u16());
    }
    let total = resp.content_length().unwrap_or(spec.size);
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    // 받는 도중 실패·취소하면 받다 만 파일을 남기지 않는다
    let received = (|| -> Result<()> {
        let mut file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&part)?);
        let mut buf = vec![0u8; 1 << 16];
        let mut last_report = std::time::Instant::now();
        loop {
            if cancel.load(Ordering::Relaxed) {
                bail!("다운로드를 취소했습니다");
            }
            let n = resp.read(&mut buf).map_err(|e| anyhow!("다운로드가 끊겼어요. 인터넷 연결을 확인하고 다시 시도해 주세요. ({e})"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n])?;
            done += n as u64;
            if last_report.elapsed() >= Duration::from_millis(150) {
                progress(done, total);
                last_report = std::time::Instant::now();
            }
        }
        file.flush()?;
        Ok(())
    })();
    if let Err(e) = received {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    if spec.size > 0 && done != spec.size {
        let _ = std::fs::remove_file(&part);
        return Err(anyhow!("파일 크기가 맞지 않습니다 ({done} / {} 바이트). 다시 시도해 주세요.", spec.size));
    }
    let got = hex(&hasher.finalize());
    if !spec.sha256.is_empty() && !got.eq_ignore_ascii_case(spec.sha256) {
        let _ = std::fs::remove_file(&part);
        return Err(anyhow!("내려받은 파일이 원본과 달라요 (손상됐거나 바뀐 파일). 다시 시도해 주세요."));
    }
    std::fs::rename(&part, &dest)?;
    progress(done, total);
    Ok(dest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_model_has_a_sha256() {
        for m in WHISPER_MODELS.iter().chain(std::iter::once(&SPEAKER_MODEL)) {
            assert_eq!(m.sha256.len(), 64, "{}", m.id);
            assert!(m.sha256.chars().all(|c| c.is_ascii_hexdigit()), "{}", m.id);
        }
    }

    #[test]
    fn hex_of_sha256_matches_known_vector() {
        // SHA-256("abc")
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
