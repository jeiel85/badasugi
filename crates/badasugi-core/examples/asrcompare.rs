//! 녹음이 끝난 회의 기록의 문장 구간을 API 음성 인식에 다시 보내, 앞 문맥(바로 앞에서 받아 적은 말)을
//! 넘길 때와 안 넘길 때를 나란히 본다 — 문장부호 · 띄어쓰기 · 표기가 한결같은지, 앞 문맥을 그대로 베끼지는 않는지.
//!
//! ```text
//! cargo run --release -p badasugi-core --no-default-features --example asrcompare -- <기록.json> <녹음.opus> [옵션]
//!   --base <url>          OpenAI 호환 주소 (기본 https://api.groq.com/openai/v1)
//!   --model <이름>         (기본 whisper-large-v3)
//!   --from <문장 id>       여기부터 (기본 0)
//!   --count <n>            문장 수 (기본 40)
//!   --glossary <a,b,c>     용어집
//!   --only <plain|context> 한쪽만
//!   --out <out.json>       문장마다 {id, saved, plain, context, prompt}
//!   --context-chars <n>    앞 문맥 최대 글자 수 (기본 asr::CONTEXT_CHARS)
//!   --local <ggml.bin>     API 대신 로컬 whisper.cpp (기본 기능 local-whisper 로 빌드)
//!   --prompt <텍스트>       용어집 문장 대신 이 프롬프트 (문체 견본 시험용)
//! ```
//! API 키는 `BADASUGI_API_KEY` 환경 변수. 문장은 녹음 중과 같은 순서로 하나씩 보낸다 (앞 문맥은 앞 문장 결과).

use std::path::PathBuf;
use std::time::Duration;

use badasugi_core::asr::{self, AsrRequest, EngineConfig, RecentText};
use badasugi_core::decode::AudioFile;
use badasugi_core::SAMPLE_RATE;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Utt {
    id: u64,
    start_ms: u64,
    end_ms: u64,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct Doc {
    utterances: Vec<Utt>,
}

#[derive(Serialize, Default)]
struct Row {
    id: u64,
    saved: String,
    plain: Option<String>,
    context: Option<String>,
    prompt: String,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let doc: Doc = serde_json::from_str(&std::fs::read_to_string(&a[0])?)?;
    let audio = PathBuf::from(&a[1]);
    let (mut base, mut model) = ("https://api.groq.com/openai/v1".to_string(), "whisper-large-v3".to_string());
    let (mut from, mut count, mut glossary, mut only, mut out) = (0u64, 40usize, String::new(), None, None);
    let (mut context_chars, mut local, mut raw_prompt) = (asr::CONTEXT_CHARS, None::<PathBuf>, None::<String>);
    let mut i = 2;
    while i < a.len() {
        let v = a.get(i + 1).cloned().unwrap_or_default();
        match a[i].as_str() {
            "--base" => base = v,
            "--model" => model = v,
            "--from" => from = v.parse()?,
            "--count" => count = v.parse()?,
            "--glossary" => glossary = v,
            "--only" => only = Some(v),
            "--out" => out = Some(v),
            "--context-chars" => context_chars = v.parse()?,
            "--local" => local = Some(v.into()),
            "--prompt" => raw_prompt = Some(v),
            other => anyhow::bail!("알 수 없는 옵션: {other}"),
        }
        i += 2;
    }
    let mut utts: Vec<Utt> = doc.utterances.into_iter().filter(|u| u.id >= from).collect();
    utts.sort_by_key(|u| u.id);
    utts.truncate(count);
    let Some(last) = utts.last() else {
        anyhow::bail!("문장이 없어요");
    };

    // 마지막 문장 끝까지만 풀어 둔다
    let need = (last.end_ms * SAMPLE_RATE as u64 / 1000) as usize;
    let mut pcm = Vec::with_capacity(need);
    let mut f = AudioFile::open(&audio)?;
    while pcm.len() < need && f.read(&mut pcm)? {}

    let terms: Vec<String> = glossary.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect();
    let prompt = match raw_prompt {
        Some(p) => p,
        None if terms.is_empty() => String::new(),
        None => format!("회의 녹취록입니다. 주요 용어: {}.", terms.join(", ")),
    };
    let engine = match local {
        Some(model_path) => EngineConfig::Local {
            model_path,
            threads: 0,
            beam_size: 1,
        },
        None => EngineConfig::Api {
            base_url: base,
            api_key: std::env::var("BADASUGI_API_KEY").unwrap_or_default(),
            model,
        },
    };
    println!(
        "{} · 문장 {}개 (id {}~{}) · 앞 문맥 {context_chars}자",
        engine.label(),
        utts.len(),
        utts[0].id,
        last.id
    );
    let mut t = asr::create(&engine)?;

    let mut rows: Vec<Row> = Vec::new();
    let mut recent = RecentText::with_max_chars(context_chars);
    for (n, u) in utts.iter().enumerate() {
        let s = ((u.start_ms * SAMPLE_RATE as u64 / 1000) as usize).min(pcm.len());
        let e = ((u.end_ms * SAMPLE_RATE as u64 / 1000) as usize).min(pcm.len());
        let samples = &pcm[s..e];
        let mut row = Row {
            id: u.id,
            saved: u.text.clone(),
            ..Default::default()
        };
        if only.as_deref() != Some("context") {
            row.plain = Some(recognize(t.as_mut(), samples, &prompt, &terms, "")?);
        }
        if only.as_deref() != Some("plain") {
            let context = recent.before(u.id);
            let text = recognize(t.as_mut(), samples, &prompt, &terms, &context)?;
            recent.record(u.id, &text);
            row.prompt = context;
            row.context = Some(text);
        }
        println!("#{:<4} 저장됨 | {}", u.id, row.saved);
        if let Some(p) = &row.plain {
            println!("      문맥 없음 | {p}");
        }
        if let Some(c) = &row.context {
            println!("      문맥 있음 | {c}");
        }
        eprintln!("[{}/{}]", n + 1, utts.len());
        rows.push(row);
    }

    let stat = |name: &str, texts: Vec<&str>| {
        let texts: Vec<&str> = texts.into_iter().filter(|t| !t.trim().is_empty()).collect();
        let n = texts.len().max(1);
        let ends = texts.iter().filter(|t| t.trim_end().ends_with(['.', '?', '!', '…'])).count();
        let commas = texts.iter().filter(|t| t.contains(',')).count();
        let thanks = texts.iter().filter(|t| t.trim().trim_start_matches("네,").trim().trim_end_matches('.') == "감사합니다").count();
        println!(
            "{name:<8} 문장 {:>3} | 문장부호로 끝남 {:>3} ({:>3}%) | 쉼표 {:>3} | '감사합니다'만 {:>2}",
            texts.len(),
            ends,
            ends * 100 / n,
            commas,
            thanks
        );
    };
    println!("\n── 요약 ──");
    stat("저장됨", rows.iter().map(|r| r.saved.as_str()).collect());
    if only.as_deref() != Some("context") {
        stat("문맥 없음", rows.iter().filter_map(|r| r.plain.as_deref()).collect());
    }
    if only.as_deref() != Some("plain") {
        stat("문맥 있음", rows.iter().filter_map(|r| r.context.as_deref()).collect());
        // 앞 문맥을 그대로 베꼈는지 (Whisper 는 소리가 없으면 프롬프트를 되풀이하기도 한다)
        let squash = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>();
        let echoes: Vec<u64> = rows
            .iter()
            .filter(|r| {
                let c = squash(r.context.as_deref().unwrap_or(""));
                c.chars().count() >= 6 && squash(&r.prompt).contains(&c)
            })
            .map(|r| r.id)
            .collect();
        println!("앞 문맥을 그대로 베낀 문장: {} {:?}", echoes.len(), echoes);
    }
    if let Some(p) = out {
        std::fs::write(&p, serde_json::to_string_pretty(&rows)?)?;
        println!("저장: {p}");
    }
    Ok(())
}

/// 연결 문제 · 요청 한도면 기다렸다 다시 (무료 등급 분당 한도를 넘지 않게)
fn recognize(t: &mut dyn asr::Transcriber, samples: &[f32], prompt: &str, keywords: &[String], context: &str) -> anyhow::Result<String> {
    let req = AsrRequest {
        samples,
        language: "ko",
        prompt,
        keywords,
        context,
        preview: None,
    };
    for attempt in 0..6 {
        match t.transcribe(&req) {
            Ok(text) => return Ok(badasugi_core::text::clean(&text)),
            Err(e) if asr::retryable(&e) && attempt < 5 => {
                eprintln!("  다시 보냄 ({e})");
                std::thread::sleep(Duration::from_secs(15));
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}
