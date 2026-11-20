//! 요약 불릿 끝의 말꼬리 떼기 — "~방안 검토 필요 제기 [14:07]" → "~방안 검토 [14:07]"
//!
//! 문체 규칙(`prompts/rules/style.md`)으로 꼬리를 붙이지 말라고 해도 gpt-5-mini 같은 모델은 실행마다 0~11개를
//! 남기고, 한 낱말을 막으면 비슷한 낱말로 바꿔 쓴다("제안" → "제시"). 그래서 요약이 끝난 뒤 줄 끝 낱말을 한 번 더 뗀다.
//!
//! - 떼는 곳: 한 줄 요약과 섹션마다의 목록 줄. 줄 끝 근거 시각 `[12:34]` · ⭐ · 화자 괄호 "(김 팀장)" 와
//!   할 일의 "— 기한: …"(요약 보내기 · 일정 파일이 읽는다)은 그대로 두고 그 앞에서 뗀다.
//! - 굵은 주제 줄(`- **주제**`) · 강의 틀의 용어 풀이는 건드리지 않는다.
//! - 할 일처럼 할 것을 적는 섹션에서는 "공유 · 보고 · 논의 · 확정" 이 할 일 그 자체라 떼지 않는다 ("견적서 : 고객사 공유").
//! - 떼면 뜻이 끊기는 곳에서 멈춘다: 남은 끝이 꾸미는 말("추가 논의" · "팀 내 공유")이거나 조사("영업팀과 논의")일 때.
//! - 다 떼어지거나 "대상 :" 만 남으면 그 줄은 그대로 둔다.
//!
//! 요약이 다 끝난 뒤 한 번에 뗀다 — 요약하는 동안 화면에는 진행(섹션 · 막대)만 가고 글은 가지 않아 깜빡이지 않는다.

/// 어느 섹션에서든 떼는 꼬리 — 누가 무엇을 말했다는 말하기 낱말 · 추진
const ALWAYS: &[&str] = &["제기", "언급", "지적", "우려", "의견", "권장", "권고", "추진"];
/// 상태 꼬리 — 어느 섹션에서든 떼고, 남은 끝이 꾸미는 말이어도 뗀다 ("2명 추가 필요" → "2명 추가")
const STATE: &[&str] = &["필요", "필요성", "예정", "중", "상태"];
/// 말하기 낱말이지만 할 일 · 정한 것에서는 그 자체가 할 일 · 정한 내용이라 남긴다 ("결과 : 매주 월요일 공유")
const ACTS: &[&str] = &["제안", "제시", "논의", "공유", "보고"];
/// 정해졌다는 꼬리 — 할 일 · 남은 질문에서는 할 일 그 자체라 남긴다 ("- [ ] 출시일 확정")
const SETTLED: &[&str] = &["확정", "합의"];
/// 한 줄 요약에서만 ("~ 방향" · "~ 계획 수립")
const ONE_LINE_ONLY: &[&str] = &["방향", "수립"];

/// 떼고 남은 끝이 이 말이면 뗀 꼬리가 그 짝이었다 — 떼면 "추가" · "팀 내" 처럼 끊긴 채 끝난다
const DANGLING: &[&str] = &[
    "추가", "추후", "향후", "별도", "따로", "재", "다시", "계속", "지속", "지속적", "상세", "구체적", "심층", "본격", "적극",
    "적극적", "사전", "함께", "같이", "우선", "먼저", "직접", "즉시", "바로", "긴급", "정기", "정기적", "수시", "매주", "매일",
    "매월", "주간", "월간", "일일", "중간", "최종", "잠정", "공식", "내부", "공동", "전체", "간", "내", "중", "후", "전", "시",
    "및", "등", "또는", "관련", "대한", "위한", "통한", "대해", "위해", "통해",
];
/// 떼고 남은 끝 낱말이 이 조사로 끝나면 뗀 꼬리가 서술어였다 ("영업팀과 논의" · "고객사에 공유" · "방안을 제시")
const PARTICLES: &[&str] = &["에게", "께", "한테", "에서", "으로", "와", "과", "에", "로", "을", "를"];
/// 할 일 줄의 담당 · 기한 칸 — 이 앞에서 뗀다 (share.rs 가 읽는 꼴)
const FIELDS: &[&str] = &["— 기한", "— 담당", "– 기한", "– 담당", "- 기한", "- 담당", " 기한:", " 담당:", " 기한 :", " 담당 :"];

/// 섹션마다 뗄 꼬리
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// 건드리지 않음 — 첫 섹션 앞 · 제목 · 용어 풀이
    Keep,
    /// 한 줄 요약 — 목록 줄이 아니어도, 꼬리 모두 + 방향 · 수립
    OneLine,
    /// 할 일 · 남은 질문 · 다음 단계 · 더 알아볼 것 · 우려 · 더 확인할 것처럼 할 것 · 챙길 것 —
    /// 할 일 낱말([`ACTS`] · [`SETTLED`])은 남긴다
    Todo,
    /// 결정 사항 · 합의한 것 · 정한 것 — 정한 내용이 될 수 있는 [`ACTS`] 는 남긴다
    Decided,
    /// 그 밖 (주요 논의 · 요약 틀의 본문 섹션 · 내 틀) — 꼬리 모두
    Notes,
}

fn kind_of(section: &str) -> Kind {
    let s = section.trim();
    if s == "제목" || s.contains("용어") {
        Kind::Keep
    } else if s == "한 줄 요약" {
        Kind::OneLine
    } else if s.contains("할 일") || s.contains("남은 질문") || s.starts_with("다음") || s.contains("더 ") {
        Kind::Todo
    } else if s.contains("결정") || s.contains("합의") || s.contains("정한") {
        Kind::Decided
    } else {
        Kind::Notes
    }
}

impl Kind {
    fn cuts(self, word: &str) -> bool {
        let has = |list: &[&str]| list.contains(&word);
        match self {
            Kind::Keep => false,
            Kind::OneLine => has(ALWAYS) || has(STATE) || has(ACTS) || has(SETTLED) || has(ONE_LINE_ONLY),
            Kind::Notes => has(ALWAYS) || has(STATE) || has(ACTS) || has(SETTLED),
            Kind::Decided => has(ALWAYS) || has(STATE) || has(SETTLED),
            Kind::Todo => has(ALWAYS) || has(STATE),
        }
    }
}

/// 요약 본문("### 섹션" 들)의 말꼬리를 뗀다 → (고친 본문, 고친 줄 수)
pub fn trim(text: &str) -> (String, usize) {
    let mut kind = Kind::Keep;
    let mut changed = 0;
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with('#') {
            kind = kind_of(t.trim_start_matches('#'));
            out.push(line.to_string());
            continue;
        }
        match trim_line(line, kind) {
            Some(new) => {
                changed += 1;
                out.push(new);
            }
            None => out.push(line.to_string()),
        }
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    (joined, changed)
}

/// 줄 하나 → 고쳤으면 Some
fn trim_line(line: &str, kind: Kind) -> Option<String> {
    if kind == Kind::Keep || line.trim().is_empty() {
        return None;
    }
    let (lead, body) = match bullet(line) {
        Some(x) => x,
        // 한 줄 요약은 목록 줄이 아니어도
        None if kind == Kind::OneLine => line.split_at(line.len() - line.trim_start().len()),
        None => return None,
    };
    trim_body(body, kind).map(|b| format!("{lead}{b}"))
}

/// "  - [ ] 본문" → ("  - [ ] ", "본문"). 목록 줄이 아니면 None
fn bullet(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start();
    let after = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")).or_else(|| {
        let (n, r) = rest.split_once(". ")?;
        (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then_some(r)
    })?;
    let after = ["[ ] ", "[x] ", "[X] "].iter().find_map(|c| after.strip_prefix(c)).unwrap_or(after);
    Some(line.split_at(line.len() - after.len()))
}

fn trim_body(body: &str, kind: Kind) -> Option<String> {
    // 할 일의 "— 기한: …" · "— 담당: …" 은 그대로 두고 그 앞에서
    if let Some(i) = FIELDS.iter().filter_map(|f| body.find(f)).min() {
        let (head, field) = body.split_at(i);
        let core = head.trim_end();
        let gap = &head[core.len()..];
        return cut_words(core, kind).map(|c| format!("{c}{gap}{field}"));
    }
    let core = body[..tail_start(body)].trim_end();
    let tail = &body[core.len()..];
    // 굵은 주제 줄
    let bare = core.trim_end_matches([':', '：']);
    if bare.starts_with("**") && bare.ends_with("**") {
        return None;
    }
    cut_words(core, kind).map(|c| format!("{c}{tail}"))
}

/// 줄 끝의 근거 시각 `[12:34]`(여러 개) · ⭐ · 화자 괄호 "(김 팀장)" 이 시작하는 곳
fn tail_start(body: &str) -> usize {
    let mut s = body.trim_end();
    loop {
        if let Some(r) = s.strip_suffix('\u{fe0f}').unwrap_or(s).strip_suffix('⭐') {
            s = r.trim_end();
            continue;
        }
        if let Some((i, inner)) = s.strip_suffix(']').and_then(|r| r.rfind('[').map(|i| (i, &r[i + 1..]))) {
            if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit() || c == ':') {
                s = s[..i].trim_end();
                continue;
            }
        }
        break;
    }
    // 화자 괄호 — 앞이 띄어져 있을 때만 (낱말에 붙은 "수(예: 3개)" 는 본문)
    if s.ends_with(')') {
        if let Some(i) = open_paren(s) {
            if s[..i].ends_with(' ') {
                s = s[..i].trim_end();
            }
        }
    }
    s.len()
}

/// 끝의 ')' 와 짝인 '(' 의 자리
fn open_paren(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// 끝에서부터 꼬리 낱말을 뗀다 → 바뀌었으면 Some. 다 떼어지거나 "대상 :" 만 남으면 None (그대로 둔다)
fn cut_words(core: &str, kind: Kind) -> Option<String> {
    // 시각 앞에 홀로 남은 줄표 ("~ 교체 — [12:53]")
    let mut cur = core.trim_end_matches([' ', '—', '–']);
    loop {
        let (rest, last) = cur.rsplit_once(' ').unwrap_or(("", cur));
        let word = last.trim_end_matches(['.', ',']);
        if !kind.cuts(word) {
            break;
        }
        let rest = rest.trim_end();
        // 나열의 한 항목("등급 : 상 / 중" · "검토 · 논의")이면 꼬리가 아니라 내용이다
        if rest.ends_with([',', '·', '/', '—', '–', '-']) {
            break;
        }
        if rest.is_empty() || rest.ends_with(':') || rest.ends_with('：') {
            return None;
        }
        let prev = rest.rsplit(' ').next().unwrap_or(rest);
        let particle = prev.chars().count() >= 2 && PARTICLES.iter().any(|p| prev.ends_with(p));
        if particle || (!STATE.contains(&word) && DANGLING.contains(&prev)) {
            break;
        }
        cur = rest;
    }
    (cur.len() != core.len()).then(|| cur.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 섹션 하나의 줄들을 떼어 본다
    fn cut(section: &str, lines: &str) -> String {
        let (out, _) = trim(&format!("### {section}\n{lines}"));
        out.split_once('\n').map(|x| x.1.to_string()).unwrap_or_default()
    }

    #[test]
    fn talk_and_need_tails_are_cut_before_the_time() {
        // 실제 요약(gpt-5-mini)에 남았던 줄들
        for (from, to) in [
            ("  - 메일 : 발송 시각 변경 방안 검토 필요 제기 [14:07]", "  - 메일 : 발송 시각 변경 방안 검토 [14:07]"),
            ("  - 익명화 요건 : 가맹점 3개 이상·익명화 수준 필요성 언급 [21:46]", "  - 익명화 요건 : 가맹점 3개 이상·익명화 수준 [21:46]"),
            ("  - 프롬프트 분리 : MD 파일로 분리해 가변 변수만 교체하는 방식 제시 [12:53]", "  - 프롬프트 분리 : MD 파일로 분리해 가변 변수만 교체하는 방식 [12:53]"),
            ("  - 계산 위치 비교 : 서버 배치·권한 이슈 논의 [12:53] [14:07]", "  - 계산 위치 비교 : 서버 배치·권한 이슈 [12:53] [14:07]"),
            ("  - VPN 도입 검토 : 트래픽별 과금 영향 검토 중 [28:15]", "  - VPN 도입 검토 : 트래픽별 과금 영향 검토 [28:15]"),
            ("  - 클라이언트 부하 : 브라우저 성능 영향 우려 [14:07]", "  - 클라이언트 부하 : 브라우저 성능 영향 [14:07]"),
            // 시각 앞에 홀로 남은 줄표도
            ("  - 프롬프트 : MD 파일로 분리해 가변 변수만 교체 — [12:53]", "  - 프롬프트 : MD 파일로 분리해 가변 변수만 교체 [12:53]"),
            // ⭐ · 시각 없는 줄 · 마침표
            ("  - ⭐ 결제 모듈 : 분리 관리 필요 [03:10]", "  - ⭐ 결제 모듈 : 분리 관리 [03:10]"),
            ("  - 결제 모듈 : 분리 관리 필요 ⭐ [03:10]", "  - 결제 모듈 : 분리 관리 ⭐ [03:10]"),
            ("- 로그 : 수집 일원화 필요성 언급.", "- 로그 : 수집 일원화"),
        ] {
            assert_eq!(cut("주요 논의", from), to);
        }
    }

    #[test]
    fn what_is_left_alone() {
        for line in [
            "- **출시 일정 논의**",
            "- **출시 일정 논의**:",
            "- 없음",
            "  - 결제 모듈 : 분리 관리 [03:10]",
            // 다 떼어지거나 대상만 남으면 그대로
            "  - 논의 필요 [01:00]",
            "  - 서버 : 논의 필요 [01:00]",
            // 떼면 뜻이 끊기는 곳
            "  - 가격 정책 : 추가 논의 [10:00]",
            "  - 회의록 : 팀 내 공유 [10:00]",
            "  - 결제 관련 논의 [10:00]",
            "  - 단가 : 협력사와 논의 [05:00]",
            "  - 견적서 : 고객사에 공유 [05:00]",
            // 낱말 가운데 · 붙여 쓴 꼬리는 낱말이 아니다
            "  - 로그 : 재논의 [10:00]",
            "  - 로그 : 검토·논의 [10:00]",
            // 나열의 한 항목
            "  - 후보 등급 : 상 / 중 [03:10]",
            "  - 로그 : 검토 · 논의 [10:00]",
            "  - 진행 : 완료, 중 [10:00]",
        ] {
            assert_eq!(cut("주요 논의", line), line);
        }
        // 첫 섹션 앞 · 제목 · 용어 풀이
        assert_eq!(trim("- 검토 필요 [01:00]").0, "- 검토 필요 [01:00]");
        assert_eq!(cut("제목", "출시 일정 논의"), "출시 일정 논의");
        assert_eq!(cut("용어 · 개념", "- **합의 알고리즘** — 노드 간 상태 합의 [10:00]"), "- **합의 알고리즘** — 노드 간 상태 합의 [10:00]");
        // 목록 줄이 아닌 본문 줄
        assert_eq!(cut("주요 논의", "검토 필요 [01:00]"), "검토 필요 [01:00]");
    }

    #[test]
    fn stops_where_cutting_more_would_break_the_meaning() {
        assert_eq!(cut("주요 논의", "  - 가격 정책 : 추가 논의 필요 [10:00]"), "  - 가격 정책 : 추가 논의 [10:00]");
        // 상태 꼬리는 꾸미는 말 앞에서도 뗀다
        assert_eq!(cut("주요 논의", "  - QA : 2명 추가 필요 [10:00]"), "  - QA : 2명 추가 [10:00]");
        assert_eq!(cut("주요 논의", "  - 고객사에 필요 [10:00]"), "  - 고객사에 필요 [10:00]");
    }

    #[test]
    fn todo_keeps_its_due_and_its_own_verbs() {
        assert_eq!(
            cut("할 일", "- [ ] 결제 모듈 : 분리 검토 필요 — 기한: 다음 주 예정 [14:07]"),
            "- [ ] 결제 모듈 : 분리 검토 — 기한: 다음 주 예정 [14:07]",
            "기한 칸은 그대로"
        );
        assert_eq!(cut("할 일", "- [ ] 기본 구조 : 기본 아키텍처 수정 추진 [33:46]"), "- [ ] 기본 구조 : 기본 아키텍처 수정 [33:46]");
        assert_eq!(cut("할 일", "- [x] 시안 : 2종 작성 예정 [00:17]"), "- [x] 시안 : 2종 작성 [00:17]");
        // 다른 틀의 담당 칸
        assert_eq!(
            cut("할 일", "- [ ] 견적서 작성 필요 — 담당: 이 개발 / 기한: 금요일 [00:07]"),
            "- [ ] 견적서 작성 — 담당: 이 개발 / 기한: 금요일 [00:07]"
        );
        // 할 일 그 자체인 낱말
        for line in ["- [ ] 견적서 : 고객사 공유 [00:10]", "- [ ] 실적 : 팀장 보고", "- [ ] 출시일 확정 — 기한: 금요일", "- [ ] 단가 : 협력사 논의"] {
            assert_eq!(cut("할 일", line), line);
        }
        assert_eq!(cut("남은 질문", "- 가격 정책 : 영업팀 논의 필요 [10:00]"), "- 가격 정책 : 영업팀 논의 [10:00]");
        assert_eq!(cut("다음 단계", "- 데모 : 다음 주 공유 [10:00]"), "- 데모 : 다음 주 공유 [10:00]");
        // 면접 틀의 "우려 · 더 확인할 것" 도 챙길 것
        assert_eq!(cut("우려 · 더 확인할 것", "- 처우 : 입사일 확정 필요 [03:10]"), "- 처우 : 입사일 확정 [03:10]");
        assert_eq!(cut("더 알아볼 것", "- 단가 : 협력사 논의 [05:00]"), "- 단가 : 협력사 논의 [05:00]");
    }

    #[test]
    fn decisions_drop_settled_but_keep_what_was_decided() {
        assert_eq!(cut("결정 사항", "- 출시 일정 : 다음 달 확정 [00:25]"), "- 출시 일정 : 다음 달 [00:25]");
        assert_eq!(cut("결정 사항", "- 테스트 : 12월 중 진행 예정 — [27:47]"), "- 테스트 : 12월 중 진행 [27:47]");
        assert_eq!(cut("합의한 것", "- 단가 : 10% 인하 합의 [12:00]"), "- 단가 : 10% 인하 [12:00]");
        assert_eq!(cut("결정 사항", "- 결과 : 매주 월요일 공유 [00:30]"), "- 결과 : 매주 월요일 공유 [00:30]");
    }

    #[test]
    fn one_line_summary_also_drops_direction_words() {
        assert_eq!(cut("한 줄 요약", "출시 일정 · 결제 모듈 분리 방향 수립"), "출시 일정 · 결제 모듈 분리");
        assert_eq!(cut("한 줄 요약", "AI 리포트 구조 · 데이터 전략 합의"), "AI 리포트 구조 · 데이터 전략");
        assert_eq!(cut("한 줄 요약", "- 출시 일정 · 로그 일원화 논의"), "- 출시 일정 · 로그 일원화");
        // 다른 섹션에서는 방향 · 수립을 떼지 않는다
        assert_eq!(cut("주요 논의", "  - 예측 모델 : 다음달 매출 예측 방향 [15:30]"), "  - 예측 모델 : 다음달 매출 예측 방향 [15:30]");
    }

    #[test]
    fn speaker_brackets_stay_at_the_end() {
        // 면접 · 강의 · 아이디어 틀처럼 화자 자리가 있는 줄
        assert_eq!(cut("질문과 답", "  - Rust 백엔드 개발 중 (지원자) [03:10]"), "  - Rust 백엔드 개발 (지원자) [03:10]");
        assert_eq!(
            cut("질의응답", "- **Q.** 캐시 범위 — **A.** 화면 단위 캐시 권장 (김 강사) [40:02]"),
            "- **Q.** 캐시 범위 — **A.** 화면 단위 캐시 (김 강사) [40:02]"
        );
        assert_eq!(
            cut("나온 아이디어", "- **매장 알림** — 재고 부족 시 푸시 알림 제안 (이 개발) [05:00]"),
            "- **매장 알림** — 재고 부족 시 푸시 알림 (이 개발) [05:00]"
        );
        // 낱말에 붙은 괄호는 본문
        assert_eq!(
            cut("주요 논의", "  - 익명화 기준 : 최소 집계 수(예: 3개 이상) 필요성 언급 [21:46]"),
            "  - 익명화 기준 : 최소 집계 수(예: 3개 이상) [21:46]"
        );
        assert_eq!(cut("고객 반응 · 우려", "- 가격 : 경쟁사 대비 비쌈 우려 [11:00]"), "- 가격 : 경쟁사 대비 비쌈 [11:00]");
    }

    #[test]
    fn whole_summary_counts_changed_lines() {
        let text = "### 한 줄 요약\n출시 일정 · 결제 분리 합의\n\n### 주요 논의\n- **출시 일정**\n  - 15일 출시 · 목요일 점검 필요 [00:25]\n  - 앱 심사 : 2일 소요 [00:30]\n\n### 결정 사항\n- 출시일 : 15일 확정 [00:25]\n\n### 할 일\n- [ ] 결제 모듈 배포 — 기한: 금요일 [00:07]\n\n### 남은 질문\n- 없음\n";
        let (out, n) = trim(text);
        assert_eq!(n, 3);
        assert_eq!(
            out,
            "### 한 줄 요약\n출시 일정 · 결제 분리\n\n### 주요 논의\n- **출시 일정**\n  - 15일 출시 · 목요일 점검 [00:25]\n  - 앱 심사 : 2일 소요 [00:30]\n\n### 결정 사항\n- 출시일 : 15일 [00:25]\n\n### 할 일\n- [ ] 결제 모듈 배포 — 기한: 금요일 [00:07]\n\n### 남은 질문\n- 없음\n"
        );
        assert_eq!(out.lines().count(), text.lines().count(), "줄 수는 그대로");
    }
}
