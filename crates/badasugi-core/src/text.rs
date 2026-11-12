//! 인식 결과 후처리: 공백 정리, Whisper 의 한국어 환각 문구 제거, 반복 루프 축약.

/// 무음·잡음에서 Whisper 가 자주 지어내는 문구 (유튜브 자막 학습 흔적)
const HALLUCINATIONS: &[&str] = &[
    "시청해주셔서 감사합니다",
    "시청해 주셔서 감사합니다",
    "시청 해주셔서 감사합니다",
    "구독과 좋아요",
    "구독 좋아요",
    "좋아요와 구독",
    "알림 설정",
    "MBC 뉴스",
    "KBS 뉴스",
    "SBS 뉴스",
    "YTN 뉴스",
    "뉴스 김",
    "자막 제공",
    "자막 제작",
    "한글자막",
    "한글 자막",
    "다음 영상에서 만나요",
    "Thank you for watching",
    "Thanks for watching",
    "ご視聴ありがとうございました",
];

pub fn clean(raw: &str) -> String {
    let mut s = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    // [음악], (박수) 같은 비언어 표기 제거
    s = strip_brackets(&s);
    s = collapse_repeats(&s);
    s = dedupe_sentences(&s);
    let s = s.trim().trim_start_matches(['-', '·']).trim().to_string();

    // 환각 문구가 들어간 문장은 통째로 뺀다 (학습 데이터의 자막 크레딧 흔적)
    let kept: Vec<String> = sentences(&s)
        .into_iter()
        .filter(|sent| {
            let compact: String = sent.chars().filter(|c| !c.is_whitespace()).collect();
            !HALLUCINATIONS.iter().any(|h| {
                let hc: String = h.chars().filter(|c| !c.is_whitespace()).collect();
                compact.contains(&hc)
            })
        })
        .collect();
    let s = kept.join(" ");
    if s.chars().all(|c| !c.is_alphanumeric()) {
        return String::new();
    }
    s
}

/// `[음악]`, `(웃음)`, `♪` 같은 비언어 표기를 지운다. 괄호 안이 길면 실제 내용일 수 있어 남긴다.
fn strip_brackets(s: &str) -> String {
    let chars: Vec<char> = s.chars().filter(|&c| c != '♪').collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let close = match c {
            '[' => Some(']'),
            '(' => Some(')'),
            _ => None,
        };
        if let Some(close) = close {
            if let Some(j) = chars[i + 1..].iter().position(|&x| x == close) {
                let inner = j;
                if c == '[' || inner <= 6 {
                    i += j + 2;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 문장 단위로 쪼갠다 (마침표·물음표·느낌표 뒤)
fn sentences(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        // "0.4초" 같은 소수점은 문장 끝이 아니다: 뒤가 공백이거나 끝일 때만 자른다
        let end = chars.get(i + 1).is_none_or(|n| n.is_whitespace());
        if matches!(c, '.' | '?' | '!' | '。') && end {
            out.push(cur.trim().to_string());
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// 같은 문장이 연달아 나오면 한 번만 남긴다. 끝에 잘린 반복(앞 문장의 접두어)도 지운다.
fn dedupe_sentences(s: &str) -> String {
    let parts = sentences(s);
    if parts.len() < 2 {
        return s.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        let n = norm(&p);
        if n.is_empty() {
            continue;
        }
        if let Some(prev) = out.last() {
            let pn = norm(prev);
            if pn == n || (n.chars().count() >= 4 && pn.starts_with(&n)) {
                continue;
            }
        }
        out.push(p);
    }
    out.join(" ")
}

/// 디코딩이 반복 루프에 빠진 결과인지 (다시 인식할지 판단)
/// 한국어로 받아 적는데 한글이 하나도 없고 영문만 나온 경우 (줄인 인코더 창에서 가끔 생기는 엉뚱한 영어 출력)
///
/// "API 연동" 처럼 섞인 건 정상으로 보고, "OK" 같은 아주 짧은 영어도 넘긴다.
pub fn is_wrong_script(s: &str, lang: &str) -> bool {
    if lang != "ko" {
        return false;
    }
    let hangul = s
        .chars()
        .filter(|c| ('\u{AC00}'..='\u{D7A3}').contains(c) || ('\u{3131}'..='\u{318E}').contains(c))
        .count();
    let latin = s.chars().filter(|c| c.is_ascii_alphabetic()).count();
    hangul == 0 && latin >= 3
}

pub fn is_degenerate(s: &str) -> bool {
    let parts = sentences(s);
    for w in parts.windows(2) {
        let (a, b) = (norm(&w[0]), norm(&w[1]));
        if !a.is_empty() && (a == b || (b.chars().count() >= 4 && a.starts_with(&b))) {
            return true;
        }
    }
    // 글자 단위 압축률: 같은 4글자 조각이 지나치게 많이 반복
    let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() >= 40 {
        let mut grams = std::collections::HashMap::new();
        for g in chars.windows(4) {
            *grams.entry(g).or_insert(0usize) += 1;
        }
        let uniq = grams.len() as f32 / (chars.len() - 3) as f32;
        if uniq < 0.45 {
            return true;
        }
    }
    false
}

/// "네 네 네 네 네 네" / "그래서 그래서 그래서 …" 같은 디코딩 루프를 2회로 줄인다
fn collapse_repeats(s: &str) -> String {
    let words: Vec<&str> = s.split(' ').collect();
    if words.len() < 6 {
        return s.to_string();
    }
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    let mut i = 0;
    'outer: while i < words.len() {
        for n in 1..=4 {
            if i + n * 4 > words.len() {
                continue;
            }
            let pat = &words[i..i + n];
            let mut reps = 1;
            while i + (reps + 1) * n <= words.len() && &words[i + reps * n..i + (reps + 1) * n] == pat {
                reps += 1;
            }
            if reps >= 4 {
                out.extend_from_slice(pat);
                out.extend_from_slice(pat);
                i += reps * n;
                continue 'outer;
            }
        }
        out.push(words[i]);
        i += 1;
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_script_only_for_korean_without_hangul() {
        assert!(is_wrong_script("and then, it's", "ko"));
        assert!(is_wrong_script("And.", "ko"));
        assert!(!is_wrong_script("OK", "ko"));
        assert!(!is_wrong_script("API 연동은 금요일까지", "ko"));
        assert!(!is_wrong_script("and then", "en"));
        assert!(!is_wrong_script("", "ko"));
    }

    #[test]
    fn removes_hallucination() {
        assert_eq!(clean("시청해주셔서 감사합니다."), "");
        assert_eq!(clean(" MBC 뉴스 이덕영입니다."), "");
        assert_eq!(clean("[음악]"), "");
        assert_eq!(clean("네, 감사합니다."), "네, 감사합니다.");
        assert_eq!(clean("한글자막 제공 및 자막 제공 및 광고를 포함하고 있습니다."), "");
        assert_eq!(clean("좋습니다. 시청해 주셔서 감사합니다."), "좋습니다.");
    }

    #[test]
    fn keeps_normal_text() {
        let t = "오늘 회의는 3분기 매출 정리부터 하겠습니다.";
        assert_eq!(clean(t), t);
        let t = "평균 2초에서 0.4초로 줄였습니다. 다음 주에 적용하겠습니다.";
        assert_eq!(clean(t), t);
    }

    #[test]
    fn dedupes_sentences() {
        let t = "먼저 지난주 진행 상황부터 공유해 주시겠어요. 먼저 지난주 진행 상황부터 공유해 주시겠어요. 먼저 지난주 진";
        assert_eq!(clean(t), "먼저 지난주 진행 상황부터 공유해 주시겠어요.");
        assert!(is_degenerate(t));
        assert!(!is_degenerate("네, 신규 결제 모듈은 개발이 끝났고 현재 테스트 서버에서 검증 중입니다."));
    }

    #[test]
    fn collapses_loops() {
        assert_eq!(clean("네 네 네 네 네 네 네 네 알겠습니다"), "네 네 알겠습니다");
    }
}
