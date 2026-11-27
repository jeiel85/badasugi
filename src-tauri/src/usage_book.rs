//! 이 앱으로 쓴 API 사용량 — 제공자별 · 날짜별로 세어 `usage.json` 에 둔다 (설정 › 사용량)
//!
//! - 음성 인식: API 로 보낸 녹음 길이 · 요청 수 · 실패 수
//! - 요약 · 묻기: 요청 수 · 입력/출력 토큰 (응답에 담겨 온 값). 입력 합계에는 Claude 프롬프트 캐시로 읽고 쓴 토큰도 넣는다
//! - 모델별로도 따로 세어 두고, 볼 때 단가표(`pricing`)로 금액을 셈한다
//! - 로컬 엔진 · Ollama 는 비용이 없으므로 세지 않는다

use std::collections::BTreeMap;
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::pricing;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Tally {
    pub stt_secs: f64,
    pub stt_requests: u64,
    pub stt_failures: u64,
    pub sum_requests: u64,
    pub sum_in_tokens: u64,
    pub sum_out_tokens: u64,
    /// 모델별 사용량 (금액 셈용). 모델을 기록하기 전(v0.4.6 까지) 사용분은 위 합계에만 있다
    pub models: BTreeMap<String, ModelTally>,
}

/// 모델 하나의 사용량 — 성공한 요청만
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelTally {
    pub stt_secs: f64,
    /// 공급자가 요금을 매기는 녹음 길이 (Groq 는 요청마다 최소 10초)
    pub stt_billed_secs: f64,
    pub stt_requests: u64,
    /// 캐시를 거치지 않은 입력 토큰
    pub in_tokens: u64,
    pub out_tokens: u64,
    /// Claude 프롬프트 캐시에 쓴 입력 토큰 (입력 단가의 1.25배)
    pub cache_write_tokens: u64,
    /// Claude 프롬프트 캐시에서 읽은 입력 토큰 (입력 단가보다 훨씬 싸다)
    pub cache_read_tokens: u64,
}

impl ModelTally {
    fn add(&mut self, o: &ModelTally) {
        self.stt_secs += o.stt_secs;
        self.stt_billed_secs += o.stt_billed_secs;
        self.stt_requests += o.stt_requests;
        self.in_tokens += o.in_tokens;
        self.out_tokens += o.out_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.cache_read_tokens += o.cache_read_tokens;
    }

    /// 보낸 입력 전체 (캐시로 읽고 쓴 것까지)
    fn prompt_tokens(&self) -> u64 {
        self.in_tokens + self.cache_write_tokens + self.cache_read_tokens
    }
}

impl Tally {
    fn add(&mut self, o: &Tally) {
        self.stt_secs += o.stt_secs;
        self.stt_requests += o.stt_requests;
        self.stt_failures += o.stt_failures;
        self.sum_requests += o.sum_requests;
        self.sum_in_tokens += o.sum_in_tokens;
        self.sum_out_tokens += o.sum_out_tokens;
        for (m, t) in &o.models {
            self.models.entry(m.clone()).or_default().add(t);
        }
    }

    fn model(&mut self, model: &str) -> &mut ModelTally {
        let m = model.trim();
        self.models.entry(if m.is_empty() { "?".into() } else { m.to_string() }).or_default()
    }
}

/// 날짜("YYYY-MM-DD") → 제공자 → 사용량
type Days = BTreeMap<String, BTreeMap<String, Tally>>;

/// 쓴 양을 금액으로 — 단가표(`pricing`) 기준 추정치
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    /// 음성 인식 (USD)
    pub stt_usd: f64,
    /// 요약 (USD)
    pub sum_usd: f64,
    /// 요금이 매겨진 녹음 길이 (Groq 는 요청마다 최소 10초라 실제 길이보다 길다)
    pub stt_billed_secs: f64,
    /// 모델을 기록하기 전 사용분을 지금 설정한 모델 단가로 셈한 부분이 있다
    pub estimated: bool,
    /// 단가를 몰라 금액에서 뺀 모델
    pub unpriced: Vec<String>,
}

/// 사용량의 쓰임새 — 모델 기록 전 사용분을 어느 모델 단가로 셀지 고를 때
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Use {
    Stt,
    Summary,
}

impl Cost {
    fn unpriced(&mut self, model: &str) {
        if !self.unpriced.iter().any(|m| m == model) {
            self.unpriced.push(model.to_string());
        }
    }

    fn add_model(&mut self, provider: &str, model: &str, t: &ModelTally, usd_krw: f64) {
        if t.stt_requests > 0 || t.stt_billed_secs > 0.0 {
            self.stt_billed_secs += t.stt_billed_secs;
            match pricing::audio_price(provider, model) {
                Some(per_hour) => self.stt_usd += t.stt_billed_secs / 3600.0 * per_hour.usd(usd_krw),
                None => self.unpriced(model),
            }
        }
        if t.prompt_tokens() > 0 || t.out_tokens > 0 {
            match pricing::token_price(provider, model) {
                Some((i, o)) => {
                    let read = pricing::cache_read_price(provider, model).unwrap_or(i * 0.1);
                    self.sum_usd += (t.in_tokens as f64 * i
                        + t.cache_write_tokens as f64 * i * pricing::CACHE_WRITE_RATE
                        + t.cache_read_tokens as f64 * read
                        + t.out_tokens as f64 * o)
                        / 1e6;
                }
                None => self.unpriced(model),
            }
        }
    }

    /// 제공자 하나의 사용량을 금액으로. `legacy(쓰임새)` 는 모델 기록 전 사용분에 쓸 모델(지금 설정),
    /// `usd_krw` 는 원화 단가(리턴제로 · CLOVA Speech)를 달러로 바꿀 환율
    pub fn of(provider: &str, t: &Tally, legacy: &dyn Fn(Use) -> Option<String>, usd_krw: f64) -> Cost {
        let mut c = Cost::default();
        let mut known = ModelTally::default();
        for (model, m) in &t.models {
            c.add_model(provider, model, m, usd_krw);
            known.add(m);
        }
        // 합계에는 있는데 모델별 기록에 없는 양 = 모델을 기록하기 전 사용분
        let rest_secs = (t.stt_secs - known.stt_secs).max(0.0);
        let rest_reqs = t.stt_requests.saturating_sub(t.stt_failures).saturating_sub(known.stt_requests);
        // 요청마다의 길이는 모르니, 요청 수 × 최소 길이와 전체 길이 중 큰 쪽으로 (가장 적게 잡은 값)
        let rest_billed = match rest_reqs {
            0 => rest_secs,
            n => (pricing::billed_secs(provider, rest_secs / n as f64) * n as f64).max(rest_secs),
        };
        let rest = ModelTally {
            stt_secs: rest_secs,
            stt_billed_secs: rest_billed,
            stt_requests: rest_reqs,
            ..Default::default()
        };
        if rest.stt_requests > 0 || rest.stt_secs >= 0.5 {
            c.estimated = true;
            match legacy(Use::Stt) {
                Some(model) => c.add_model(provider, &model, &rest, usd_krw),
                None => c.unpriced("이전 기록"),
            }
        }
        let rest = ModelTally {
            in_tokens: t.sum_in_tokens.saturating_sub(known.prompt_tokens()),
            out_tokens: t.sum_out_tokens.saturating_sub(known.out_tokens),
            ..Default::default()
        };
        if rest.in_tokens > 0 || rest.out_tokens > 0 {
            c.estimated = true;
            match legacy(Use::Summary) {
                Some(model) => c.add_model(provider, &model, &rest, usd_krw),
                None => c.unpriced("이전 기록"),
            }
        }
        c
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub today: BTreeMap<String, Tally>,
    pub month: BTreeMap<String, Tally>,
    pub today_cost: BTreeMap<String, Cost>,
    pub month_cost: BTreeMap<String, Cost>,
    pub rates: Vec<badasugi_core::usage::RateSnapshot>,
}

pub struct UsageBook {
    path: PathBuf,
    days: Mutex<Days>,
    dirty: Mutex<bool>,
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

impl UsageBook {
    pub fn load(path: PathBuf) -> Self {
        let days = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            days: Mutex::new(days),
            dirty: Mutex::new(false),
        }
    }

    fn with(&self, provider: &str, f: impl FnOnce(&mut Tally)) {
        if provider == "ollama" || provider.is_empty() {
            return;
        }
        let mut days = self.days.lock();
        f(days.entry(today()).or_default().entry(provider.to_string()).or_default());
        *self.dirty.lock() = true;
    }

    /// 음성 인식 API 로 보낸 발화 하나
    pub fn stt(&self, provider: &str, model: &str, secs: f64) {
        self.with(provider, |t| {
            t.stt_secs += secs;
            t.stt_requests += 1;
            let m = t.model(model);
            m.stt_secs += secs;
            m.stt_billed_secs += pricing::billed_secs(provider, secs);
            m.stt_requests += 1;
        });
    }

    /// 보냈지만 실패한 음성 인식 요청 — 요청 수에도 센다 (모두 실패해도 사용량에 보이게). 소리 길이는 더하지 않는다
    pub fn stt_failed(&self, provider: &str) {
        self.with(provider, |t| {
            t.stt_requests += 1;
            t.stt_failures += 1;
        });
    }

    /// 요약 · 묻기 요청 하나 (`cache_write` · `cache_read`: Claude 프롬프트 캐시로 쓰고 읽은 입력 토큰)
    pub fn summary(&self, provider: &str, model: &str, in_tokens: u64, out_tokens: u64, cache_write: u64, cache_read: u64) {
        self.with(provider, |t| {
            t.sum_requests += 1;
            t.sum_in_tokens += in_tokens + cache_write + cache_read;
            t.sum_out_tokens += out_tokens;
            let m = t.model(model);
            m.in_tokens += in_tokens;
            m.out_tokens += out_tokens;
            m.cache_write_tokens += cache_write;
            m.cache_read_tokens += cache_read;
        });
    }

    /// 바뀐 게 있으면 저장 (400일 넘은 기록은 정리)
    pub fn save(&self) {
        if !std::mem::take(&mut *self.dirty.lock()) {
            return;
        }
        let mut days = self.days.lock();
        let cutoff = (chrono::Local::now() - chrono::Duration::days(400)).format("%Y-%m-%d").to_string();
        days.retain(|d, _| *d >= cutoff);
        if let Ok(json) = serde_json::to_vec_pretty(&*days) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let tmp = self.path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }

    /// 오늘 · 이번 달 사용량과 금액. `legacy(제공자, 쓰임새)` 는 모델을 기록하기 전 사용분에 쓸 모델(지금 설정),
    /// `usd_krw` 는 원화 단가를 달러로 바꿀 환율 (설정 › 사용량)
    pub fn report(&self, legacy: &dyn Fn(&str, Use) -> Option<String>, usd_krw: f64) -> UsageReport {
        let days = self.days.lock();
        let t = today();
        let month = &t[..7];
        let mut m: BTreeMap<String, Tally> = BTreeMap::new();
        for (_, providers) in days.iter().filter(|(d, _)| d.starts_with(month)) {
            for (p, tally) in providers {
                m.entry(p.clone()).or_default().add(tally);
            }
        }
        let today = days.get(&t).cloned().unwrap_or_default();
        let cost = |tallies: &BTreeMap<String, Tally>| -> BTreeMap<String, Cost> {
            tallies.iter().map(|(p, t)| (p.clone(), Cost::of(p, t, &|u| legacy(p, u), usd_krw))).collect()
        };
        UsageReport {
            today_cost: cost(&today),
            month_cost: cost(&m),
            today,
            month: m,
            rates: badasugi_core::usage::snapshot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_legacy(_: &str, _: Use) -> Option<String> {
        None
    }

    const KRW: f64 = crate::settings::DEFAULT_USD_KRW;

    #[test]
    fn korean_services_are_priced_in_won_with_their_minimums() {
        let book = UsageBook::load(std::env::temp_dir().join("bd-usage-nofile-krw/usage.json"));
        // 리턴제로: 4초도 10초로 · 시간당 1,000원
        book.stt("rtzr", "sommers", 4.0);
        book.stt("rtzr", "sommers", 20.0);
        // CLOVA Speech: 15초 단위 올림 · 15초 5원
        book.stt("clova", "장문 인식", 3.0);
        book.stt("clova", "장문 인식", 16.0);
        // ElevenLabs: 초 단위 · 용어를 보내면 시간당 $0.25
        book.stt("elevenlabs", "scribe_v3+keyterms", 36.0);
        // 환율을 바꿔도 원화 금액은 그대로
        let r = book.report(&no_legacy, 1500.0);
        let won = |p: &str| r.today_cost[p].stt_usd * 1500.0;
        assert!((r.today_cost["rtzr"].stt_billed_secs - 30.0).abs() < 1e-9);
        assert!((won("rtzr") - 30.0 / 3600.0 * 1000.0).abs() < 1e-9);
        assert!((r.today_cost["clova"].stt_billed_secs - 45.0).abs() < 1e-9);
        assert!((won("clova") - 15.0).abs() < 1e-9, "15초 세 번어치 = 15원");
        assert!((r.today_cost["elevenlabs"].stt_usd - 36.0 / 3600.0 * 0.25).abs() < 1e-12);
    }

    #[test]
    fn counts_api_use_by_provider_and_skips_local() {
        let dir = std::env::temp_dir().join(format!("bd-usage-test-{}", std::process::id()));
        let path = dir.join("usage.json");
        let book = UsageBook::load(path.clone());
        book.stt("groq", "whisper-large-v3-turbo", 3.5);
        book.stt("groq", "whisper-large-v3-turbo", 2.0);
        book.stt_failed("groq");
        book.summary("gemini", "gemini-2.5-flash-lite", 1200, 300, 0, 0);
        book.summary("ollama", "exaone3.5:7.8b", 999, 999, 0, 0); // 로컬은 세지 않음
        book.save();
        let again = UsageBook::load(path);
        let r = again.report(&no_legacy, KRW);
        let g = &r.today["groq"];
        assert_eq!((g.stt_requests, g.stt_failures), (3, 1), "실패한 요청도 요청 수에");
        assert!((g.stt_secs - 5.5).abs() < 1e-9);
        assert_eq!(r.month["gemini"].sum_in_tokens, 1200);
        assert_eq!(r.month["gemini"].models["gemini-2.5-flash-lite"].out_tokens, 300);
        assert!(!r.today.contains_key("ollama"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prices_usage_per_model() {
        let mut t = Tally::default();
        // 요약 2회: 입력 51,788 · 출력 11,674 토큰 (Opus 5.5: $4 / $20)
        {
            let m = t.model("claude-opus-5-5");
            m.in_tokens = 51_788;
            m.out_tokens = 11_674;
        }
        t.sum_requests = 2;
        t.sum_in_tokens = 51_788;
        t.sum_out_tokens = 11_674;
        let c = Cost::of("anthropic", &t, &|_| None, KRW);
        assert!((c.sum_usd - (51_788.0 * 4.0 + 11_674.0 * 20.0) / 1e6).abs() < 1e-9);
        assert!(!c.estimated && c.unpriced.is_empty());

        // 묻기: 회의 기록을 캐시에 쓰고(1.25배) 다음 질문에서 읽는다(Opus 5.5 는 $0.20)
        let book = UsageBook::load(std::env::temp_dir().join("bd-usage-nofile-cache/usage.json"));
        book.summary("anthropic", "claude-opus-5-5", 100, 50, 20_000, 0);
        book.summary("anthropic", "claude-opus-5-5", 120, 60, 0, 20_000);
        let r = book.report(&no_legacy, KRW);
        assert_eq!(r.today["anthropic"].sum_in_tokens, 40_220, "입력 합계에는 캐시로 읽고 쓴 것까지");
        let c = &r.today_cost["anthropic"];
        let want = (220.0 * 4.0 + 20_000.0 * 4.0 * 1.25 + 20_000.0 * 0.20 + 110.0 * 20.0) / 1e6;
        assert!((c.sum_usd - want).abs() < 1e-9, "{} != {want}", c.sum_usd);
        assert!(!c.estimated, "캐시 토큰을 모델 기록 전 사용분으로 잘못 보지 않는다");

        // Groq 음성 인식: 3초짜리 요청도 10초로 매긴다
        let book = UsageBook::load(std::env::temp_dir().join("bd-usage-nofile/usage.json"));
        book.stt("groq", "whisper-large-v3-turbo", 3.0);
        book.stt("groq", "whisper-large-v3-turbo", 30.0);
        let r = book.report(&no_legacy, KRW);
        let c = &r.today_cost["groq"];
        assert!((c.stt_billed_secs - 40.0).abs() < 1e-9);
        assert!((c.stt_usd - 40.0 / 3600.0 * 0.04).abs() < 1e-12);
    }

    #[test]
    fn usage_before_models_were_recorded_is_estimated_with_current_model() {
        // v0.4.6 까지의 기록: 합계만 있고 모델별 기록이 없다
        let old: Tally = serde_json::from_str(
            r#"{"sttSecs":40,"sttRequests":5,"sttFailures":1,"sumRequests":1,"sumInTokens":1000000,"sumOutTokens":0}"#,
        )
        .unwrap();
        let legacy = |u: Use| {
            Some(match u {
                Use::Stt => "whisper-large-v3-turbo".to_string(),
                Use::Summary => "openai/gpt-oss-120b".to_string(),
            })
        };
        let c = Cost::of("groq", &old, &legacy, KRW);
        assert!(c.estimated);
        // 성공 4회 · 40초 → 요청마다 10초 최소라 40초
        assert!((c.stt_billed_secs - 40.0).abs() < 1e-9);
        assert!((c.sum_usd - 0.15).abs() < 1e-9);

        // 지금 설정에서도 모델을 모르면 금액에서 빼고 알린다
        let c = Cost::of("custom", &old, &|_| None, KRW);
        assert_eq!(c.unpriced, vec!["이전 기록".to_string()]);
        assert_eq!(c.stt_usd + c.sum_usd, 0.0);
    }
}
