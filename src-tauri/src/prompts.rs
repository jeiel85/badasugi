//! 요약 · 회의에 묻기 프롬프트 — 글은 `src-tauri/prompts/*.md` 에 두고, 빌드할 때 실행 파일에 넣는다
//! (실행 중에 파일을 읽지 않는다. md 를 고치면 다시 빌드해야 반영된다).
//!
//! - md 내용이 그대로 모델에게 간다. 메모 · 설명은 md 에 쓰지 말고 이 파일이나 `prompts/README.md` 에
//! - `{{rules/style.md}}` 처럼 쓴 자리는 그 파일 내용으로 바뀐다 (여러 프롬프트가 같이 쓰는 규칙)
//! - 파일 끝의 줄바꿈 · 빈 줄은 떼고 보낸다

use std::sync::LazyLock;

/// prompts/ 아래 파일 (경로, 내용)
const FILES: &[(&str, &str)] = &[
    ("summary.md", include_str!("../prompts/summary.md")),
    ("summary_part.md", include_str!("../prompts/summary_part.md")),
    ("merge_notes.md", include_str!("../prompts/merge_notes.md")),
    ("merge_final.md", include_str!("../prompts/merge_final.md")),
    ("shrink_note.md", include_str!("../prompts/shrink_note.md")),
    ("ask.md", include_str!("../prompts/ask.md")),
    // 회의록 문체 — 서술형 문장이 아니라 개조식으로 가볍게. 회의록을 받아 본 사람의 피드백(2026-11-19):
    // 불릿마다 "~은 ~라고 말했습니다. 또 ~했습니다"처럼 두세 문장씩 풀어 써서 무겁고 읽기 느리다.
    // 화자는 앞("김 팀장:")이 아니라 요점 끝("(김 팀장)")에 — 요점이 먼저 읽히게 (2026-11-19 사수님 요청)
    ("rules/style.md", include_str!("../prompts/rules/style.md")),
    // "남은 질문" 기준. 꼼꼼한 모델일수록 다른 데서 결론 난 것 · 스쳐 간 이야기 · 음성 인식 잡음까지
    // "확인이 필요하다"며 끌어올려(67분 인수인계 회의에서 8개, 대부분 확인할 필요가 없었다) 좁혀 둔다
    ("rules/open_questions.md", include_str!("../prompts/rules/open_questions.md")),
    // 구간 메모를 합칠 때: 앞 구간의 남은 질문이 뒤 구간에서 풀렸으면 빼게 한다
    ("rules/drop_resolved.md", include_str!("../prompts/rules/drop_resolved.md")),
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

prompt!(
    /// 회의록 (짧은 회의는 녹취록을 한 번에, 긴 회의는 구간 메모를 합칠 때)
    summary = "summary.md"
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
}
