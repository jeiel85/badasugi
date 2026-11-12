//! WAV 입출력 도우미

use std::io::Cursor;
use std::path::Path;

use anyhow::{Context, Result};

use crate::SAMPLE_RATE;

/// WAV 파일을 16 kHz 모노 f32 로 읽는다 (샘플레이트·채널 자동 변환)
pub fn read_16k_mono(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("WAV 열기 실패: {}", path.display()))?;
    let spec = r.spec();
    let ch = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()?
        }
    };
    let mono: Vec<f32> = interleaved
        .chunks_exact(ch)
        .map(|f| f.iter().sum::<f32>() / ch as f32)
        .collect();
    Ok(crate::resample::resample_all(&mono, spec.sample_rate, SAMPLE_RATE))
}

/// 16 kHz 모노 PCM16 WAV 바이트 (API 업로드용)
pub fn encode_pcm16(samples: &[f32]) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buf = Cursor::new(Vec::with_capacity(samples.len() * 2 + 64));
    {
        let mut w = hound::WavWriter::new(&mut buf, spec)?;
        for &s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        w.finalize()?;
    }
    Ok(buf.into_inner())
}

/// 세션 녹음 파일 (16 kHz 모노 PCM16)
pub struct WavRecorder {
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
}

impl WavRecorder {
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        Ok(Self {
            writer: hound::WavWriter::create(path, spec)?,
        })
    }

    pub fn write(&mut self, samples: &[f32]) {
        for &s in samples {
            let _ = self.writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
        }
    }

    pub fn finish(self) -> Result<()> {
        self.writer.finalize()?;
        Ok(())
    }
}
