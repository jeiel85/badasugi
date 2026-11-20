//! 회의 종류별 요약 틀 — 회의록 형식("### 섹션" 목록)을 틀마다 데이터로 둔다.
//!
//! - 기본 틀: 형식은 `prompts/templates/*.md` (모델에게 그대로 간다), 이름 · 설명은 [`BUILTIN`]
//! - 내 틀: 설정 › 요약에서 이름과 섹션 줄("섹션 이름" 또는 "섹션 이름 — 무엇을 적을지")로 만든다 ([`custom_format`])
//! - 모든 틀이 같이 쓰는 것: 제목 · 개조식 문체 · 근거 시각 · ⭐ · 긴 회의 구간 나누기(구간 메모 형식은 하나).
//!   "남은 질문" 기준은 그 섹션이 있는 틀에만 ([`prompts::summary_with`])
//! - 회의마다 마지막으로 쓴 틀을 기억한다(`SessionDoc::summary_template`) — 다시 요약하면 같은 틀로

use serde::Serialize;

use crate::prompts;
use crate::settings::{CustomTemplate, SummarySettings};

/// 틀을 고르지 않았거나 고른 틀이 없어졌을 때
pub const DEFAULT: &str = "meeting";

/// 기본 틀: (id, 이름, 언제 쓰나, 형식 파일)
const BUILTIN: &[(&str, &str, &str, &str)] = &[
    ("meeting", "일반 회의", "안건을 논의하고 정하는 회의", "templates/meeting.md"),
    ("one_on_one", "1:1 면담", "팀장 · 팀원 면담, 코칭", "templates/one_on_one.md"),
    ("interview", "면접 · 인터뷰", "채용 면접, 사용자 · 전문가 인터뷰", "templates/interview.md"),
    ("lecture", "강의 · 세미나", "교육 · 발표 · 세미나 — 배운 것 위주", "templates/lecture.md"),
    ("customer", "고객 미팅", "영업 · 고객 상담 · 협력사 미팅", "templates/customer.md"),
    ("ideas", "아이디어 회의", "브레인스토밍 — 나온 아이디어를 빠짐없이", "templates/ideas.md"),
];

/// 내 틀은 섹션을 이만큼까지만 (너무 많으면 한 섹션에 한두 줄씩 흩어진다)
const MAX_SECTIONS: usize = 12;
/// 섹션 이름 · 설명은 이 글자까지
const MAX_NAME: usize = 30;
const MAX_DESC: usize = 120;

/// 요약에 쓸 틀
#[derive(Debug, Clone)]
pub struct Template {
    pub id: String,
    pub name: String,
    /// 회의록 형식 — "### 제목" 뒤에 오는 "### 섹션" 목록 (제목은 모든 틀이 같이 쓴다)
    pub format: String,
}

/// 화면에 보여 줄 틀 (고르기 · 설정)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInfo {
    pub id: String,
    pub name: String,
    pub desc: String,
    pub sections: Vec<String>,
    pub custom: bool,
}

/// 형식의 섹션 이름 ("### " 줄)
pub fn section_names(format: &str) -> Vec<String> {
    format.lines().filter_map(|l| l.strip_prefix("### ")).map(|s| s.trim().to_string()).collect()
}

fn builtin(id: &str) -> Option<Template> {
    BUILTIN.iter().find(|b| b.0 == id).map(|b| Template {
        id: b.0.into(),
        name: b.1.into(),
        format: prompts::file(b.3).to_string(),
    })
}

fn custom(t: &CustomTemplate) -> Template {
    Template { id: t.id.clone(), name: name_of(t), format: custom_format(&t.sections) }
}

fn name_of(t: &CustomTemplate) -> String {
    let n: String = t.name.trim().chars().take(MAX_NAME).collect();
    if n.is_empty() {
        "내 틀".into()
    } else {
        n
    }
}

/// `id` 의 틀 (없으면 설정의 기본 틀, 그것도 없으면 일반 회의)
pub fn resolve(cfg: &SummarySettings, id: Option<&str>) -> Template {
    let find = |id: &str| builtin(id).or_else(|| cfg.templates.iter().find(|t| t.id == id).map(custom));
    id.and_then(find)
        .or_else(|| find(&cfg.template))
        .unwrap_or_else(|| builtin(DEFAULT).expect("일반 회의 틀"))
}

/// 고를 수 있는 틀 — 기본 틀, 그다음 내 틀
pub fn list(cfg: &SummarySettings) -> Vec<TemplateInfo> {
    let mut out: Vec<TemplateInfo> = BUILTIN
        .iter()
        .map(|b| TemplateInfo {
            id: b.0.into(),
            name: b.1.into(),
            desc: b.2.into(),
            sections: section_names(prompts::file(b.3)),
            custom: false,
        })
        .collect();
    out.extend(cfg.templates.iter().map(|t| {
        let tpl = custom(t);
        TemplateInfo { id: tpl.id, name: tpl.name, desc: String::new(), sections: section_names(&tpl.format), custom: true }
    }));
    out
}

/// 섹션 줄 하나 → (이름, 설명). "이름 — 설명" · "이름 - 설명" · "이름: 설명", 앞의 "- " · "1. " · "### " 는 뗀다
fn split_line(line: &str) -> (String, String) {
    let line = line.trim().trim_start_matches('#').trim();
    let line = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).unwrap_or(line);
    let line = match line.split_once(". ") {
        Some((n, rest)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => rest,
        _ => line,
    };
    let cut = ["—", " - ", ":", "："].iter().filter_map(|sep| line.find(sep).map(|i| (i, sep.len()))).min();
    let (name, desc) = match cut {
        Some((i, n)) => (&line[..i], &line[i + n..]),
        None => (line, ""),
    };
    let clean = |s: &str, max: usize| s.trim().trim_matches(['*', '"']).trim().chars().take(max).collect::<String>();
    (clean(name, MAX_NAME), clean(desc, MAX_DESC))
}

/// 내 틀의 섹션 줄 → 회의록 형식. 일반 회의에 있는 섹션(한 줄 요약 · 주요 논의 · 결정 사항 · 할 일 ·
/// 남은 질문)은 설명이 없으면 일반 회의의 적는 법을 그대로 쓰고, 한 줄 요약 · 할 일은 설명이 있어도 모양을 지킨다
/// (한 줄 · 체크 목록). 섹션이 하나도 없으면 일반 회의 형식
pub fn custom_format(sections: &str) -> String {
    let meeting = prompts::file("templates/meeting.md");
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in sections.lines() {
        let (name, desc) = split_line(line);
        if name.is_empty() || name == "제목" || !seen.insert(name.clone()) {
            continue;
        }
        let body = match (name.as_str(), desc.is_empty()) {
            ("한 줄 요약", false) => format!("({desc}, 개조식 한 줄로 60자 이내)"),
            ("할 일", false) => format!("- [ ] ({desc}) — 담당: (화자) / 기한: (언급된 경우) [시각]"),
            (_, true) => section_body(meeting, &name)
                .unwrap_or_else(|| format!("- ({name}에 해당하는 요점, 한 줄씩. 없으면 \"없음\") [시각]")),
            (_, false) => format!("- ({desc}, 한 줄씩. 없으면 \"없음\") [시각]"),
        };
        out.push(format!("### {name}\n{body}"));
        if out.len() >= MAX_SECTIONS {
            break;
        }
    }
    if out.is_empty() {
        return meeting.to_string();
    }
    out.join("\n\n")
}

/// 형식에서 그 섹션의 적는 법 (섹션 제목 줄 다음부터 빈 줄 전까지)
fn section_body(format: &str, name: &str) -> Option<String> {
    let mut lines = format.lines().skip_while(|l| l.strip_prefix("### ").map(str::trim) != Some(name));
    lines.next()?;
    let body: Vec<&str> = lines.take_while(|l| !l.trim().is_empty() && !l.starts_with("### ")).collect();
    (!body.is_empty()).then(|| body.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(templates: Vec<CustomTemplate>, default: &str) -> SummarySettings {
        SummarySettings { templates, template: default.into(), ..Default::default() }
    }

    fn mine(id: &str, name: &str, sections: &str) -> CustomTemplate {
        CustomTemplate { id: id.into(), name: name.into(), sections: sections.into() }
    }

    #[test]
    fn every_builtin_has_a_one_line_summary_and_no_title() {
        for b in BUILTIN {
            let names = section_names(prompts::file(b.3));
            assert_eq!(names.first().map(String::as_str), Some("한 줄 요약"), "{}", b.0);
            assert!(!names.iter().any(|n| n == "제목"), "{}: 제목은 summary.md 가 같이 쓴다", b.0);
            assert!(names.len() >= 4, "{}: {names:?}", b.0);
        }
        assert_eq!(BUILTIN[0].0, DEFAULT);
    }

    #[test]
    fn resolve_falls_back_to_the_default_then_meeting() {
        let c = cfg(vec![mine("my-1", "주간 보고", "지난주 한 일\n이번 주 할 일")], "lecture");
        assert_eq!(resolve(&c, Some("one_on_one")).name, "1:1 면담");
        assert_eq!(resolve(&c, Some("my-1")).name, "주간 보고");
        assert_eq!(resolve(&c, None).id, "lecture", "고르지 않으면 설정의 기본 틀");
        assert_eq!(resolve(&c, Some("지운-틀")).id, "lecture", "없어진 틀이면 기본 틀");
        let c = cfg(vec![], "지운-틀");
        assert_eq!(resolve(&c, None).id, "meeting", "기본 틀도 없어졌으면 일반 회의");
        let names: Vec<String> = list(&cfg(vec![mine("my-1", "  ", "가")], "meeting")).into_iter().map(|t| t.name).collect();
        assert_eq!(names.last().map(String::as_str), Some("내 틀"), "이름이 비었으면");
        assert_eq!(names.len(), BUILTIN.len() + 1);
    }

    #[test]
    fn custom_sections_become_a_format() {
        let f = custom_format(
            "한 줄 요약\n- 지난주 한 일 — 완료한 것 위주로\n2. 막힌 것: 도움이 필요한 것\n할 일 — 다음 주까지\n결정 사항\n제목\n막힌 것\n\n### 기타",
        );
        assert_eq!(section_names(&f), ["한 줄 요약", "지난주 한 일", "막힌 것", "할 일", "결정 사항", "기타"]);
        assert!(f.contains("### 한 줄 요약\n(회의 전체를 개조식 한 줄로, 60자 이내)"), "일반 회의의 적는 법 그대로\n{f}");
        assert!(f.contains("### 지난주 한 일\n- (완료한 것 위주로, 한 줄씩. 없으면 \"없음\") [시각]"), "{f}");
        assert!(f.contains("### 할 일\n- [ ] (다음 주까지) — 담당: (화자)"), "할 일은 체크 목록 모양\n{f}");
        assert!(f.contains("### 결정 사항\n- (확정된 것만, 한 줄씩. 없으면 \"없음\") [시각]"), "{f}");
        assert!(f.contains("### 기타\n- (기타에 해당하는 요점"), "{f}");
        // 섹션이 없으면 일반 회의
        assert_eq!(custom_format(" \n\n"), prompts::file("templates/meeting.md"));
        // 너무 많으면 앞의 것만
        let many: String = (0..20).map(|i| format!("섹션{i}\n")).collect();
        assert_eq!(section_names(&custom_format(&many)).len(), MAX_SECTIONS);
    }

    #[test]
    fn open_question_rule_only_for_templates_that_have_the_section() {
        let with = prompts::summary_with(prompts::file("templates/meeting.md"));
        let without = prompts::summary_with(prompts::file("templates/lecture.md"));
        assert!(with.contains("\"남은 질문\"에는") && !without.contains("\"남은 질문\"에는"));
        assert!(!without.contains("{남은 질문 기준}") && !without.contains("{형식}") && !without.contains("\n\n\n"));
        assert!(without.contains("### 제목\n(회의 주제를") && without.contains("### 질의응답"));
        // 일반 회의는 틀이 생기기 전과 같은 지시
        assert_eq!(prompts::summary(), with);
    }
}
