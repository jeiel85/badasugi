//! 오디오 파일 디코딩 (symphonia): wav · mp3 · m4a/mp4/mov(aac) · flac · ogg(vorbis) → 16 kHz 모노.
//!
//! 파일 전체를 한 번에 메모리에 풀지 않고, `read` 할 때마다 다음 몇 패킷만 디코딩한다 (1시간짜리 파일도 메모리 일정).

use std::fs::File;
use std::path::Path;

use anyhow::{anyhow, Result};
use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::resample::Resampler;
use crate::SAMPLE_RATE;

/// 불러올 수 있는 파일 확장자 (소문자)
pub const AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "m4a", "mp4", "mov", "aac", "flac", "ogg", "oga"];

const UNSUPPORTED: &str = "지원하지 않는 오디오 형식이에요 (mp3·m4a·wav·flac·ogg 지원)";

/// `read` 한 번에 최소 이만큼(16 kHz 샘플 = 100 ms) 만들어 돌려준다
const MIN_READ: usize = SAMPLE_RATE as usize / 10;

/// 손상된 패킷이 이만큼 연달아 나오면 더 읽지 않는다
const MAX_BAD_PACKETS: usize = 1000;

/// 스트리밍 오디오 파일 리더 (16 kHz 모노 f32 로 변환)
pub struct AudioFile {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    duration_ms: Option<u64>,
    resampler: Option<Resampler>,
    /// 디코딩 결과 → 인터리브 f32 (사양이 바뀌면 다시 만든다)
    buf: Option<(SampleBuffer<f32>, SignalSpec, usize)>,
    mono: Vec<f32>,
    bad_packets: usize,
    eof: bool,
    /// 이번 호출에서 이미 샘플을 내보낸 뒤 난 오류는 다음 호출에서 돌려준다
    deferred_err: Option<anyhow::Error>,
}

impl AudioFile {
    /// 파일을 열고 형식을 확인한다. 지원하지 않는 형식이거나 오디오 트랙이 없으면 바로 에러.
    pub fn open(path: &Path) -> Result<AudioFile> {
        let file = File::open(path).map_err(|e| anyhow!("파일을 열 수 없어요: {} ({e})", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(&ext.to_ascii_lowercase());
        }
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .map_err(|e| {
                log::info!("오디오 형식 판별 실패 ({}): {e}", path.display());
                anyhow!(UNSUPPORTED)
            })?;
        let format = probed.format;
        let (track_id, decoder, duration_ms) = select_track(format.as_ref()).ok_or_else(|| anyhow!(UNSUPPORTED))?;
        Ok(AudioFile {
            format,
            decoder,
            track_id,
            duration_ms,
            resampler: None,
            buf: None,
            mono: Vec::new(),
            bad_packets: 0,
            eof: false,
            deferred_err: None,
        })
    }

    /// 전체 길이 (컨테이너에 정보가 없으면 None)
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    /// 다음 조각(한 패킷 이상)을 디코딩해 16 kHz 모노로 `out` 에 덧붙인다.
    ///
    /// 더 읽을 게 있으면 `Ok(true)`, 끝이면 `Ok(false)` (이때는 아무것도 덧붙이지 않는다).
    /// 손상된 패킷은 건너뛴다. 더 읽을 수 없는 오류는 `Err` 이고, 그때는 아무것도 덧붙이지 않는다
    /// (오류 전에 나온 샘플은 `Ok(true)` 로 먼저 돌려주고 오류는 다음 호출에서 돌려준다).
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        if let Some(e) = self.deferred_err.take() {
            return Err(e);
        }
        if self.eof {
            return Ok(false);
        }
        let start = out.len();
        // 손상된 파일에서 디코더가 패닉하더라도 앱 전체가 죽지 않게
        let err = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.read_inner(out, start))) {
            Ok(Ok(())) => return Ok(out.len() > start || !self.eof),
            Ok(Err(e)) => e,
            Err(_) => anyhow!("오디오를 디코딩하다 문제가 생겼어요. 파일이 손상되었을 수 있어요."),
        };
        self.eof = true;
        if out.len() > start {
            self.deferred_err = Some(err);
            return Ok(true);
        }
        Err(err)
    }

    /// 최소 `MIN_READ` 만큼 만들거나 스트림 끝(`eof`)까지 디코딩한다. Err 는 더 읽을 수 없는 오류.
    fn read_inner(&mut self, out: &mut Vec<f32>, start: usize) -> Result<()> {
        while out.len() - start < MIN_READ {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    self.eof = true;
                    return Ok(());
                }
                Err(SymError::ResetRequired) => {
                    // 이어 붙인 스트림(체인 ogg 등): 트랙을 다시 고른다
                    let Some((id, dec, _)) = select_track(self.format.as_ref()) else {
                        self.eof = true;
                        return Ok(());
                    };
                    self.track_id = id;
                    self.decoder = dec;
                    self.buf = None;
                    continue;
                }
                Err(e) => return Err(anyhow!("오디오 파일을 끝까지 읽지 못했어요: {e}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    self.bad_packets = 0;
                    if decoded.frames() == 0 {
                        continue;
                    }
                    let spec = *decoded.spec();
                    let cap = decoded.capacity();
                    let ch = spec.channels.count().max(1);
                    let reuse = matches!(&self.buf, Some((_, s, c)) if *s == spec && *c >= cap);
                    if !reuse {
                        self.buf = Some((SampleBuffer::new(cap as u64, spec), spec, cap));
                    }
                    let (sbuf, _, _) = self.buf.as_mut().expect("방금 만든 버퍼");
                    sbuf.copy_interleaved_ref(decoded);
                    // 채널 평균으로 모노 → 16 kHz
                    self.mono.clear();
                    self.mono
                        .extend(sbuf.samples().chunks_exact(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
                    let r = self
                        .resampler
                        .get_or_insert_with(|| Resampler::new(spec.rate, SAMPLE_RATE));
                    if r.in_rate() != spec.rate {
                        *r = Resampler::new(spec.rate, SAMPLE_RATE);
                    }
                    r.process(&self.mono, out);
                }
                // 손상된 패킷은 버리고 다음 패킷으로
                Err(SymError::DecodeError(e)) => self.skip_bad(e)?,
                Err(SymError::IoError(e)) => self.skip_bad(&e.to_string())?,
                // 신호 사양이 바뀐다: 다음 패킷부터 새 사양으로
                Err(SymError::ResetRequired) => self.buf = None,
                Err(e) => return Err(anyhow!("오디오를 디코딩하지 못했어요: {e}")),
            }
        }
        Ok(())
    }

    fn skip_bad(&mut self, why: &str) -> Result<()> {
        log::debug!("손상된 오디오 패킷을 건너뜀: {why}");
        self.bad_packets += 1;
        if self.bad_packets >= MAX_BAD_PACKETS {
            return Err(anyhow!("오디오 파일이 손상되어 더 읽을 수 없어요"));
        }
        Ok(())
    }
}

/// 디코더를 만들 수 있는 첫 오디오 트랙 (기본 트랙 우선) → (트랙 ID, 디코더, 길이 ms)
fn select_track(format: &dyn FormatReader) -> Option<(u32, Box<dyn Decoder>, Option<u64>)> {
    let codecs = symphonia::default::get_codecs();
    let opts = DecoderOptions::default();
    let default_id = format.default_track().map(|t| t.id);
    let mut tracks: Vec<_> = format
        .tracks()
        .iter()
        .filter(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .collect();
    tracks.sort_by_key(|t| Some(t.id) != default_id);
    for t in tracks {
        match codecs.make(&t.codec_params, &opts) {
            Ok(d) => return Some((t.id, d, duration_ms(&t.codec_params))),
            Err(e) => log::info!("트랙 {} 은 디코딩할 수 없음: {e}", t.id),
        }
    }
    None
}

fn duration_ms(p: &CodecParameters) -> Option<u64> {
    let n = p.n_frames?;
    if let Some(tb) = p.time_base {
        let t = tb.calc_time(n);
        return Some(t.seconds * 1000 + (t.frac * 1000.0) as u64);
    }
    let sr = p.sample_rate.filter(|&r| r > 0)?;
    Some(n * 1000 / sr as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("badasugi-decode-{}-{name}", std::process::id()))
    }

    #[test]
    fn decodes_44k_stereo_wav_to_16k_mono() {
        let path = temp_path("stereo44k.wav");
        let sr = 44_100u32;
        let secs = 1.5f32;
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: sr,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            for i in 0..(sr as f32 * secs) as usize {
                let s = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin();
                // 왼쪽 0.6, 오른쪽 0.2 → 모노 평균 0.4
                w.write_sample((0.6 * s * i16::MAX as f32) as i16).unwrap();
                w.write_sample((0.2 * s * i16::MAX as f32) as i16).unwrap();
            }
            w.finalize().unwrap();
        }

        let mut f = AudioFile::open(&path).unwrap();
        assert_eq!(f.duration_ms(), Some(1500));
        let mut out = Vec::new();
        let mut reads = 0;
        while f.read(&mut out).unwrap() {
            reads += 1;
        }
        // 끝난 뒤에는 계속 false, 아무것도 덧붙이지 않는다
        let n = out.len();
        assert!(!f.read(&mut out).unwrap());
        assert_eq!(out.len(), n);
        let _ = std::fs::remove_file(&path);

        assert!(reads > 5, "조각 단위로 읽어야 함 (reads {reads})");
        let want = (secs * SAMPLE_RATE as f32) as i64;
        assert!((n as i64 - want).abs() < 50, "len {n}, want {want}");
        let tail = &out[1000..n - 1000];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        assert!((rms - 0.4 * std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01, "rms {rms}");
    }

    #[test]
    fn rejects_unsupported_and_missing() {
        let path = temp_path("note.mp3");
        std::fs::write(&path, "이건 오디오가 아니에요. just some text, no audio here.".repeat(200)).unwrap();
        let err = AudioFile::open(&path).err().expect("텍스트는 열리면 안 됨").to_string();
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("지원하지 않는 오디오 형식"), "{err}");

        let err = AudioFile::open(&temp_path("없는파일.wav")).err().expect("없는 파일").to_string();
        assert!(err.contains("파일을 열 수 없어요"), "{err}");
    }
}
