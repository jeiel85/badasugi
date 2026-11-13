//! Ogg Opus (RFC 7845) 녹음 파일 쓰기 · 읽기
//!
//! 회의 녹음은 16 kHz 모노 음성이라 Opus 24 kbps 로 충분하다. 1시간에 약 11 MB 로,
//! 같은 녹음을 WAV(PCM16)로 남길 때(약 115 MB)의 1/10 쯤이다.
//!
//! Ogg 페이지는 하나하나가 완결된 단위라, 1초마다 페이지를 닫아 디스크에 내보내면 녹음 중에도
//! 언제나 그때까지 들을 수 있고, 앱이 갑자기 꺼져도 마지막 1초 전까지는 남는다 (WAV 헤더 체크포인트와 같은 성질).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use ogg::writing::{PacketWriteEndInfo, PacketWriter};
use symphonia::core::audio::{AsAudioBufferRef, AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec};
use symphonia::core::codecs::{
    CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult, CODEC_TYPE_OPUS,
};
use symphonia::core::errors::{decode_error, unsupported_error, Result as SymResult};
use symphonia::core::formats::Packet;
use symphonia::core::support_codec;

use crate::SAMPLE_RATE;

/// 녹음 비트레이트 (bit/s). 16 kHz 음성(광대역)에 넉넉한 값.
pub const BITRATE: i32 = 24_000;

/// 한 패킷 = 20 ms (16 kHz 샘플 수)
const FRAME: usize = SAMPLE_RATE as usize / 50;
/// Ogg Opus 의 granule 은 원래 샘플레이트와 상관없이 언제나 48 kHz 기준
const GRANULE_RATE: u64 = 48_000;
const GRANULE_PER_INPUT: u64 = GRANULE_RATE / SAMPLE_RATE as u64;
const GRANULE_PER_FRAME: u64 = FRAME as u64 * GRANULE_PER_INPUT;
/// 이만큼(= 1초) 쌓이면 페이지를 닫고 디스크에 내보낸다
const FRAMES_PER_PAGE: u32 = 50;
/// 20 ms 패킷 하나의 최대 크기 (24 kbps 면 보통 60바이트 안팎)
const MAX_PACKET: usize = 1500;

/// 세션 녹음 파일 (16 kHz 모노 → Ogg Opus)
pub struct OpusRecorder {
    writer: PacketWriter<'static, BufWriter<File>>,
    encoder: ::opus::Encoder,
    serial: u32,
    /// 20 ms 가 안 돼서 아직 인코딩하지 않은 샘플
    pending: Vec<f32>,
    packet: Vec<u8>,
    /// 디코더가 앞에서 버릴 샘플 수 (48 kHz) = 인코더 지연
    pre_skip: u64,
    /// 지금까지 받은 16 kHz 샘플 수
    samples_in: u64,
    frames: u64,
    in_page: u32,
    /// 쓰기 오류는 한 번만 알린다 (디스크 가득 등 — 녹음·받아쓰기는 계속)
    failed: bool,
}

impl OpusRecorder {
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut encoder = ::opus::Encoder::new(SAMPLE_RATE, ::opus::Channels::Mono, ::opus::Application::Voip)
            .context("Opus 인코더를 만들 수 없어요")?;
        encoder.set_bitrate(::opus::Bitrate::Bits(BITRATE))?;
        encoder.set_signal(::opus::Signal::Voice)?;
        // lookahead 는 입력 샘플레이트 기준 → granule(48 kHz) 로
        let pre_skip = encoder.get_lookahead()?.max(0) as u64 * GRANULE_PER_INPUT;

        let file = File::create(path).with_context(|| format!("녹음 파일을 만들 수 없어요: {}", path.display()))?;
        let mut writer = PacketWriter::new(BufWriter::new(file));
        let serial = stream_serial();
        // 머리 패킷 두 개는 각자 자기 페이지에 (RFC 7845 §3)
        writer.write_packet(opus_head(pre_skip as u16), serial, PacketWriteEndInfo::EndPage, 0)?;
        writer.write_packet(opus_tags(), serial, PacketWriteEndInfo::EndPage, 0)?;
        writer.inner_mut().flush()?;
        Ok(Self {
            writer,
            encoder,
            serial,
            pending: Vec::with_capacity(FRAME),
            packet: vec![0; MAX_PACKET],
            pre_skip,
            samples_in: 0,
            frames: 0,
            in_page: 0,
            failed: false,
        })
    }

    /// 16 kHz 모노 샘플을 덧붙인다. 1초마다 페이지를 닫아 디스크에 내보낸다.
    pub fn write(&mut self, samples: &[f32]) {
        self.samples_in += samples.len() as u64;
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (FRAME - self.pending.len()).min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() == FRAME {
                let granule = (self.frames + 1) * GRANULE_PER_FRAME;
                let r = self.encode_pending(granule, false);
                self.note(r);
            }
        }
    }

    /// 남은 소리를 마저 인코딩하고 스트림을 닫는다.
    pub fn finish(mut self) -> Result<()> {
        // 인코더 지연(pre-skip)만큼 무음을 더 넣어 마지막 소리까지 디코딩되게 하고,
        // 마지막 페이지의 granule 을 실제 길이로 적어 덧붙인 무음은 잘라 내게 한다 (RFC 7845 §4.4)
        let end = self.pre_skip + self.samples_in * GRANULE_PER_INPUT;
        loop {
            let next = (self.frames + 1) * GRANULE_PER_FRAME;
            let last = next >= end;
            self.encode_pending(if last { end } else { next }, last)?;
            if last {
                break;
            }
        }
        self.writer.inner_mut().flush()?;
        Ok(())
    }

    /// `pending` 을 (모자라면 무음으로 채워) 패킷 하나로 인코딩해 쓴다
    fn encode_pending(&mut self, granule: u64, last: bool) -> Result<()> {
        self.pending.resize(FRAME, 0.0);
        let n = self.encoder.encode_float(&self.pending, &mut self.packet);
        // 인코딩이 실패해도 비운다 (안 비우면 write 가 같은 프레임을 끝없이 다시 시도한다)
        self.pending.clear();
        let n = n?;
        self.frames += 1;
        self.in_page += 1;
        let end = if last {
            PacketWriteEndInfo::EndStream
        } else if self.in_page >= FRAMES_PER_PAGE {
            PacketWriteEndInfo::EndPage
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        self.writer.write_packet(self.packet[..n].to_vec(), self.serial, end, granule)?;
        if end != PacketWriteEndInfo::NormalPacket {
            self.in_page = 0;
            self.writer.inner_mut().flush()?;
        }
        Ok(())
    }

    fn note(&mut self, r: Result<()>) {
        if let Err(e) = r {
            if !self.failed {
                self.failed = true;
                log::warn!("녹음 파일 쓰기 실패 (받아쓰기는 계속): {e}");
            }
        }
    }
}

/// 논리 스트림 번호 (파일마다 달라야 이어 붙여도 구분된다)
fn stream_serial() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (t.as_nanos() as u32) ^ std::process::id().rotate_left(16)
}

/// 식별 머리 (RFC 7845 §5.1)
fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut h = Vec::with_capacity(19);
    h.extend_from_slice(b"OpusHead");
    h.push(1); // 버전
    h.push(1); // 채널 수
    h.extend_from_slice(&pre_skip.to_le_bytes());
    h.extend_from_slice(&SAMPLE_RATE.to_le_bytes()); // 원래 샘플레이트 (참고용)
    h.extend_from_slice(&0i16.to_le_bytes()); // 출력 이득
    h.push(0); // 채널 매핑 0 (모노·스테레오)
    h
}

/// 주석 머리 (RFC 7845 §5.2)
fn opus_tags() -> Vec<u8> {
    let vendor = format!("badasugi {} ({})", env!("CARGO_PKG_VERSION"), ::opus::version());
    let mut h = Vec::with_capacity(16 + vendor.len());
    h.extend_from_slice(b"OpusTags");
    h.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    h.extend_from_slice(vendor.as_bytes());
    h.extend_from_slice(&0u32.to_le_bytes()); // 사용자 주석 없음
    h
}

// ───────────────────────── 읽기 (symphonia 디코더) ─────────────────────────

/// 120 ms (Opus 패킷 최대 길이) @ 48 kHz
const MAX_FRAMES: usize = 5760;

/// libopus 로 푸는 symphonia Opus 디코더 (symphonia 0.5 에는 Opus 디코더가 없다)
///
/// symphonia 의 Ogg 분리기가 넘겨 주는 패킷을 48 kHz 로 디코딩한다. 모노·스테레오만 (회의 녹음·음성 메모는 거의 이것).
pub struct OpusDecoder {
    // libopus 디코더는 Send 만 된다. decode 는 &mut self 라 잠글 일은 없고, Sync 를 맞추려고 감싼다.
    dec: Mutex<::opus::Decoder>,
    params: CodecParameters,
    channels: usize,
    pcm: Vec<f32>,
    buf: AudioBuffer<f32>,
}

impl Decoder for OpusDecoder {
    fn try_new(params: &CodecParameters, _options: &DecoderOptions) -> SymResult<Self> {
        let count = params.channels.map(Channels::count).unwrap_or(1);
        // 채널 매핑 1 이상은 스트림이 하나일 때만 (여러 스트림 = 다채널은 기본 디코더로 못 푼다)
        if let Some(head) = params.extra_data.as_deref() {
            if head.len() >= 19 && head[18] != 0 && head.get(19).copied() != Some(1) {
                return unsupported_error("opus: 다채널(멀티스트림)은 지원하지 않아요");
            }
        }
        let (ch, layout) = match count {
            1 => (::opus::Channels::Mono, Channels::FRONT_LEFT),
            2 => (::opus::Channels::Stereo, Channels::FRONT_LEFT | Channels::FRONT_RIGHT),
            _ => return unsupported_error("opus: 다채널은 지원하지 않아요"),
        };
        let dec = ::opus::Decoder::new(GRANULE_RATE as u32, ch).or_else(|_| decode_error("opus: 디코더를 만들 수 없어요"))?;
        Ok(Self {
            dec: Mutex::new(dec),
            params: params.clone(),
            channels: count,
            pcm: vec![0.0; MAX_FRAMES * count],
            buf: AudioBuffer::new(MAX_FRAMES as u64, SignalSpec::new(GRANULE_RATE as u32, layout)),
        })
    }

    fn supported_codecs() -> &'static [CodecDescriptor] {
        &[support_codec!(CODEC_TYPE_OPUS, "opus", "Opus (libopus)")]
    }

    fn reset(&mut self) {
        if let Ok(d) = self.dec.get_mut() {
            let _ = d.reset_state();
        }
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    fn decode(&mut self, packet: &Packet) -> SymResult<AudioBufferRef<'_>> {
        self.buf.clear();
        let dec = match self.dec.get_mut() {
            Ok(d) => d,
            Err(p) => p.into_inner(),
        };
        let n = match dec.decode_float(&packet.data, &mut self.pcm, false) {
            Ok(n) => n.min(MAX_FRAMES),
            Err(_) => return decode_error("opus: 손상된 패킷"),
        };
        self.buf.render_reserved(Some(n));
        for c in 0..self.channels {
            let plane = self.buf.chan_mut(c);
            for (i, s) in plane.iter_mut().enumerate() {
                *s = self.pcm[i * self.channels + c];
            }
        }
        self.buf.trim(packet.trim_start() as usize, packet.trim_end() as usize);
        Ok(self.buf.as_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult::default()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        self.buf.as_audio_buffer_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::AudioFile;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("badasugi-opus-{}-{name}", std::process::id()))
    }

    fn tone(secs: f32, amp: f32) -> Vec<f32> {
        (0..(SAMPLE_RATE as f32 * secs) as usize)
            .map(|i| amp * (2.0 * std::f32::consts::PI * 300.0 * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    fn decode_all(path: &Path) -> (Option<u64>, Vec<f32>) {
        let mut f = AudioFile::open(path).unwrap();
        let dur = f.duration_ms();
        let mut out = Vec::new();
        while f.read(&mut out).unwrap() {}
        (dur, out)
    }

    #[test]
    fn records_and_reads_back_same_length() {
        let path = temp_path("rec.opus");
        let secs = 3.37f32; // 20 ms 로 나누어떨어지지 않는 길이
        let src = tone(secs, 0.5);
        {
            let mut r = OpusRecorder::create(&path).unwrap();
            // 실제 녹음처럼 들쭉날쭉한 크기로 나눠 넣는다
            for c in src.chunks(437) {
                r.write(c);
            }
            r.finish().unwrap();
        }
        let size = std::fs::metadata(&path).unwrap().len();
        let (dur, out) = decode_all(&path);
        let _ = std::fs::remove_file(&path);

        // 24 kbps ≈ 3 KB/s (WAV 는 32 KB/s)
        let wav = (src.len() * 2) as u64;
        assert!(size < wav / 6, "opus {size} B vs wav {wav} B");
        let dur = dur.expect("길이 정보") as i64;
        assert!((dur - (secs * 1000.0) as i64).abs() <= 30, "길이 {dur} ms");
        let want = src.len() as i64;
        // gapless 를 끄고 읽으므로 앞쪽 인코더 지연(~6.5 ms = 104 샘플)만큼 길 수 있다
        assert!((out.len() as i64 - want).abs() < 400, "len {} want {want}", out.len());
        let (a, b) = (rms(&src[1600..src.len() - 1600]), rms(&out[1600..src.len() - 1600]));
        assert!((a - b).abs() / a < 0.1, "rms src {a} out {b}");
    }

    #[test]
    fn growing_file_is_readable_while_recording() {
        let path = temp_path("live.opus");
        let mut r = OpusRecorder::create(&path).unwrap();
        // 2.5초 녹음 중 (finish 전) → 1초마다 닫힌 페이지까지, 즉 2초는 읽혀야 한다
        r.write(&tone(2.5, 0.3));
        let (_, out) = decode_all(&path);
        assert!(
            (out.len() as i64 - 2 * SAMPLE_RATE as i64).abs() < 400,
            "녹음 중 읽힌 길이 {}",
            out.len()
        );
        assert!(rms(&out[1600..]) > 0.15);
        r.finish().unwrap();
        let (_, out) = decode_all(&path);
        let _ = std::fs::remove_file(&path);
        assert!((out.len() as i64 - (2.5 * SAMPLE_RATE as f32) as i64).abs() < 400, "len {}", out.len());
    }

    #[test]
    fn empty_recording_is_valid() {
        let path = temp_path("empty.opus");
        OpusRecorder::create(&path).unwrap().finish().unwrap();
        let (_, out) = decode_all(&path);
        let _ = std::fs::remove_file(&path);
        assert!(out.len() < 400, "len {}", out.len());
    }
}
