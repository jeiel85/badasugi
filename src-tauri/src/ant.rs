//! Claude 를 API 키 없이 쓰기 — Anthropic 공식 CLI(`ant`) 로그인 토큰
//!
//! 사용자가 명령 프롬프트에서 `ant auth login` 으로 Claude Console 계정에 한 번 로그인해 두면,
//! 요청마다 `ant auth print-credentials --access-token` 으로 짧게 유효한 토큰을 받아 쓴다
//! (만료가 가까우면 ant 가 알아서 새로 받아 준다). 로그인 정보 파일은 앱이 직접 읽지 않는다.
//! 과금은 API 키와 같은 Console 크레딧에서 빠진다.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

const EXE: &str = "ant.exe";
/// 토큰을 새로 받느라 네트워크를 탈 수 있어 넉넉히
const TIMEOUT: Duration = Duration::from_secs(30);

/// ant.exe 를 찾는다. 앱이 트레이에 오래 떠 있는 동안 ant 를 설치해 PATH 가 바뀌었을 수 있어
/// 프로세스 PATH 뒤에 레지스트리의 지금 PATH 도 본다
pub fn find() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(registry_path_dirs());
    // PATH 에 넣지 않고 풀어 두기 쉬운 곳 · `go install` 위치
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(Path::new(&local).join("Programs").join("ant"));
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        dirs.push(Path::new(&home).join("go").join("bin"));
    }
    find_in(&dirs)
}

fn find_in(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(EXE)).find(|p| p.is_file())
}

/// 로그인한 프로필의 액세스 토큰
pub fn access_token(ant: &Path) -> Result<String> {
    let mut cmd = Command::new(ant);
    cmd.args(["auth", "print-credentials", "--access-token"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000; // 요약할 때마다 콘솔 창이 깜빡이지 않게
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| anyhow!("ant 를 실행하지 못했어요 ({}): {e}", ant.display()))?;
    // 출력이 파이프 버퍼를 넘쳐 ant 가 멈추지 않게 따로 읽는다
    let drain = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut r) = r {
                let _ = r.read_to_string(&mut s);
            }
            s
        })
    };
    let out = drain(child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!("ant 가 {}초 동안 응답하지 않아요", TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if !status.success() {
        let why = err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        bail!("ant 로그인 정보를 가져오지 못했어요. 명령 프롬프트에서 ant auth login 을 실행해 주세요 ({why})");
    }
    parse_token(&out)
}

/// `--access-token` 출력 = 토큰 한 줄. 플래그 없이 찍히는 JSON 이나 빈 출력은 거른다
fn parse_token(stdout: &str) -> Result<String> {
    let t = stdout.trim();
    if t.is_empty() || t.starts_with('{') || t.contains(char::is_whitespace) {
        bail!("ant 가 알 수 없는 형식으로 답했어요. ant 를 최신 버전으로 바꿔 주세요");
    }
    Ok(t.to_string())
}

/// 레지스트리에 저장된 지금의 사용자 · 시스템 PATH (%변수% 는 펼친 채로)
#[cfg(windows)]
fn registry_path_dirs() -> Vec<PathBuf> {
    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let name = wide("Path");
    let mut dirs = Vec::new();
    for (root, sub) in [
        (HKEY_CURRENT_USER, "Environment"),
        (HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment"),
    ] {
        let sub = wide(sub);
        let mut buf = vec![0u16; 4096];
        loop {
            let mut bytes = (buf.len() * 2) as u32;
            // RRF_RT_REG_SZ: REG_EXPAND_SZ 도 %USERPROFILE% 같은 변수를 펼쳐 준다
            let rc = unsafe {
                RegGetValueW(root, sub.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
            };
            if rc == ERROR_MORE_DATA && buf.len() < 1 << 20 {
                buf.resize((bytes as usize / 2).max(buf.len() * 2), 0);
                continue;
            }
            if rc == 0 {
                let s = String::from_utf16_lossy(&buf[..bytes as usize / 2]);
                dirs.extend(std::env::split_paths(s.trim_end_matches('\0')));
            }
            break;
        }
    }
    dirs
}

#[cfg(not(windows))]
fn registry_path_dirs() -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_one_bare_line() {
        assert_eq!(parse_token("sk-ant-oat01-abc_DEF\r\n").unwrap(), "sk-ant-oat01-abc_DEF");
        assert!(parse_token("").is_err());
        assert!(parse_token("  \n").is_err());
        assert!(parse_token("{\"access_token\":\"x\"}").is_err());
        assert!(parse_token("note: something\ntoken").is_err());
    }

    #[test]
    fn finds_ant_in_the_first_dir_that_has_it() {
        let root = std::env::temp_dir().join(format!("badasugi-ant-{}", std::process::id()));
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(b.join(EXE), b"").unwrap();
        std::fs::write(c.join(EXE), b"").unwrap();
        // 폴더 이름이 ant.exe 인 것은 실행 파일이 아니다
        std::fs::create_dir_all(a.join(EXE)).unwrap();
        assert_eq!(find_in(&[a.clone(), b.clone(), c.clone()]), Some(b.join(EXE)));
        assert_eq!(find_in(&[a.clone(), root.join("none")]), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn registry_path_is_readable() {
        // 시스템 PATH 에는 언제나 Windows 폴더가 있다
        let dirs = registry_path_dirs();
        assert!(dirs.iter().any(|d| d.to_string_lossy().to_lowercase().contains("windows")), "{dirs:?}");
        assert!(dirs.iter().all(|d| !d.to_string_lossy().contains('%')), "변수가 펼쳐지지 않음: {dirs:?}");
    }
}
