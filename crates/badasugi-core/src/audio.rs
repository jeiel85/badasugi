//! 오디오 캡처: 마이크 · 시스템 소리(WASAPI 루프백) · 둘 다.
//!
//! cpal 콜백에서는 모노로 줄여 채널로 보내기만 한다. 리샘플·믹싱은 파이프라인 스레드가 맡는다.

use std::thread::JoinHandle;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    /// 마이크만 (대면 회의)
    #[default]
    Mic,
    /// PC 에서 나는 소리만 (온라인 회의 상대방, 영상)
    System,
    /// 둘 다 (온라인 회의에서 나 + 상대방)
    Both,
}

impl SourceKind {
    pub fn uses_mic(self) -> bool {
        matches!(self, SourceKind::Mic | SourceKind::Both)
    }
    pub fn uses_system(self) -> bool {
        matches!(self, SourceKind::System | SourceKind::Both)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CaptureConfig {
    pub source: SourceKind,
    /// 마이크 장치 ID (없으면 기본 장치)
    pub mic_device: Option<String>,
}

pub const SRC_MIC: u8 = 0;
pub const SRC_SYSTEM: u8 = 1;

/// 콜백 → 파이프라인 으로 넘어가는 모노 오디오 조각 (장치 샘플레이트)
pub struct RawChunk {
    pub source: u8,
    pub rate: u32,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

pub fn list_input_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default_id = host.default_input_device().and_then(|d| d.id().ok()).map(|i| i.to_string());
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|d| {
            let id = d.id().ok()?.to_string();
            Some(DeviceInfo {
                is_default: default_id.as_deref() == Some(id.as_str()),
                name: d.to_string(),
                id,
            })
        })
        .collect()
}

pub struct Capture {
    stop_tx: Sender<()>,
    handle: Option<JoinHandle<()>>,
    pub mic_name: Option<String>,
    pub system_name: Option<String>,
}

impl Capture {
    /// 캡처를 시작한다. 장치 문제는 여기서 바로 에러로 돌려준다.
    pub fn start(cfg: &CaptureConfig, tx: Sender<RawChunk>) -> Result<Capture> {
        // 테스트·데모용: 장치 대신 WAV 파일을 실시간 속도로 흘려 넣는다 (스피커로 소리를 내지 않고 전체 경로 검증)
        if let Ok(path) = std::env::var("BADASUGI_TEST_WAV") {
            return Self::start_file(std::path::PathBuf::from(path), tx);
        }
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(Option<String>, Option<String>)>>(1);
        let cfg = cfg.clone();
        let handle = std::thread::Builder::new()
            .name("bd-capture".into())
            .spawn(move || capture_thread(cfg, tx, stop_rx, ready_tx))?;
        match ready_rx.recv() {
            Ok(Ok((mic_name, system_name))) => Ok(Capture {
                stop_tx,
                handle: Some(handle),
                mic_name,
                system_name,
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => Err(anyhow!("오디오 캡처 스레드가 비정상 종료했습니다")),
        }
    }

    fn start_file(path: std::path::PathBuf, tx: Sender<RawChunk>) -> Result<Capture> {
        let samples = crate::wav::read_16k_mono(&path)?;
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("wav").to_string();
        let handle = std::thread::Builder::new().name("bd-testwav".into()).spawn(move || {
            let t0 = std::time::Instant::now();
            for (i, chunk) in samples.chunks(160).enumerate() {
                if stop_rx.try_recv().is_ok() {
                    return;
                }
                let _ = tx.send(RawChunk {
                    source: SRC_MIC,
                    rate: crate::SAMPLE_RATE,
                    samples: chunk.to_vec(),
                });
                let due = std::time::Duration::from_millis(10 * (i as u64 + 1));
                if let Some(wait) = due.checked_sub(t0.elapsed()) {
                    std::thread::sleep(wait);
                }
            }
            let _ = stop_rx.recv();
        })?;
        Ok(Capture {
            stop_tx,
            handle: Some(handle),
            mic_name: Some(format!("테스트 파일: {name}")),
            system_name: None,
        })
    }

    pub fn stop(mut self) {
        let _ = self.stop_tx.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop_tx.try_send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn capture_thread(
    cfg: CaptureConfig,
    tx: Sender<RawChunk>,
    stop_rx: Receiver<()>,
    ready_tx: Sender<Result<(Option<String>, Option<String>)>>,
) {
    let host = cpal::default_host();
    let mut streams = Vec::new();
    let mut names = (None, None);

    let result: Result<()> = (|| {
        if cfg.source.uses_mic() {
            let device = match cfg.mic_device.as_deref().filter(|s| !s.is_empty()) {
                Some(id) => {
                    let parsed = id.parse().map_err(|_| anyhow!("마이크 장치 ID 가 올바르지 않습니다"))?;
                    host.device_by_id(&parsed)
                        .or_else(|| host.default_input_device())
                }
                None => host.default_input_device(),
            }
            .ok_or_else(|| anyhow!("마이크를 찾을 수 없습니다. 마이크 연결과 Windows 개인정보 설정(마이크 접근)을 확인해 주세요."))?;
            let config = device.default_input_config().context("마이크 설정을 읽을 수 없습니다")?;
            names.0 = Some(device.to_string());
            streams.push(build(&device, config, SRC_MIC, tx.clone())?);
        }
        if cfg.source.uses_system() {
            let device = host
                .default_output_device()
                .ok_or_else(|| anyhow!("출력 장치(스피커/헤드폰)를 찾을 수 없습니다"))?;
            let config = device.default_output_config().context("출력 장치 설정을 읽을 수 없습니다")?;
            names.1 = Some(device.to_string());
            // 출력 장치에 입력 스트림을 열면 WASAPI 루프백으로 동작한다
            streams.push(build(&device, config, SRC_SYSTEM, tx.clone())?);
        }
        for s in &streams {
            s.play().context("오디오 스트림 시작 실패")?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            let _ = ready_tx.send(Ok(names));
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    }
    let _ = stop_rx.recv();
    drop(streams);
}

fn build(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    source: u8,
    tx: Sender<RawChunk>,
) -> Result<cpal::Stream> {
    let fmt = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();
    match fmt {
        SampleFormat::F32 => build_typed::<f32>(device, stream_config, source, tx),
        SampleFormat::I16 => build_typed::<i16>(device, stream_config, source, tx),
        SampleFormat::I32 => build_typed::<i32>(device, stream_config, source, tx),
        SampleFormat::U16 => build_typed::<u16>(device, stream_config, source, tx),
        SampleFormat::F64 => build_typed::<f64>(device, stream_config, source, tx),
        SampleFormat::I8 => build_typed::<i8>(device, stream_config, source, tx),
        other => Err(anyhow!("지원하지 않는 오디오 형식: {other}")),
    }
}

fn build_typed<T>(device: &cpal::Device, config: cpal::StreamConfig, source: u8, tx: Sender<RawChunk>) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;
    let rate = config.sample_rate;
    let err_fn = move |e: cpal::Error| log::warn!("오디오 스트림 오류(src={source}): {e}");
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut mono = Vec::with_capacity(data.len() / channels);
            for frame in data.chunks_exact(channels) {
                let mut acc = 0.0f32;
                for &s in frame {
                    acc += f32::from_sample_(s);
                }
                mono.push(acc / channels as f32);
            }
            let _ = tx.try_send(RawChunk {
                source,
                rate: rate_u32(rate),
                samples: mono,
            });
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

#[inline]
fn rate_u32<R: Into<u32>>(r: R) -> u32 {
    r.into()
}
