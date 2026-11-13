//! 진단 로그 — 문제가 생겼을 때 원인을 찾기 위한 기록
//!
//! - 위치: `%LOCALAPPDATA%\com.badasugi.desktop\logs\badasugi-YYYY-MM-DD.log` (설정 › 로그 폴더)
//! - 7일치만 보관하고, 하루 10MB 를 넘으면 그날은 더 쓰지 않는다
//! - 녹취록 내용 · API 키는 남기지 않는다 (앱의 로그 문구를 그렇게 쓴다)
//! - 앱이 갑자기 죽는 경우(패닉)도 어느 스레드 · 어느 코드였는지와 함께 남긴다
//! - 기본은 이 앱의 정보(info) 이상 + 다른 라이브러리의 경고 이상. `BADASUGI_LOG=debug` 로 자세히

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use log::{LevelFilter, Log, Metadata, Record};
use parking_lot::Mutex;

const KEEP_DAYS: i64 = 7;
const MAX_BYTES: u64 = 10 * 1024 * 1024;

/// 로그 폴더 (자동 점검 모드면 점검용 임시 폴더 안)
pub fn log_dir() -> PathBuf {
    if let Some(home) = crate::smoke::home() {
        return home.join("logs");
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("com.badasugi.desktop")
        .join("logs")
}

struct Day {
    date: String,
    file: File,
    bytes: u64,
    capped: bool,
}

struct FileLogger {
    dir: PathBuf,
    ours: LevelFilter,
    day: Mutex<Option<Day>>,
}

impl FileLogger {
    fn open(&self, date: &str) -> Option<Day> {
        std::fs::create_dir_all(&self.dir).ok()?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(format!("badasugi-{date}.log")))
            .ok()?;
        let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(Day {
            date: date.to_string(),
            file,
            bytes,
            capped: bytes >= MAX_BYTES,
        })
    }
}

impl Log for FileLogger {
    fn enabled(&self, m: &Metadata) -> bool {
        // 이 앱(badasugi, badasugi_core)은 정보 이상, 다른 라이브러리는 경고 이상만
        let limit = if m.target().starts_with("badasugi") {
            self.ours
        } else {
            LevelFilter::Warn
        };
        m.level() <= limit
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let now = chrono::Local::now();
        let line = format!("{} {:<5} {}: {}\n", now.format("%H:%M:%S%.3f"), r.level(), r.target(), r.args());
        if cfg!(debug_assertions) {
            eprint!("{line}");
        }
        let date = now.format("%Y-%m-%d").to_string();
        let mut g = self.day.lock();
        if g.as_ref().is_none_or(|d| d.date != date) {
            *g = self.open(&date);
        }
        let Some(d) = g.as_mut() else {
            return;
        };
        if d.capped {
            return;
        }
        if d.bytes + line.len() as u64 > MAX_BYTES {
            let _ = d.file.write_all("… 오늘 로그가 너무 커서 더 남기지 않아요\n".as_bytes());
            d.capped = true;
            return;
        }
        if d.file.write_all(line.as_bytes()).is_ok() {
            d.bytes += line.len() as u64;
        }
    }

    fn flush(&self) {
        if let Some(d) = self.day.lock().as_mut() {
            let _ = d.file.flush();
        }
    }
}

/// 앱을 켤 때 한 번 (Tauri 보다 먼저)
pub fn init() {
    let dir = log_dir();
    remove_old(&dir, chrono::Local::now().date_naive());
    let ours = match std::env::var("BADASUGI_LOG").ok().as_deref() {
        Some("trace") => LevelFilter::Trace,
        Some("debug") => LevelFilter::Debug,
        Some("warn") => LevelFilter::Warn,
        _ => LevelFilter::Info,
    };
    // 앱이 끝날 때까지 쓰므로 한 번 만들어 놓고 놓아준다
    let logger: &'static FileLogger = Box::leak(Box::new(FileLogger {
        dir,
        ours,
        day: Mutex::new(None),
    }));
    if log::set_logger(logger).is_ok()
    {
        log::set_max_level(ours.max(LevelFilter::Warn));
    }
    // 패닉: 어느 스레드 · 어느 코드에서였는지 남기고, 원래 처리(종료 등)는 그대로
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("이름 없음").to_string();
        log::error!("패닉 (스레드 {thread}): {info}");
        log::logger().flush();
        prev(info);
    }));
}

/// 보관 기간이 지난 로그 파일 지우기
fn remove_old(dir: &Path, today: chrono::NaiveDate) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = today - chrono::Duration::days(KEEP_DAYS);
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(date) = name.strip_prefix("badasugi-").and_then(|n| n.strip_suffix(".log")) else {
            continue;
        };
        if chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok_and(|d| d < cutoff) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::remove_old;

    #[test]
    fn keeps_only_the_last_seven_days() {
        let dir = std::env::temp_dir().join(format!("bd-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["badasugi-2026-11-01.log", "badasugi-2026-11-06.log", "badasugi-2026-11-13.log", "other.log"] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        remove_old(&dir, chrono::NaiveDate::from_ymd_opt(2026, 11, 13).unwrap());
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        assert_eq!(left, ["badasugi-2026-11-06.log", "badasugi-2026-11-13.log", "other.log"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
