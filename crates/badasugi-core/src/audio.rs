//! 오디오 캡처: 마이크 · 시스템 소리(WASAPI 루프백) · 둘 다.
//!
//! cpal 콜백에서는 모노로 줄여 채널로 보내기만 한다. 리샘플·믹싱은 파이프라인 스레드가 맡는다.
//! 장치가 빠지거나 기본 장치가 바뀌어 스트림이 죽으면 캡처 스레드가 알아서 다시 연결한다.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};

/// 캡처 중 사용자에게 알릴 일 (장치 끊김 · 재연결)
pub type Notify = Arc<dyn Fn(String) + Send + Sync>;

const MSG_LOST: &str = "오디오 장치 연결이 끊겼어요. 다시 연결하는 중…";
/// 오류가 몰려 올 때 첫 재연결 전에 잠깐 기다린다
const RECONNECT_DEBOUNCE: Duration = Duration::from_millis(500);
const RECONNECT_RETRY: Duration = Duration::from_secs(2);

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
    /// 캡처를 시작한다. 처음 장치를 여는 문제는 여기서 바로 에러로 돌려준다.
    ///
    /// 녹음 중 장치가 끊기면 `notify` 로 알리고 2초마다 다시 연결을 시도한다 (성공하면 다시 알림).
    pub fn start(cfg: &CaptureConfig, tx: Sender<RawChunk>, notify: Notify) -> Result<Capture> {
        // 테스트·데모용: 장치 대신 WAV 파일을 실시간 속도로 흘려 넣는다 (스피커로 소리를 내지 않고 전체 경로 검증)
        if let Ok(path) = std::env::var("BADASUGI_TEST_WAV") {
            return Self::start_file(std::path::PathBuf::from(path), tx);
        }
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(Option<String>, Option<String>)>>(1);
        let cfg = cfg.clone();
        let handle = std::thread::Builder::new()
            .name("bd-capture".into())
            .spawn(move || capture_thread(cfg, tx, stop_rx, ready_tx, notify))?;
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
    notify: Notify,
) {
    // 스트림 오류 콜백 → 이 스레드 (가득 차 있으면 버림: 연달아 오는 오류는 한 번으로)
    let (err_tx, err_rx) = crossbeam_channel::bounded::<()>(1);
    let streams = match open_streams(&cfg, &tx, &err_tx) {
        Ok((streams, mic, system)) => {
            let _ = ready_tx.send(Ok((mic, system)));
            streams
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let reopen = || {
        open_streams(&cfg, &tx, &err_tx).map(|(s, mic, system)| {
            let name = [mic, system].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            (s, name)
        })
    };
    supervise(streams, reopen, &stop_rx, &err_rx, &notify, RECONNECT_DEBOUNCE, RECONNECT_RETRY);
}

/// 멈추라는 신호가 올 때까지 스트림을 붙잡고 있다가, 장치 오류가 나면 끊김을 알리고 다시 연결한다.
fn supervise<S>(
    mut streams: Vec<S>,
    mut reopen: impl FnMut() -> Result<(Vec<S>, String)>,
    stop_rx: &Receiver<()>,
    err_rx: &Receiver<()>,
    notify: &Notify,
    debounce: Duration,
    retry: Duration,
) {
    loop {
        crossbeam_channel::select! {
            // 신호가 오거나 Capture 가 사라지면 끝
            recv(stop_rx) -> _ => return,
            recv(err_rx) -> _ => {}
        }
        notify(MSG_LOST.into());
        drop(std::mem::take(&mut streams));
        let mut wait = debounce;
        loop {
            match stop_rx.recv_timeout(wait) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            // 죽은 스트림들이 남긴 오류는 털어 낸다
            while err_rx.try_recv().is_ok() {}
            match reopen() {
                Ok((s, name)) => {
                    streams = s;
                    notify(format!("다시 연결했어요: {name}"));
                    break;
                }
                Err(e) => {
                    log::info!("오디오 장치 재연결 실패: {e:#}");
                    wait = retry;
                }
            }
        }
    }
}

/// 설정대로 스트림을 열고 재생까지 시작한다 → (스트림, 마이크 이름, 시스템 장치 이름)
fn open_streams(
    cfg: &CaptureConfig,
    tx: &Sender<RawChunk>,
    err_tx: &Sender<()>,
) -> Result<(Vec<cpal::Stream>, Option<String>, Option<String>)> {
    let host = cpal::default_host();
    let mut streams = Vec::new();
    let (mut mic, mut system) = (None, None);
    if cfg.source.uses_mic() {
        let device = match cfg.mic_device.as_deref().filter(|s| !s.is_empty()) {
            // 고른 마이크가 없어졌으면 기본 마이크로
            Some(id) => {
                let parsed = id.parse().map_err(|_| anyhow!("마이크 장치 ID 가 올바르지 않습니다"))?;
                host.device_by_id(&parsed).or_else(|| host.default_input_device())
            }
            None => host.default_input_device(),
        }
        .ok_or_else(|| anyhow!("마이크를 찾을 수 없습니다. 마이크 연결과 Windows 개인정보 설정(마이크 접근)을 확인해 주세요."))?;
        let config = device.default_input_config().context("마이크 설정을 읽을 수 없습니다")?;
        mic = Some(device.to_string());
        streams.push(build(&device, config, SRC_MIC, tx.clone(), err_tx.clone())?);
    }
    if cfg.source.uses_system() {
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("출력 장치(스피커/헤드폰)를 찾을 수 없습니다"))?;
        let config = device.default_output_config().context("출력 장치 설정을 읽을 수 없습니다")?;
        system = Some(device.to_string());
        // 출력 장치에 입력 스트림을 열면 WASAPI 루프백으로 동작한다
        streams.push(build(&device, config, SRC_SYSTEM, tx.clone(), err_tx.clone())?);
    }
    for s in &streams {
        s.play().context("오디오 스트림 시작 실패")?;
    }
    Ok((streams, mic, system))
}

fn build(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    source: u8,
    tx: Sender<RawChunk>,
    err_tx: Sender<()>,
) -> Result<cpal::Stream> {
    let fmt = config.sample_format();
    let c: cpal::StreamConfig = config.into();
    match fmt {
        SampleFormat::F32 => build_typed::<f32>(device, c, source, tx, err_tx),
        SampleFormat::I16 => build_typed::<i16>(device, c, source, tx, err_tx),
        SampleFormat::I32 => build_typed::<i32>(device, c, source, tx, err_tx),
        SampleFormat::U16 => build_typed::<u16>(device, c, source, tx, err_tx),
        SampleFormat::F64 => build_typed::<f64>(device, c, source, tx, err_tx),
        SampleFormat::I8 => build_typed::<i8>(device, c, source, tx, err_tx),
        other => Err(anyhow!("지원하지 않는 오디오 형식: {other}")),
    }
}

fn build_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    source: u8,
    tx: Sender<RawChunk>,
    err_tx: Sender<()>,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;
    let rate = config.sample_rate;
    let err_fn = move |e: cpal::Error| {
        use cpal::ErrorKind as K;
        match e.kind() {
            // 순간 끊김·우선순위 거절·자동 경로 변경은 스트림이 살아 있다
            K::Xrun | K::RealtimeDenied | K::DeviceChanged => log::debug!("오디오 스트림 경고(src={source}): {e}"),
            _ => {
                log::warn!("오디오 스트림 오류(src={source}): {e}");
                let _ = err_tx.try_send(());
            }
        }
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn collect() -> (Notify, Arc<parking_lot::Mutex<Vec<String>>>) {
        let msgs = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let m = msgs.clone();
        (Arc::new(move |s: String| m.lock().push(s)), msgs)
    }

    #[test]
    fn reconnects_once_per_error_burst() {
        let (notify, msgs) = collect();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let (err_tx, err_rx) = crossbeam_channel::bounded::<()>(1);
        let errs = err_tx.clone();
        let h = std::thread::spawn(move || {
            let mut attempts = 0;
            let reopen = || {
                attempts += 1;
                if attempts < 3 {
                    Err(anyhow!("장치 없음"))
                } else {
                    Ok((vec![attempts], "테스트 마이크".to_string()))
                }
            };
            let d = Duration::from_millis(30);
            supervise(vec![0], reopen, &stop_rx, &err_rx, &notify, d, d);
            attempts
        });
        // 오류가 몰려 와도 (디바운스 중에 또 와도) 끊김 알림은 한 번
        for _ in 0..5 {
            let _ = errs.try_send(());
        }
        std::thread::sleep(Duration::from_millis(10));
        let _ = errs.try_send(());
        std::thread::sleep(Duration::from_millis(300));
        stop_tx.send(()).unwrap();
        let attempts = h.join().unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(*msgs.lock(), vec![MSG_LOST.to_string(), "다시 연결했어요: 테스트 마이크".to_string()]);
        drop(err_tx);
    }

    #[test]
    fn stop_interrupts_retry() {
        let (notify, msgs) = collect();
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let (err_tx, err_rx) = crossbeam_channel::bounded::<()>(1);
        err_tx.send(()).unwrap();
        let h = std::thread::spawn(move || {
            let reopen = || -> Result<(Vec<u8>, String)> { Err(anyhow!("장치 없음")) };
            supervise(vec![0u8], reopen, &stop_rx, &err_rx, &notify, Duration::from_millis(10), Duration::from_secs(60));
        });
        std::thread::sleep(Duration::from_millis(100));
        let t = Instant::now();
        drop(stop_tx); // Capture 가 사라져도 멈춘다
        h.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(1));
        assert_eq!(*msgs.lock(), vec![MSG_LOST.to_string()]);
    }
}
