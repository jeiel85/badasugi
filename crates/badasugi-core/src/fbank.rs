//! Kaldi 호환 80차 log-mel filterbank (화자 임베딩 모델 입력용).
//!
//! 25 ms 프레임 / 10 ms 이동, DC 제거, 프리엠퍼시스 0.97, povey 윈도, 512 FFT, 20 Hz~8 kHz mel 80 밴드.
//! 출력은 발화 단위 평균을 뺀(CMN) 값이다. CMN 덕분에 입력 스케일(±1 또는 ±32768)은 결과에 영향이 없다.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

pub const NUM_BINS: usize = 80;
const FRAME_LEN: usize = 400;
const FRAME_SHIFT: usize = 160;
const NFFT: usize = 512;
const PREEMPH: f32 = 0.97;

pub struct Fbank {
    window: Vec<f32>,
    /// (시작 FFT bin, 가중치들)
    banks: Vec<(usize, Vec<f32>)>,
    fft: Arc<dyn Fft<f32>>,
}

fn mel(f: f64) -> f64 {
    1127.0 * (1.0 + f / 700.0).ln()
}

impl Default for Fbank {
    fn default() -> Self {
        Self::new(16_000)
    }
}

impl Fbank {
    pub fn new(sample_rate: u32) -> Self {
        let window = (0..FRAME_LEN)
            .map(|i| {
                let a = 2.0 * std::f64::consts::PI * i as f64 / (FRAME_LEN - 1) as f64;
                (0.5 - 0.5 * a.cos()).powf(0.85) as f32
            })
            .collect();

        let nyquist = sample_rate as f64 / 2.0;
        let (lo, hi) = (mel(20.0), mel(nyquist));
        let delta = (hi - lo) / (NUM_BINS + 1) as f64;
        let fft_bins = NFFT / 2;
        let bin_hz = sample_rate as f64 / NFFT as f64;
        let banks = (0..NUM_BINS)
            .map(|m| {
                let left = lo + m as f64 * delta;
                let center = left + delta;
                let right = center + delta;
                let mut start = usize::MAX;
                let mut weights = Vec::new();
                for i in 0..fft_bins {
                    let mf = mel(i as f64 * bin_hz);
                    if mf > left && mf < right {
                        let w = if mf <= center {
                            (mf - left) / (center - left)
                        } else {
                            (right - mf) / (right - center)
                        };
                        if start == usize::MAX {
                            start = i;
                        }
                        // 연속 구간이므로 순서대로 채운다
                        weights.resize(i - start, 0.0);
                        weights.push(w as f32);
                    }
                }
                (if start == usize::MAX { 0 } else { start }, weights)
            })
            .collect();

        let fft = FftPlanner::<f32>::new().plan_fft_forward(NFFT);
        Self { window, banks, fft }
    }

    pub fn num_frames(num_samples: usize) -> usize {
        if num_samples < FRAME_LEN {
            0
        } else {
            1 + (num_samples - FRAME_LEN) / FRAME_SHIFT
        }
    }

    /// 프레임 × 80 (행 우선) 특징. CMN 적용.
    pub fn compute(&self, samples: &[f32]) -> (Vec<f32>, usize) {
        let frames = Self::num_frames(samples.len());
        let mut feats = vec![0.0f32; frames * NUM_BINS];
        let mut buf = vec![Complex32::new(0.0, 0.0); NFFT];
        let mut frame = [0.0f32; FRAME_LEN];
        let mut scratch = vec![Complex32::new(0.0, 0.0); self.fft.get_inplace_scratch_len()];
        let mut power = [0.0f32; NFFT / 2 + 1];

        for t in 0..frames {
            let off = t * FRAME_SHIFT;
            for (d, s) in frame.iter_mut().zip(&samples[off..off + FRAME_LEN]) {
                *d = s * 32768.0;
            }
            let mean = frame.iter().sum::<f32>() / FRAME_LEN as f32;
            frame.iter_mut().for_each(|v| *v -= mean);
            for i in (1..FRAME_LEN).rev() {
                frame[i] -= PREEMPH * frame[i - 1];
            }
            frame[0] -= PREEMPH * frame[0];
            for (i, c) in buf.iter_mut().enumerate() {
                *c = if i < FRAME_LEN {
                    Complex32::new(frame[i] * self.window[i], 0.0)
                } else {
                    Complex32::new(0.0, 0.0)
                };
            }
            self.fft.process_with_scratch(&mut buf, &mut scratch);
            for (p, c) in power.iter_mut().zip(buf.iter()) {
                *p = c.norm_sqr();
            }
            let row = &mut feats[t * NUM_BINS..(t + 1) * NUM_BINS];
            for (m, (start, w)) in self.banks.iter().enumerate() {
                let e: f32 = w.iter().zip(&power[*start..]).map(|(a, b)| a * b).sum();
                row[m] = e.max(f32::EPSILON).ln();
            }
        }

        if frames > 0 {
            for m in 0..NUM_BINS {
                let mean = (0..frames).map(|t| feats[t * NUM_BINS + m]).sum::<f32>() / frames as f32;
                for t in 0..frames {
                    feats[t * NUM_BINS + m] -= mean;
                }
            }
        }
        (feats, frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_and_cmn() {
        let fb = Fbank::default();
        let x: Vec<f32> = (0..16_000).map(|i| ((i as f32) * 0.05).sin() * 0.3).collect();
        let (f, n) = fb.compute(&x);
        assert_eq!(n, 98);
        assert_eq!(f.len(), 98 * NUM_BINS);
        let mean0: f32 = (0..n).map(|t| f[t * NUM_BINS]).sum::<f32>() / n as f32;
        assert!(mean0.abs() < 1e-3);
        assert!(f.iter().all(|v| v.is_finite()));
    }
}
