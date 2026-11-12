//! Silero VAD (v5/v6, ONNX) + 발화 분할기.
//!
//! 16 kHz 오디오를 512 샘플(32 ms) 단위로 판정하고, 말이 멈추면 즉시 한 발화를 잘라 내보낸다.
//! 발화가 너무 길어지면 최근 구간에서 음성 확률이 가장 낮은 지점(숨 쉬는 곳)을 찾아 자른다.

use anyhow::{anyhow, Result};
use ort::session::Session;
use ort::value::Tensor;
use serde::{Deserialize, Serialize};

use crate::SAMPLE_RATE;

static SILERO_MODEL: &[u8] = include_bytes!("../assets/silero_vad.onnx");

pub const CHUNK: usize = 512;
const CONTEXT: usize = 64;

pub struct SileroVad {
    session: Session,
    state: Vec<f32>,
    context: [f32; CONTEXT],
    names: [String; 3],
    out_names: [String; 2],
}

impl SileroVad {
    pub fn new() -> Result<Self> {
        let session = crate::onnx::session_from_memory(SILERO_MODEL, 1)?;
        let inputs: Vec<String> = session.inputs().iter().map(|o| o.name().to_string()).collect();
        let outputs: Vec<String> = session.outputs().iter().map(|o| o.name().to_string()).collect();
        let find = |list: &[String], want: &str, fallback: usize| -> String {
            list.iter()
                .find(|n| n.as_str() == want)
                .cloned()
                .or_else(|| list.get(fallback).cloned())
                .unwrap_or_else(|| want.to_string())
        };
        let names = [
            find(&inputs, "input", 0),
            find(&inputs, "state", 1),
            find(&inputs, "sr", 2),
        ];
        let out_names = [find(&outputs, "output", 0), find(&outputs, "stateN", 1)];
        log::debug!("silero inputs={inputs:?} outputs={outputs:?}");
        Ok(Self {
            session,
            state: vec![0.0; 2 * 128],
            context: [0.0; CONTEXT],
            names,
            out_names,
        })
    }

    pub fn reset(&mut self) {
        self.state.iter_mut().for_each(|v| *v = 0.0);
        self.context = [0.0; CONTEXT];
    }

    /// 512 샘플 한 덩어리의 음성 확률 (0..1)
    pub fn prob(&mut self, chunk: &[f32]) -> Result<f32> {
        debug_assert_eq!(chunk.len(), CHUNK);
        let mut x = Vec::with_capacity(CONTEXT + CHUNK);
        x.extend_from_slice(&self.context);
        x.extend_from_slice(chunk);
        let input = Tensor::from_array(([1usize, CONTEXT + CHUNK], x))?;
        let state = Tensor::from_array(([2usize, 1, 128], self.state.clone()))?;
        let sr = Tensor::from_array(((), vec![SAMPLE_RATE as i64]))?;
        let outputs = self.session.run(ort::inputs![
            self.names[0].as_str() => input,
            self.names[1].as_str() => state,
            self.names[2].as_str() => sr,
        ])?;
        let (_, p) = outputs[self.out_names[0].as_str()].try_extract_tensor::<f32>()?;
        let p = *p.first().ok_or_else(|| anyhow!("VAD 출력이 비어 있음"))?;
        let (_, st) = outputs[self.out_names[1].as_str()].try_extract_tensor::<f32>()?;
        if st.len() == self.state.len() {
            self.state.copy_from_slice(st);
        }
        self.context.copy_from_slice(&chunk[CHUNK - CONTEXT..]);
        Ok(p)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SegmenterConfig {
    /// 이 확률 이상이면 말하기 시작
    pub threshold: f32,
    /// 이 확률 미만이 이어지면 침묵으로 본다
    pub neg_threshold: f32,
    /// 이만큼 조용하면 발화를 끝낸다
    pub min_silence_ms: u32,
    /// 발화 앞뒤 여유
    pub pad_ms: u32,
    /// 이보다 짧은 발화(기침·클릭)는 버린다
    pub min_speech_ms: u32,
    /// 한 발화의 최대 길이 (넘으면 숨 쉬는 지점에서 자름)
    pub max_segment_ms: u32,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            neg_threshold: 0.35,
            min_silence_ms: 500,
            pad_ms: 200,
            min_speech_ms: 300,
            max_segment_ms: 15_000,
        }
    }
}

/// VAD 로 잘라낸 발화 한 덩어리 (16 kHz 모노)
#[derive(Debug, Clone)]
pub struct SpeechSegment {
    /// 세션 시작 기준 샘플 위치
    pub start: u64,
    pub end: u64,
    pub samples: Vec<f32>,
    /// 32 ms 단위 음성 확률 (화자 전환 지점 보정용)
    pub probs: Vec<f32>,
}

impl SpeechSegment {
    pub fn duration_ms(&self) -> u64 {
        crate::samples_to_ms(self.samples.len() as u64)
    }
}

pub struct Segmenter {
    vad: SileroVad,
    cfg: SegmenterConfig,
    pending: Vec<f32>,
    pre_roll: std::collections::VecDeque<f32>,
    in_speech: bool,
    seg: Vec<f32>,
    seg_probs: Vec<f32>,
    seg_start: u64,
    silence_run: usize,
    total: u64,
    last_prob: f32,
}

fn ms_to_samples(ms: u32) -> usize {
    ms as usize * SAMPLE_RATE as usize / 1000
}

impl Segmenter {
    pub fn new(cfg: SegmenterConfig) -> Result<Self> {
        Ok(Self {
            vad: SileroVad::new()?,
            cfg,
            pending: Vec::with_capacity(CHUNK * 4),
            pre_roll: Default::default(),
            in_speech: false,
            seg: Vec::new(),
            seg_probs: Vec::new(),
            seg_start: 0,
            silence_run: 0,
            total: 0,
            last_prob: 0.0,
        })
    }

    pub fn is_speaking(&self) -> bool {
        self.in_speech
    }

    pub fn last_prob(&self) -> f32 {
        self.last_prob
    }

    /// 현재 진행 중인 발화 길이 (ms)
    pub fn current_ms(&self) -> u64 {
        if self.in_speech {
            crate::samples_to_ms(self.seg.len() as u64)
        } else {
            0
        }
    }

    /// 지금까지 처리한 전체 길이 (샘플)
    pub fn position(&self) -> u64 {
        self.total + self.pending.len() as u64
    }

    pub fn push(&mut self, samples: &[f32], out: &mut Vec<SpeechSegment>) -> Result<()> {
        self.pending.extend_from_slice(samples);
        let mut offset = 0;
        while self.pending.len() - offset >= CHUNK {
            let chunk: Vec<f32> = self.pending[offset..offset + CHUNK].to_vec();
            offset += CHUNK;
            self.step(&chunk, out)?;
        }
        self.pending.drain(..offset);
        Ok(())
    }

    fn step(&mut self, chunk: &[f32], out: &mut Vec<SpeechSegment>) -> Result<()> {
        let p = self.vad.prob(chunk)?;
        self.last_prob = p;
        let pad = ms_to_samples(self.cfg.pad_ms);

        if !self.in_speech {
            self.pre_roll.extend(chunk.iter().copied());
            while self.pre_roll.len() > pad + CHUNK {
                self.pre_roll.pop_front();
            }
            if p >= self.cfg.threshold {
                self.in_speech = true;
                self.seg_start = self.total + CHUNK as u64 - self.pre_roll.len() as u64;
                self.seg = self.pre_roll.drain(..).collect();
                let pre_chunks = self.seg.len().div_ceil(CHUNK);
                self.seg_probs = vec![0.0; pre_chunks.saturating_sub(1)];
                self.seg_probs.push(p);
                self.silence_run = 0;
            }
        } else {
            self.seg.extend_from_slice(chunk);
            self.seg_probs.push(p);
            if p < self.cfg.neg_threshold {
                self.silence_run += CHUNK;
            } else if p >= self.cfg.threshold {
                self.silence_run = 0;
            }

            if self.silence_run >= ms_to_samples(self.cfg.min_silence_ms) {
                // 끝의 침묵은 pad 만큼만 남긴다
                let cut = self.silence_run.saturating_sub(pad).min(self.seg.len());
                let keep = self.seg.len() - cut;
                self.seg.truncate(keep);
                let keep_chunks = keep.div_ceil(CHUNK).min(self.seg_probs.len());
                self.seg_probs.truncate(keep_chunks);
                self.finish(out);
                self.in_speech = false;
                self.pre_roll.clear();
            } else if self.seg.len() >= ms_to_samples(self.cfg.max_segment_ms) {
                self.force_split(out);
            }
        }
        self.total += CHUNK as u64;
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<SpeechSegment>) {
        let seg = std::mem::take(&mut self.seg);
        let probs = std::mem::take(&mut self.seg_probs);
        let voiced = probs.iter().filter(|&&p| p >= self.cfg.threshold).count() * CHUNK;
        if voiced >= ms_to_samples(self.cfg.min_speech_ms) {
            let start = self.seg_start;
            out.push(SpeechSegment {
                start,
                end: start + seg.len() as u64,
                samples: seg,
                probs,
            });
        }
    }

    /// 너무 긴 발화: 뒤쪽 40% 구간에서 음성 확률이 가장 낮은 지점에서 자른다
    fn force_split(&mut self, out: &mut Vec<SpeechSegment>) {
        let n = self.seg_probs.len();
        let from = n * 6 / 10;
        let mut best = n.saturating_sub(1);
        let mut best_p = f32::MAX;
        for i in from..n.saturating_sub(1) {
            if self.seg_probs[i] < best_p {
                best_p = self.seg_probs[i];
                best = i;
            }
        }
        let cut = ((best + 1) * CHUNK).min(self.seg.len());
        let rest: Vec<f32> = self.seg.split_off(cut);
        let rest_probs: Vec<f32> = self.seg_probs.split_off((best + 1).min(n));
        let next_start = self.seg_start + cut as u64;
        self.finish(out);
        self.seg = rest;
        self.seg_probs = rest_probs;
        self.seg_start = next_start;
    }

    /// 녹음 종료: 진행 중이던 발화를 내보낸다
    pub fn flush(&mut self, out: &mut Vec<SpeechSegment>) {
        if self.in_speech {
            // 남은 자투리도 포함
            let rest = std::mem::take(&mut self.pending);
            self.seg.extend_from_slice(&rest);
            self.finish(out);
            self.in_speech = false;
        }
        self.pending.clear();
        self.pre_roll.clear();
        self.vad.reset();
    }
}
