//! 개인정보 가리기 · 녹음 보관 기간 (기능 확장 계획 16번)
//!
//! - **가리기** ([`mask`]): 전화번호 · 주민등록번호 · 외국인등록번호 · 카드번호 · 계좌번호처럼 보이는 숫자 · 이메일을
//!   `[전화번호]` 같은 자리 글로 바꾼다. 규칙(정규식)으로 PC 안에서 — 밖으로 보내는 것이 없다.
//!   - AI 로 보낼 때(요약 · 회의에 묻기 · 모든 회의에 묻기 · 따라잡기 · 번역): 보낼 사본만 가린다([`for_ai`]).
//!     기록의 원문은 그대로라 답의 근거 시각은 원문 문장으로 이어진다.
//!   - 내보낼 때(Markdown · Word · PDF · 텍스트 · 자막): 내보낼 사본만 가린다. JSON 은 원본 데이터라 그대로.
//!   - 음성 인식은 숫자를 글자로 적기도 해서("공일공 일이삼사") 그런 말은 잡지 못한다 — 숫자로 받아 적힌 것만.
//! - **녹음 보관 기간** ([`expire_audio`]): 정한 날수가 지난 회의의 녹음 파일(.opus)을 지운다. 녹취록 · 요약 · 메모는
//!   남기고, 기록에서 녹음 파일 이름을 비운다(다시 듣기 자리에 녹음이 없다고 나온다). 받아 적은 원본 파일
//!   (파일 받아쓰기)은 사용자의 파일이라 건드리지 않는다.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::session::SessionDoc;
use crate::summary::{Reply, Turn};

/// 개인정보 설정
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PrivacySettings {
    /// AI 로 보낼 때 가리기
    pub mask_ai: bool,
    /// 내보낼 때 가리기 (JSON 빼고)
    pub mask_export: bool,
    /// 녹음 파일 보관 날수 (0 = 계속 보관)
    pub audio_keep_days: u32,
}

/// (규칙, 자리 글, 최소 숫자 수) — 앞의 것부터. 숫자 경계 `\b` 는 ASCII 로 바꿔 쓴다(`(?-u:\b)`):
/// 한글 조사가 바로 붙어도("010-1234-5678로") 숫자 끝으로 보게
static RULES: LazyLock<Vec<(Regex, &'static str, usize)>> = LazyLock::new(|| {
    let r = |p: &str| Regex::new(&p.replace(r"\b", r"(?-u:\b)")).expect("개인정보 규칙");
    vec![
        // 이메일
        (r(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9\-]+(?:\.[A-Za-z0-9\-]+)+"), "[이메일]", 0),
        // 주민등록번호 · 외국인등록번호: 생년월일 6자리 - 뒷자리 7자리 (뒷자리 첫 숫자 1~8)
        (r(r"\b\d{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12]\d|3[01])\s?[-–]?\s?[1-8]\d{6}\b"), "[주민번호]", 13),
        // 카드번호: 4자리 넷 (빈칸 · - 로 나눠도), 아멕스 4-6-5
        (r(r"\b\d{4}[\s\-]?\d{4}[\s\-]?\d{4}[\s\-]?\d{4}\b|\b3[47]\d{2}[\s\-]?\d{6}[\s\-]?\d{5}\b"), "[카드번호]", 15),
        // 전화번호: 휴대폰 010 · 011 … / 지역 02 · 031 … / 070 · 050x / 1588 같은 대표번호
        (r(r"\b01[016789][\s\-.]?\d{3,4}[\s\-.]?\d{4}\b"), "[전화번호]", 10),
        (r(r"\b0(?:2|[3-6][1-5]|70|50\d)[\s\-.)]?\d{3,4}[\s\-.]?\d{4}\b"), "[전화번호]", 9),
        (r(r"\b1[5-9]\d{2}[\s\-]\d{4}\b"), "[전화번호]", 8),
        // 계좌번호: 숫자 무리 3~4개를 - 로 이은 10자리 이상 (예: 110-123-456789, 1002-123-456789)
        (r(r"\b\d{2,6}-\d{2,6}-\d{2,8}(?:-\d{1,3})?\b"), "[계좌번호]", 10),
    ]
});

/// 글 속 개인정보처럼 보이는 것을 자리 글로 → (가린 글, 가린 수)
pub fn mask(text: &str) -> (String, usize) {
    let mut out = text.to_string();
    let mut n = 0;
    for (re, tag, min) in RULES.iter() {
        if !re.is_match(&out) {
            continue;
        }
        // 숫자가 모자라면 그대로 — 계좌번호 꼴은 날짜(2026-11-20) 같은 것과도 맞는다
        out = re
            .replace_all(&out, |c: &regex::Captures| {
                let m = &c[0];
                if m.chars().filter(char::is_ascii_digit).count() >= *min {
                    n += 1;
                    (*tag).to_string()
                } else {
                    m.to_string()
                }
            })
            .into_owned();
    }
    (out, n)
}

/// 기록 사본을 가린다: 문장 · 제목 · 화자 이름 · 요약 · 메모 · 묻고 답한 것 · 번역 · 참석자 · 받아 적은 파일 이름
/// (시각 · 화자 번호 · 배정 · id 는 그대로)
pub fn mask_doc(doc: &SessionDoc) -> SessionDoc {
    let mut d = doc.clone();
    let m = |s: &mut String| *s = mask(s).0;
    m(&mut d.title);
    for u in &mut d.utterances {
        m(&mut u.text);
    }
    // 화자 이름을 메일 · 번호로 붙인 경우 — 녹취록 · AI 에 보내는 글의 "이름: 말" 에 들어간다
    for name in d.speakers.values_mut() {
        m(name);
    }
    for a in &mut d.attendees {
        m(a);
    }
    if let Some(f) = d.source_file.as_mut() {
        m(f);
    }
    for c in d.comments.values_mut() {
        m(c);
    }
    if let Some(s) = d.summary.as_mut() {
        m(s);
    }
    m(&mut d.notes);
    for x in &mut d.qa {
        m(&mut x.q);
        m(&mut x.a);
    }
    if let Some(t) = d.translation.as_mut() {
        for x in t.items.values_mut() {
            // 원문(src)도 같은 규칙으로 가려야 가린 문장과 짝이 맞는다
            m(&mut x.src);
            m(&mut x.text);
        }
    }
    d
}

/// AI 로 보낼 사본 — 설정에서 켰으면 가린 것, 아니면 그대로
pub fn for_ai(doc: SessionDoc, p: &PrivacySettings) -> SessionDoc {
    if p.mask_ai {
        mask_doc(&doc)
    } else {
        doc
    }
}

/// 보내는 글(차례마다)을 가려 `chat`(AI 와 주고받는 함수, summary::Chat 모양)으로 넘기는 함수 — 회의에 묻기 ·
/// 모든 회의에 묻기. 질문과 맞는 회의 · 구간은 원문으로 고르고(번호 · 메일로 찾는 질문도 맞는 회의를 고르게),
/// 고른 뒤 만든 글만 나갈 때 가린다: 회의 기록 덩어리 · 앞선 질문 · 답(가리기를 켜기 전 것도) · 직접 쓴 질문 모두
pub fn masked_chat<F>(mut chat: F) -> impl FnMut(&str, &[Turn], &mut dyn FnMut(&str)) -> anyhow::Result<Reply>
where
    F: FnMut(&str, &[Turn], &mut dyn FnMut(&str)) -> anyhow::Result<Reply>,
{
    move |system: &str, turns: &[Turn], on_text: &mut dyn FnMut(&str)| {
        let texts: Vec<String> = turns.iter().map(|t| mask(t.text).0).collect();
        let masked: Vec<Turn> = turns.iter().zip(&texts).map(|(t, s)| Turn { user: t.user, text: s, cache: t.cache }).collect();
        chat(system, &masked, on_text)
    }
}

/// 기록 파일을 읽어 보관 기간이 지난 녹음이 있으면 (기록, 녹음 파일 이름).
/// 기록마다 회의 날짜(startedAt)로 따진다. 다른 폴더를 가리키는 이름은 건드리지 않는다
fn expired_doc(json: &std::path::Path, keep_days: u32, today: chrono::NaiveDate) -> Option<(SessionDoc, String)> {
    let doc = serde_json::from_str::<SessionDoc>(&std::fs::read_to_string(json).ok()?).ok()?;
    let audio = doc.audio_file.clone().filter(|a| !a.is_empty())?;
    let day = doc.started_at.get(..10).and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())?;
    let old = keep_days > 0 && (today - day).num_days() >= keep_days as i64;
    (old && !audio.contains(['/', '\\']) && !audio.contains("..")).then_some((doc, audio))
}

/// 보관 기간이 지난 녹음이 있어 보이는 기록 파일들 — 기록 잠금 없이 훑는다 (지우기는 [`expire_one`] 이 잠금 안에서 다시 보고)
pub fn expired(dir: &std::path::Path, keep_days: u32, today: chrono::NaiveDate) -> Vec<std::path::PathBuf> {
    if keep_days == 0 {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| expired_doc(p, keep_days, today).is_some())
        .collect()
}

/// 기록 하나의 녹음 파일을 지운다 → 지웠나. 기록 잠금을 쥔 채 부른다 — 그 사이 저장과 엇갈리지 않게 파일을 다시 읽고,
/// `skip`(녹음 중인 기록 id)은 건너뛴다.
/// 녹음 파일을 먼저 지우고 그다음 기록의 녹음 이름을 비운다: 파일을 못 지우면(재생 중 · 권한) 이름을 그대로 둬 다음에
/// 다시 하고, 이름을 비우다 실패해도 다음 차례에 "파일은 없고 이름만 남은" 기록으로 다시 잡혀 이름만 비운다
pub fn expire_one(dir: &std::path::Path, json: &std::path::Path, keep_days: u32, today: chrono::NaiveDate, skip: Option<&str>) -> bool {
    let Some((mut doc, audio)) = expired_doc(json, keep_days, today) else {
        return false;
    };
    if skip == Some(doc.id.as_str()) {
        return false;
    }
    let ap = dir.join(&audio);
    let gone = ap.exists();
    if gone {
        if let Err(e) = std::fs::remove_file(&ap) {
            log::warn!("녹음 보관 기간이 지났지만 녹음 파일을 지우지 못함 — 다음에 다시: {} ({e})", ap.display());
            return false;
        }
        log::info!("녹음 보관 기간({keep_days}일)이 지나 지움: {}", ap.display());
    }
    doc.audio_file = None;
    // 기록과 같은 방식으로 (다 쓴 뒤 바꿔치기 · Markdown 회의록도)
    if let Err(e) = crate::session::write_doc(dir, &doc) {
        log::warn!("녹음 파일은 지웠지만 기록의 녹음 이름을 비우지 못함 — 다음에 다시: {e}");
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_common_korean_personal_numbers() {
        let cases = [
            ("연락처는 010-1234-5678 이에요", "연락처는 [전화번호] 이에요"),
            ("01012345678 로 문자 주세요", "[전화번호] 로 문자 주세요"),
            ("사무실 02-123-4567, 지사 031 987 6543", "사무실 [전화번호], 지사 [전화번호]"),
            ("대표번호 1588-1234", "대표번호 [전화번호]"),
            ("주민번호 900101-1234567 확인", "주민번호 [주민번호] 확인"),
            ("카드 1234 5678 9012 3456 으로", "카드 [카드번호] 으로"),
            ("카드 1234-5678-9012-3456", "카드 [카드번호]"),
            ("메일은 kim.lee+work@example.co.kr 로", "메일은 [이메일] 로"),
            ("계좌 110-123-456789 로 보내 주세요", "계좌 [계좌번호] 로 보내 주세요"),
            // 조사가 바로 붙어도
            ("010-1234-5678로 연락", "[전화번호]로 연락"),
            ("900101-1234567을 적어", "[주민번호]을 적어"),
            ("1002-123-456789에서", "[계좌번호]에서"),
        ];
        for (src, want) in cases {
            assert_eq!(mask(src).0, want, "{src}");
        }
        assert_eq!(mask("연락처 010-1234-5678, 메일 a@b.com").1, 2);
    }

    #[test]
    fn leaves_ordinary_numbers_alone() {
        for s in [
            "2026-11-20 회의",
            "평균 2초에서 0.4초로",
            "매출 1,234,567원",
            "버전 0.4.31",
            "15일 출시, 3분기 목표 120%",
            "[12:34] 근거 시각",
            "회의실 1204호",
            "주문 번호 20261120",
        ] {
            assert_eq!(mask(s), (s.to_string(), 0), "{s}");
        }
    }

    #[test]
    fn masked_doc_keeps_times_and_ids() {
        let d: SessionDoc = serde_json::from_value(serde_json::json!({
            "version": 1, "id": "m", "title": "010-1111-2222 고객 통화",
            "speakers": { "1": "kim@example.com" }, "attendees": ["이 매니저 <lee@example.com>"],
            "sourceFile": "010-9999-8888 통화.m4a",
            "utterances": [{ "id": 7, "startMs": 5000, "endMs": 9000, "speaker": 1, "text": "제 번호는 010-1234-5678 입니다" }],
            "summary": "- 고객 연락처 010-1234-5678 [00:05]", "notes": "a@b.com",
            "qa": [{ "q": "번호?", "a": "010-1234-5678 [00:05]", "at": "", "model": "" }]
        }))
        .unwrap();
        let m = mask_doc(&d);
        assert_eq!(m.title, "[전화번호] 고객 통화");
        assert_eq!((m.utterances[0].id, m.utterances[0].start_ms, m.utterances[0].speaker), (7, 5000, Some(1)));
        assert_eq!(m.utterances[0].text, "제 번호는 [전화번호] 입니다");
        assert_eq!(m.speaker_name(Some(1)), "[이메일]", "화자 이름도 (번호 · 배정은 그대로)");
        assert_eq!(m.attendees, vec!["이 매니저 <[이메일]>".to_string()]);
        assert_eq!(m.source_file.as_deref(), Some("[전화번호] 통화.m4a"));
        assert_eq!(m.summary.as_deref(), Some("- 고객 연락처 [전화번호] [00:05]"), "근거 시각은 그대로");
        assert_eq!(m.notes, "[이메일]");
        assert_eq!(m.qa[0].a, "[전화번호] [00:05]");
        assert_eq!(d.utterances[0].text, "제 번호는 010-1234-5678 입니다", "원본은 그대로");
        let off = for_ai(d.clone(), &PrivacySettings::default());
        assert_eq!(off.utterances[0].text, d.utterances[0].text);
        let on = for_ai(d, &PrivacySettings { mask_ai: true, ..Default::default() });
        assert_eq!(on.utterances[0].text, "제 번호는 [전화번호] 입니다");
    }

    /// 앱이 하는 대로: 잠금 없이 훑고 하나씩 지우기
    fn expire_audio(dir: &std::path::Path, keep_days: u32, today: chrono::NaiveDate, skip: Option<&str>) -> usize {
        expired(dir, keep_days, today).iter().filter(|p| expire_one(dir, p, keep_days, today, skip)).count()
    }

    #[test]
    fn expires_only_old_audio() {
        let dir = std::env::temp_dir().join(format!("bd-expire-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |id: &str, day: &str, audio: Option<&str>| {
            let d: SessionDoc = serde_json::from_value(serde_json::json!({
                "version": 1, "id": id, "title": id, "startedAt": format!("{day} 10:00"), "audioFile": audio,
                "utterances": [{ "id": 1, "startMs": 0, "endMs": 1000, "speaker": 0, "text": "안녕" }]
            }))
            .unwrap();
            std::fs::write(dir.join(format!("{id}.json")), serde_json::to_string(&d).unwrap()).unwrap();
            // 다른 폴더를 가리키는 이름("../x.opus")은 파일을 만들지 않는다 — 지우지 않는지만 본다
            if let Some(a) = audio.filter(|a| !a.contains('/')) {
                std::fs::write(dir.join(a), b"OggS").unwrap();
            }
        };
        mk("old", "2026-10-01", Some("old.opus"));
        mk("new", "2026-11-15", Some("new.opus"));
        mk("live", "2026-09-01", Some("live.opus"));
        mk("evil", "2026-09-01", Some("../x.opus"));
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        assert_eq!(expire_audio(&dir, 0, today, None), 0, "0 이면 계속 보관");
        assert_eq!(expire_audio(&dir, 30, today, Some("live")), 1);
        assert!(!dir.join("old.opus").exists() && dir.join("new.opus").exists() && dir.join("live.opus").exists());
        let old: SessionDoc = serde_json::from_str(&std::fs::read_to_string(dir.join("old.json")).unwrap()).unwrap();
        assert_eq!((old.audio_file, old.utterances.len()), (None, 1), "녹취록은 남기고 녹음 이름만 비운다");
        assert_eq!(expire_audio(&dir, 30, today, Some("live")), 0, "두 번 지우지 않는다");

        // 녹음 파일을 지우지 못하면(재생 중 · 권한 — 여기선 같은 이름의 폴더로 흉내) 녹음 이름을 남겨 다음에 다시
        mk("stuck", "2026-09-01", None);
        std::fs::create_dir_all(dir.join("stuck.opus")).unwrap();
        let mut s: SessionDoc = serde_json::from_str(&std::fs::read_to_string(dir.join("stuck.json")).unwrap()).unwrap();
        s.audio_file = Some("stuck.opus".into());
        std::fs::write(dir.join("stuck.json"), serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(expire_audio(&dir, 30, today, Some("live")), 0);
        let s: SessionDoc = serde_json::from_str(&std::fs::read_to_string(dir.join("stuck.json")).unwrap()).unwrap();
        assert_eq!(s.audio_file.as_deref(), Some("stuck.opus"), "못 지운 녹음은 이름을 지키고");
        assert!(dir.join("stuck.opus").exists());
        let stuck = |dir: &std::path::Path| expired(dir, 30, today).iter().any(|p| p.ends_with("stuck.json"));
        assert!(stuck(&dir), "다음에 다시 잡는다");

        // 파일은 이미 없고 이름만 남은 기록은 이름만 비운다
        std::fs::remove_dir(dir.join("stuck.opus")).unwrap();
        assert_eq!(expire_audio(&dir, 30, today, Some("live")), 0, "지운 파일 수는 0");
        let s: SessionDoc = serde_json::from_str(&std::fs::read_to_string(dir.join("stuck.json")).unwrap()).unwrap();
        assert_eq!(s.audio_file, None);
        assert!(!stuck(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
