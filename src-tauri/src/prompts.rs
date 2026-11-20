//! 요약 · 회의에 묻기 프롬프트 — 글은 `src-tauri/prompts/*.md` 에 두고, 빌드할 때 실행 파일에 넣는다
//! (실행 중에 파일을 읽지 않는다. md 를 고치면 다시 빌드해야 반영된다).
//!
//! - md 내용이 그대로 모델에게 간다. 메모 · 설명은 md 에 쓰지 말고 이 파일이나 `prompts/README.md` 에
//! - `{{rules/style.md}}` 처럼 쓴 자리는 그 파일 내용으로 바뀐다 (여러 프롬프트가 같이 쓰는 규칙)
//! - 파일 끝의 줄바꿈 · 빈 줄은 떼고 보낸다
//! - 회의록(`summary.md`)의 `{형식}` · `{남은 질문 기준}` 자리는 실행 중에 요약 틀([`summary_with`])로 채운다

use std::sync::LazyLock;

/// prompts/ 아래 파일 (경로, 내용)
const FILES: &[(&str, &str)] = &[
    ("summary.md", include_str!("../prompts/summary.md")),
    ("summary_part.md", include_str!("../prompts/summary_part.md")),
    ("merge_notes.md", include_str!("../prompts/merge_notes.md")),
    ("merge_final.md", include_str!("../prompts/merge_final.md")),
    ("shrink_note.md", include_str!("../prompts/shrink_note.md")),
    ("ask.md", include_str!("../prompts/ask.md")),
    // 모든 회의에 묻기 — 여러 회의를 번호로 실어 보낸다. 근거는 [회의 2 · 12:34] (화면이 그 회의의 그 문장으로)
    ("ask_all.md", include_str!("../prompts/ask_all.md")),
    // 지금까지 따라잡기 — 녹음 중 "지난 10분"만 실어 묻는 지시 (사용자 글). `{범위}` 자리는 ask.rs 가
    // "지난 10분([32:10]부터) 동안" · "회의 처음부터 지금까지" 로 채운다. 지금 하는 얘기를 맨 앞에 —
    // 늦게 들어온 사람이 바로 대화에 끼어들 수 있게
    ("catch_up.md", include_str!("../prompts/catch_up.md")),
    // 회의록 문체 — 서술형 문장이 아니라 개조식으로 가볍게. 회의록을 받아 본 사람의 피드백(2026-11-19):
    // 불릿마다 "~은 ~라고 말했습니다. 또 ~했습니다"처럼 두세 문장씩 풀어 써서 무겁고 읽기 느리다.
    // 화자는 앞("김 팀장:")이 아니라 요점 끝("(김 팀장)")에 — 요점이 먼저 읽히게 (2026-11-19 사수님 요청)
    ("rules/style.md", include_str!("../prompts/rules/style.md")),
    // "남은 질문" 기준. 꼼꼼한 모델일수록 다른 데서 결론 난 것 · 스쳐 간 이야기 · 음성 인식 잡음까지
    // "확인이 필요하다"며 끌어올려(67분 인수인계 회의에서 8개, 대부분 확인할 필요가 없었다) 좁혀 둔다
    ("rules/open_questions.md", include_str!("../prompts/rules/open_questions.md")),
    // 구간 메모를 합칠 때: 앞 구간의 남은 질문이 뒤 구간에서 풀렸으면 빼게 한다
    ("rules/drop_resolved.md", include_str!("../prompts/rules/drop_resolved.md")),
    // 요약 불릿 끝의 근거 시각 [12:34] — 화면에서 눌러 녹취록 그 문장으로 간다 (회의에 묻기와 같은 표기).
    // 긴 회의는 구간 메모에도 달게 해, 메모를 합치고 줄여도 시각이 회의록까지 살아남게 한다
    ("rules/evidence_time.md", include_str!("../prompts/rules/evidence_time.md")),
    // 중요 표시 ⭐ — 회의 중 단축키로 표시한 문장은 녹취록에 "⭐ 문장"으로 간다(SessionDoc::to_ai_text).
    // 요약이 꼭 다루고 그 불릿에도 ⭐ 를 달게 해, 긴 회의의 구간 메모를 합치고 줄여도 남게 한다
    ("rules/starred.md", include_str!("../prompts/rules/starred.md")),
    // 회의 종류별 요약 틀 — 회의록의 "### 제목" 뒤에 오는 형식 (templates.rs 의 기본 틀 목록이 고른다).
    // 일반 회의(meeting)는 틀이 생기기 전 회의록 형식 그대로
    ("templates/meeting.md", include_str!("../prompts/templates/meeting.md")),
    ("templates/one_on_one.md", include_str!("../prompts/templates/one_on_one.md")),
    ("templates/interview.md", include_str!("../prompts/templates/interview.md")),
    ("templates/lecture.md", include_str!("../prompts/templates/lecture.md")),
    ("templates/customer.md", include_str!("../prompts/templates/customer.md")),
    ("templates/ideas.md", include_str!("../prompts/templates/ideas.md")),
];

/// md 를 보낼 글로: `{{다른 파일}}` 자리를 그 파일 내용으로 바꾸고 끝의 줄바꿈을 뗀다
fn build(name: &str) -> String {
    let (_, src) = FILES.iter().find(|f| f.0 == name).unwrap_or_else(|| panic!("프롬프트 파일이 없음: {name}"));
    let mut text = src.to_string();
    for (other, body) in FILES {
        let tag = format!("{{{{{other}}}}}");
        if text.contains(&tag) {
            text = text.replace(&tag, body.trim_end());
        }
    }
    text.trim_end().to_string()
}

macro_rules! prompt {
    ($(#[$doc:meta])* $name:ident = $file:literal) => {
        $(#[$doc])*
        pub fn $name() -> &'static str {
            static TEXT: LazyLock<String> = LazyLock::new(|| build($file));
            TEXT.as_str()
        }
    };
}

/// 이름으로 고르는 파일 (요약 틀 형식처럼 코드가 목록에서 고르는 것)
pub fn file(name: &str) -> &'static str {
    static ALL: LazyLock<std::collections::HashMap<&'static str, String>> =
        LazyLock::new(|| FILES.iter().map(|f| (f.0, build(f.0))).collect());
    ALL.get(name).map(String::as_str).unwrap_or_else(|| panic!("프롬프트 파일이 없음: {name}"))
}

/// 회의록 지시 — `format` 은 요약 틀의 형식("### 섹션" 목록, 제목은 빼고). "남은 질문" 섹션이 있는 틀에만
/// 남은 질문 기준을 넣는다 (없는 틀에 넣으면 쓸데없이 길고, 모델이 없는 섹션을 만들기도 한다)
/// (짧은 회의는 녹취록을 한 번에, 긴 회의는 구간 메모를 합칠 때)
pub fn summary_with(format: &str) -> String {
    let open = format.lines().any(|l| l.trim_start_matches('#').trim() == "남은 질문" && l.starts_with("###"));
    let base = file("summary.md");
    let base = if open {
        base.replace("{남은 질문 기준}", open_questions())
    } else {
        base.replace("{남은 질문 기준}

", "")
    };
    base.replace("{형식}", format.trim_end())
}

/// 일반 회의 틀의 회의록 지시 (틀이 생기기 전과 같은 글)
pub fn summary() -> &'static str {
    static TEXT: LazyLock<String> = LazyLock::new(|| summary_with(file("templates/meeting.md")));
    TEXT.as_str()
}
prompt!(
    /// "남은 질문" 기준 (그 섹션이 있는 틀의 회의록 · 구간 메모에 들어간다)
    open_questions = "rules/open_questions.md"
);
prompt!(
    /// 긴 회의: 구간 하나를 나중에 합칠 메모로 (구간 메모 · 메모 합치기 · 줄이기에 같이 쓴다)
    summary_part = "summary_part.md"
);
prompt!(
    /// 메모 묶음을 하나의 메모로 합치라는 지시 (사용자 글 앞머리)
    merge_notes = "merge_notes.md"
);
prompt!(
    /// 구간 메모를 모두 합쳐 회의록으로 만들라는 지시 (사용자 글 앞머리)
    merge_final = "merge_final.md"
);
prompt!(
    /// 너무 큰 구간 메모를 절반으로 줄이라는 지시 (사용자 글 앞머리)
    shrink_note = "shrink_note.md"
);
prompt!(
    /// 회의에 묻기
    ask = "ask.md"
);
prompt!(
    /// 모든 회의에 묻기
    ask_all = "ask_all.md"
);
prompt!(
    /// 지금까지 따라잡기 — 실린 구간을 정리하라는 지시 (사용자 글, `{범위}` 자리를 채워 보낸다)
    catch_up = "catch_up.md"
);
#[cfg(test)]
prompt!(
    /// 다른 구간에서 풀린 남은 질문은 빼라는 규칙 (merge_notes · merge_final 에 들어 있다)
    drop_resolved = "rules/drop_resolved.md"
);

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<(&'static str, String)> {
        FILES.iter().map(|f| (f.0, build(f.0))).collect()
    }

    #[test]
    fn every_placeholder_is_filled() {
        for (name, text) in all() {
            assert!(!text.contains("{{") && !text.contains("}}"), "{name}: 바뀌지 않은 {{{{…}}}} 자리가 남음");
        }
    }

    #[test]
    fn no_windows_line_endings() {
        // 체크아웃 설정에 따라 CRLF 로 바뀌면 모델에게 \r 이 섞여 간다 (.gitattributes 로 LF 고정)
        for (name, text) in all() {
            assert!(!text.contains('\r'), "{name}: CRLF 줄바꿈");
        }
    }

    #[test]
    fn shared_rules_are_included() {
        assert!(summary().contains("개조식으로"), "회의록에 문체 규칙");
        assert!(summary().contains("\"남은 질문\"에는"), "회의록에 남은 질문 기준");
        assert!(summary_part().contains("\"남은 질문\"에는"), "구간 메모에 남은 질문 기준");
        assert!(merge_notes().ends_with(drop_resolved()));
        assert!(merge_final().ends_with(drop_resolved()));
        // 규칙 뒤 빈 줄이 그대로 (규칙 끝 줄바꿈을 떼고 md 의 빈 줄로 잇는다)
        assert!(summary().contains("되풀이하지 마세요.\n\n\"남은 질문\"에는"));
    }

    #[test]
    fn evidence_times_survive_every_step() {
        // 회의록 · 구간 메모 모두 근거 시각을 달고, 메모를 합치거나 줄여도 시각을 지우지 않는다
        for (name, text) in [("summary", summary()), ("summary_part", summary_part())] {
            assert!(text.contains("근거가 된 발언의 녹취록 시각"), "{name}: 근거 시각 규칙");
            assert!(text.contains("[시각]"), "{name}: 형식에 시각 자리");
        }
        for (name, text) in [("merge_notes", merge_notes()), ("merge_final", merge_final()), ("shrink_note", shrink_note())] {
            assert!(text.contains("시각 [12:34]"), "{name}: 메모의 시각을 그대로 옮기라는 말");
        }
        // 화면이 누를 수 있는 표기 — 회의에 묻기와 같은 [분:초]
        assert!(ask().contains("[12:34]"));
    }

    #[test]
    fn catch_up_has_one_range_slot() {
        // 따라잡기 지시는 회의에 묻기 시스템 프롬프트(근거 시각 · 화자 이름 규칙)와 함께 간다 — 채울 자리는 하나
        assert_eq!(catch_up().matches("{범위}").count(), 1);
        assert!(catch_up().contains("**지금**"));
    }

    #[test]
    fn starred_parts_survive_every_step() {
        // 회의록 · 구간 메모 모두 ⭐ 표시한 발언을 꼭 다루고, 메모를 합치거나 줄여도 ⭐ 줄을 지우지 않는다
        for (name, text) in [("summary", summary()), ("summary_part", summary_part())] {
            assert!(text.contains("⭐ 바로 뒤의 문장은"), "{name}: 중요 표시 규칙");
        }
        for (name, text) in [("merge_notes", merge_notes()), ("merge_final", merge_final()), ("shrink_note", shrink_note())] {
            assert!(text.contains("⭐ 가 붙은 줄"), "{name}: ⭐ 줄을 지키라는 말");
        }
    }
}
