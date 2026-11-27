//! 녹음 뒤 다시 받아 적기 — 저장된 녹음에서 문장 구간을 그대로 잘라 다른(더 정확한) 모델로 다시 받아 적는다.
//!
//! 녹음 중에는 빠른 모델로 바로바로 받아 적고, 녹음이 끝난 뒤 느리지만 더 정확한 모델로 같은 구간을 다시 받아 적어
//! 글만 바꾸는 데 쓴다. 문장 나눔 · 시각 · 화자는 그대로다. 녹음 파일은 처음부터 한 번만 차례로 읽고
//! (1시간 녹음을 통째로 메모리에 올리지 않게) 지난 구간은 버린다.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;

use crate::asr::{AsrRequest, Transcriber};
use crate::decode::AudioFile;
use crate::SAMPLE_RATE;

/// 다시 받아 적을 문장 구간 (녹음 시작부터 ms)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub id: u64,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// 다음 문장으로 넘어가기 전에 묻는다
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// 이어서 한다
    Go,
    /// 잠시 쉰다 (새 녹음 중 등) — 쉬는 동안 모델을 내려 메모리를 비운다
    Wait,
    /// 그만둔다 (받아 적은 데까지는 이미 `on_text` 로 넘겼다)
    Stop,
}

/// 인식 요청에 같이 보내는 것 (녹음 때와 같게)
pub struct Request<'a> {
    pub language: &'a str,
    /// 용어집 프롬프트
    pub prompt: &'a str,
    pub keywords: &'a [String],
}

/// 쉬는 동안 다시 물어보는 간격
const WAIT_STEP: Duration = Duration::from_millis(500);

/// 구간마다 다시 받아 적어 `on_text(구간, 글)` 로 넘긴다. 글이 `None` 이면 이 모델이 알아듣지 못했거나
/// 엉뚱한 말(반복 · 다른 언어)을 내놓은 것 — 원래 글을 그대로 두라는 뜻이다.
/// 구간은 시작 순서로 다룬다. 끝까지 했으면 `Ok(true)`, `Hold::Stop` 으로 그만뒀으면 `Ok(false)`.
/// `make` 는 인식기를 만든다 (처음 · 쉬었다 다시 시작할 때).
/// `hold` 는 구간 사이와 인식하는 중에도(로컬 엔진) 묻는다 — 인식 중에 `Go` 가 아니면 하던 구간을 그만두고
/// (녹음 · 말로 입력에 CPU · GPU 를 바로 내주려고) 쉬었다가 그 구간부터 다시 한다.
pub fn run(
    audio: &Path,
    spans: &[Span],
    req: &Request,
    make: &mut dyn FnMut() -> Result<Box<dyn Transcriber>>,
    hold: &(dyn Fn() -> Hold + Sync),
    on_text: &mut dyn FnMut(Span, Option<String>),
) -> Result<bool> {
    let mut spans = spans.to_vec();
    spans.sort_by_key(|s| (s.start_ms, s.id));
    let mut file = AudioFile::open(audio)?;
    // pcm[0] 이 녹음의 몇 번째 샘플인지
    let mut base = 0usize;
    let mut pcm: Vec<f32> = Vec::new();
    let mut eof = false;
    let mut engine: Option<Box<dyn Transcriber>> = None;
    // 인식하는 중에 그만두라고 했는지 (그만둔 오류와 진짜 오류를 가른다)
    let interrupted = AtomicBool::new(false);
    let abort = || {
        let stop = hold() != Hold::Go;
        if stop {
            interrupted.store(true, Ordering::SeqCst);
        }
        stop
    };
    for (i, span) in spans.iter().enumerate() {
        let start = ms_to_sample(span.start_ms);
        let end = ms_to_sample(span.end_ms).max(start);
        let text = loop {
            loop {
                match hold() {
                    Hold::Go => break,
                    Hold::Stop => return Ok(false),
                    Hold::Wait => {
                        engine = None;
                        std::thread::sleep(WAIT_STEP);
                    }
                }
            }
            while !eof && base + pcm.len() < end {
                eof = !file.read(&mut pcm)?;
                // 이 구간 앞의 소리는 다시 쓰지 않으므로 읽는 대로 버린다 — 긴 녹음 뒤쪽부터 이어서 하거나
                // 긴 쉼이 있어도 메모리가 녹음 길이만큼 늘지 않게
                let drop = start.saturating_sub(base).min(pcm.len());
                if drop > 0 {
                    pcm.drain(..drop);
                    base += drop;
                }
            }
            let from = start.saturating_sub(base).min(pcm.len());
            let to = end.saturating_sub(base).min(pcm.len());
            if to <= from {
                // 녹음 파일이 문장보다 짧다 (녹음이 끝나기 전에 앱이 꺼짐 등)
                break None;
            }
            if engine.is_none() {
                engine = Some(make()?);
            }
            interrupted.store(false, Ordering::SeqCst);
            let r = engine.as_mut().expect("방금 만듦").transcribe(&AsrRequest {
                samples: &pcm[from..to],
                language: req.language,
                prompt: req.prompt,
                keywords: req.keywords,
                context: "",
                preview: None,
                abort: Some(&abort),
            });
            match r {
                Ok(raw) => break accept(&raw, req.language),
                // 그만뒀다 → 쉬었다가 이 구간부터 다시
                Err(_) if interrupted.load(Ordering::SeqCst) => continue,
                Err(e) => return Err(e),
            }
        };
        on_text(*span, text);
        // 다음 구간보다 앞선 소리는 더 쓰지 않는다
        let keep_from = spans.get(i + 1).map_or(base + pcm.len(), |n| ms_to_sample(n.start_ms));
        let drop = keep_from.saturating_sub(base).min(pcm.len());
        if drop > 0 {
            pcm.drain(..drop);
            base += drop;
        }
    }
    Ok(true)
}

/// 다시 받아 적은 글로 바꿔도 되는지 — 비었거나 같은 말 되풀이 · 다른 언어면 원래 글을 둔다
fn accept(raw: &str, language: &str) -> Option<String> {
    let text = crate::text::clean(raw);
    if text.is_empty() || crate::text::is_degenerate(&text) || crate::text::is_wrong_script(&text, language) {
        return None;
    }
    Some(text)
}

fn ms_to_sample(ms: u64) -> usize {
    (ms * SAMPLE_RATE as u64 / 1000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::atomic::AtomicU32;

    /// 받은 소리의 크기(첫 샘플)로 글을 정하는 가짜 인식기 — 구간마다 다른 크기로 채운 녹음에서
    /// 어느 구간을 받았는지 알 수 있다
    struct Fake;

    impl Transcriber for Fake {
        fn transcribe(&mut self, req: &AsrRequest) -> Result<String> {
            let level = req.samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
            let n = (level * 10.0).round() as u32;
            Ok(match n {
                // 한국어 회의에서 엉뚱한 영어 (줄인 창에서 가끔 생긴다)
                9 => "twenty minutes, OK.".into(),
                8 => String::new(),
                _ => format!("구간 {n} 입니다."),
            })
        }
    }

    /// 1초마다 크기가 0.1, 0.2, … 인 소리 (n 초)
    fn write_steps(path: &Path, levels: &[f32]) {
        let mut w = crate::wav::WavRecorder::create(path).unwrap();
        for &l in levels {
            w.write(&vec![l; SAMPLE_RATE as usize]);
        }
        w.finish().unwrap();
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bd-redo-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("a.wav")
    }

    fn sp(id: u64, s: u64, e: u64) -> Span {
        Span { id, start_ms: s, end_ms: e }
    }

    const REQ: Request = Request { language: "ko", prompt: "", keywords: &[] };

    #[test]
    fn each_span_gets_its_own_audio_in_start_order() {
        let path = temp("order");
        write_steps(&path, &[0.1, 0.2, 0.3, 0.4, 0.5]);
        let made = Rc::new(Cell::new(0));
        let m = made.clone();
        let mut got = Vec::new();
        // 시작 순서가 뒤섞여 와도 차례로 (녹음 파일은 한 번만 읽는다)
        let spans = [sp(3, 3100, 3900), sp(1, 1100, 1900), sp(4, 4100, 4900), sp(0, 100, 900)];
        let done = run(
            &path,
            &spans,
            &REQ,
            &mut || {
                m.set(m.get() + 1);
                Ok(Box::new(Fake) as Box<dyn Transcriber>)
            },
            &|| Hold::Go,
            &mut |s, t| got.push((s.id, t)),
        )
        .unwrap();
        assert!(done);
        assert_eq!(
            got,
            vec![
                (0, Some("구간 1 입니다.".into())),
                (1, Some("구간 2 입니다.".into())),
                (3, Some("구간 4 입니다.".into())),
                (4, Some("구간 5 입니다.".into())),
            ]
        );
        assert_eq!(made.get(), 1, "인식기는 한 번만 만든다");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn resuming_late_in_a_recording_gets_the_right_audio() {
        // 앞 구간은 이미 했다 (이어서 하기) — 앞의 소리는 읽는 대로 버리고 뒤 구간만
        let path = temp("resume");
        write_steps(&path, &[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
        let mut got = Vec::new();
        run(
            &path,
            &[sp(5, 5100, 5900), sp(3, 3100, 3900)],
            &REQ,
            &mut || Ok(Box::new(Fake) as Box<dyn Transcriber>),
            &|| Hold::Go,
            &mut |s, t| got.push((s.id, t)),
        )
        .unwrap();
        assert_eq!(got, vec![(3, Some("구간 4 입니다.".into())), (5, Some("구간 6 입니다.".into()))]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn bad_or_empty_results_keep_the_original_text() {
        let path = temp("bad");
        write_steps(&path, &[0.9, 0.8, 0.3]);
        let mut got = Vec::new();
        // 녹음보다 뒤에 있는 구간(녹음이 덜 저장됨)도 원래 글을 둔다
        let spans = [sp(0, 100, 900), sp(1, 1100, 1900), sp(2, 2100, 2900), sp(3, 5000, 6000)];
        run(
            &path,
            &spans,
            &REQ,
            &mut || Ok(Box::new(Fake) as Box<dyn Transcriber>),
            &|| Hold::Go,
            &mut |s, t| got.push((s.id, t)),
        )
        .unwrap();
        assert_eq!(got, vec![(0, None), (1, None), (2, Some("구간 3 입니다.".into())), (3, None)]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn waiting_drops_the_model_and_stop_ends_early() {
        let path = temp("hold");
        write_steps(&path, &[0.1, 0.2, 0.3, 0.4]);
        let made = Rc::new(Cell::new(0));
        let m = made.clone();
        let asked = AtomicU32::new(0);
        let mut got = Vec::new();
        let spans = [sp(0, 100, 900), sp(1, 1100, 1900), sp(2, 2100, 2900), sp(3, 3100, 3900)];
        // 두 번째 구간 앞에서 한 번 쉬고, 네 번째 구간 앞에서 그만둔다
        let done = run(
            &path,
            &spans,
            &REQ,
            &mut || {
                m.set(m.get() + 1);
                Ok(Box::new(Fake) as Box<dyn Transcriber>)
            },
            &|| match asked.fetch_add(1, Ordering::SeqCst) + 1 {
                2 => Hold::Wait,
                5 => Hold::Stop,
                _ => Hold::Go,
            },
            &mut |s, t| got.push((s.id, t)),
        )
        .unwrap();
        assert!(!done);
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(made.get(), 2, "쉬고 나면 인식기를 다시 만든다");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 인식하는 중에 그만두라는지 묻는 가짜 인식기 (로컬 엔진처럼) — 몇 번 받아 적으려 했는지 센다
    struct Interruptible(std::sync::Arc<AtomicU32>);

    impl Transcriber for Interruptible {
        fn transcribe(&mut self, req: &AsrRequest) -> Result<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            if req.abort.is_some_and(|a| a()) {
                anyhow::bail!("그만둠");
            }
            Fake.transcribe(req)
        }
    }

    #[test]
    fn a_span_stopped_midway_is_done_again_after_the_wait() {
        let path = temp("abort");
        write_steps(&path, &[0.1, 0.2]);
        let tries = std::sync::Arc::new(AtomicU32::new(0));
        let t = tries.clone();
        let asked = AtomicU32::new(0);
        let mut got = Vec::new();
        // 첫 구간을 받아 적는 중에(두 번째 물음) 녹음이 시작됐다 → 그만두고, 쉬었다가 그 구간부터 다시
        let done = run(
            &path,
            &[sp(0, 100, 900), sp(1, 1100, 1900)],
            &REQ,
            &mut || Ok(Box::new(Interruptible(t.clone())) as Box<dyn Transcriber>),
            &|| match asked.fetch_add(1, Ordering::SeqCst) + 1 {
                2 | 3 => Hold::Wait,
                _ => Hold::Go,
            },
            &mut |s, t| got.push((s.id, t)),
        )
        .unwrap();
        assert!(done);
        assert_eq!(got, vec![(0, Some("구간 1 입니다.".into())), (1, Some("구간 2 입니다.".into()))]);
        assert_eq!(tries.load(Ordering::SeqCst), 3, "그만둔 구간은 한 번 더 받아 적는다");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_real_error_is_not_mistaken_for_a_stop() {
        struct Broken;
        impl Transcriber for Broken {
            fn transcribe(&mut self, _: &AsrRequest) -> Result<String> {
                anyhow::bail!("모델이 망가짐")
            }
        }
        let path = temp("err");
        write_steps(&path, &[0.1]);
        let r = run(
            &path,
            &[sp(0, 100, 900)],
            &REQ,
            &mut || Ok(Box::new(Broken) as Box<dyn Transcriber>),
            &|| Hold::Go,
            &mut |_, _| {},
        );
        assert!(r.unwrap_err().to_string().contains("망가짐"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
