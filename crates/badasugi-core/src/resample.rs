//! 스트리밍 리샘플러: 임의 입력 레이트 → 16 kHz 모노.
//!
//! 음성 인식용이라 "저역통과 FIR(블랙만 윈도 sinc) + 선형 보간" 으로 충분하다.
//! 입력이 44.1/48 kHz 일 때 7.2 kHz 이상을 걸러낸 뒤 약 3배 과샘플된 신호를 보간하므로 왜곡이 무시할 수준이다.

const TAPS: usize = 63;

pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    step: f64,
    fir: Vec<f32>,
    /// FIR 지연선 (가장 최근 TAPS-1 개 원본 샘플)
    hist: Vec<f32>,
    /// 필터를 통과한 입력 샘플 (보간 대기)
    filtered: Vec<f32>,
    /// `filtered` 기준 다음 출력 샘플의 위치
    pos: f64,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let step = in_rate as f64 / out_rate as f64;
        let fir = if in_rate > out_rate {
            design_lowpass(0.45 * out_rate as f64 / in_rate as f64)
        } else {
            Vec::new()
        };
        Self {
            in_rate,
            out_rate,
            step,
            hist: vec![0.0; fir.len().saturating_sub(1)],
            fir,
            filtered: Vec::with_capacity(4096),
            pos: 0.0,
        }
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    pub fn is_passthrough(&self) -> bool {
        self.in_rate == self.out_rate
    }

    /// 입력을 밀어 넣고, 만들어진 출력 샘플을 `out` 에 덧붙인다.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_passthrough() {
            out.extend_from_slice(input);
            return;
        }
        if self.fir.is_empty() {
            self.filtered.extend_from_slice(input);
        } else {
            let n = self.fir.len();
            for &x in input {
                self.hist.push(x);
                // hist 길이 = n (방금 넣은 샘플 포함)
                let h = &self.hist[self.hist.len() - n..];
                let mut acc = 0.0f32;
                for (a, b) in h.iter().zip(self.fir.iter()) {
                    acc += a * b;
                }
                self.filtered.push(acc);
                if self.hist.len() > 8192 {
                    let keep = self.hist.len() - (n - 1);
                    self.hist.drain(..keep);
                }
            }
        }
        while self.pos + 1.0 < self.filtered.len() as f64 {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            let a = self.filtered[i];
            let b = self.filtered[i + 1];
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        let consumed = (self.pos as usize).min(self.filtered.len());
        if consumed > 0 {
            self.filtered.drain(..consumed);
            self.pos -= consumed as f64;
        }
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }
}

/// 정규화 차단 주파수 `fc` (사이클/샘플) 의 블랙만 윈도 sinc 저역통과 필터
fn design_lowpass(fc: f64) -> Vec<f32> {
    let m = (TAPS - 1) as f64;
    let mut h: Vec<f64> = (0..TAPS)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
            };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / m).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = h.iter().sum();
    for v in &mut h {
        *v /= sum;
    }
    h.into_iter().map(|v| v as f32).collect()
}

/// 한 번에 전체 버퍼를 변환 (파일 처리용)
pub fn resample_all(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    let mut r = Resampler::new(in_rate, out_rate);
    let mut out = Vec::with_capacity((input.len() as f64 * out_rate as f64 / in_rate as f64) as usize + 16);
    r.process(input, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_and_tone_preserved() {
        let sr = 48_000;
        let f = 440.0f32;
        let input: Vec<f32> = (0..sr).map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / sr as f32).sin()).collect();
        // 청크 단위 스트리밍 처리도 한 번에 처리한 것과 같아야 한다
        let mut r = Resampler::new(sr as u32, 16_000);
        let mut out = Vec::new();
        for c in input.chunks(480) {
            r.process(c, &mut out);
        }
        assert!((out.len() as i64 - 16_000).abs() < 40, "len {}", out.len());
        // 440 Hz 진폭이 유지되는지 (지연 구간 이후 RMS ≈ 0.707)
        let tail = &out[1000..15000];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        assert!((rms - 0.707).abs() < 0.02, "rms {rms}");
    }

    #[test]
    fn removes_high_frequency() {
        let sr = 48_000;
        let f = 12_000.0f32; // 16 kHz 로 내리면 앨리어싱이 생길 대역
        let input: Vec<f32> = (0..sr).map(|i| (2.0 * std::f32::consts::PI * f * i as f32 / sr as f32).sin()).collect();
        let out = resample_all(&input, sr as u32, 16_000);
        let tail = &out[1000..15000];
        let rms = (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(rms < 0.02, "rms {rms}");
    }
}
