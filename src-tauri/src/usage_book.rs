//! 이 앱으로 쓴 API 사용량 — 제공자별 · 날짜별로 세어 `usage.json` 에 둔다 (설정 › API 사용량)
//!
//! - 음성 인식: API 로 보낸 녹음 길이 · 요청 수 · 실패 수
//! - 요약: 요청 수 · 입력/출력 토큰 (응답에 담겨 온 값)
//! - 로컬 엔진 · Ollama 는 비용이 없으므로 세지 않는다

use std::collections::BTreeMap;
use std::path::PathBuf;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Tally {
    pub stt_secs: f64,
    pub stt_requests: u64,
    pub stt_failures: u64,
    pub sum_requests: u64,
    pub sum_in_tokens: u64,
    pub sum_out_tokens: u64,
}

impl Tally {
    fn add(&mut self, o: &Tally) {
        self.stt_secs += o.stt_secs;
        self.stt_requests += o.stt_requests;
        self.stt_failures += o.stt_failures;
        self.sum_requests += o.sum_requests;
        self.sum_in_tokens += o.sum_in_tokens;
        self.sum_out_tokens += o.sum_out_tokens;
    }
}

/// 날짜("YYYY-MM-DD") → 제공자 → 사용량
type Days = BTreeMap<String, BTreeMap<String, Tally>>;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub today: BTreeMap<String, Tally>,
    pub month: BTreeMap<String, Tally>,
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
    pub fn stt(&self, provider: &str, secs: f64) {
        self.with(provider, |t| {
            t.stt_secs += secs;
            t.stt_requests += 1;
        });
    }

    pub fn stt_failed(&self, provider: &str) {
        self.with(provider, |t| t.stt_failures += 1);
    }

    pub fn summary(&self, provider: &str, in_tokens: u64, out_tokens: u64) {
        self.with(provider, |t| {
            t.sum_requests += 1;
            t.sum_in_tokens += in_tokens;
            t.sum_out_tokens += out_tokens;
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

    pub fn report(&self) -> UsageReport {
        let days = self.days.lock();
        let t = today();
        let month = &t[..7];
        let mut m: BTreeMap<String, Tally> = BTreeMap::new();
        for (d, providers) in days.iter().filter(|(d, _)| d.starts_with(month)) {
            let _ = d;
            for (p, tally) in providers {
                m.entry(p.clone()).or_default().add(tally);
            }
        }
        UsageReport {
            today: days.get(&t).cloned().unwrap_or_default(),
            month: m,
            rates: badasugi_core::usage::snapshot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_api_use_by_provider_and_skips_local() {
        let dir = std::env::temp_dir().join(format!("bd-usage-test-{}", std::process::id()));
        let path = dir.join("usage.json");
        let book = UsageBook::load(path.clone());
        book.stt("groq", 3.5);
        book.stt("groq", 2.0);
        book.stt_failed("groq");
        book.summary("gemini", 1200, 300);
        book.summary("ollama", 999, 999); // 로컬은 세지 않음
        book.save();
        let again = UsageBook::load(path);
        let r = again.report();
        let g = &r.today["groq"];
        assert_eq!((g.stt_requests, g.stt_failures), (2, 1));
        assert!((g.stt_secs - 5.5).abs() < 1e-9);
        assert_eq!(r.month["gemini"].sum_in_tokens, 1200);
        assert!(!r.today.contains_key("ollama"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
