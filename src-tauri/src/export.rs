//! 회의록을 Word(.docx) · PDF 로 — 결재 · 공유용 (기능 확장 계획 13번)
//!
//! 회의 기록 하나를 문서 블록([`Block`])으로 한 번 짜고([`blocks`]), 같은 블록으로
//! - Word: WordprocessingML 을 직접 써서 zip 으로 묶는다 ([`docx`]) — 외부 프로그램 · 큰 라이브러리 없이
//! - PDF: HTML 로 그려([`html`]) 화면(WebView2)의 인쇄용 칸(`#print-doc`)에 넣고 PrintToPdf 로 저장한다
//!   ([`print_pdf`]) — 화면과 같은 글꼴 · 색으로, 인쇄 대화상자 없이 바로 파일로
//!
//! 실리는 것은 Markdown 회의록과 같다: 요약(할 일은 표로) · 메모 · 묻고 답하기 · 발언 통계 · 녹취록 ·
//! 문장 코멘트와 중요 표시 ⭐. 요약의 근거 시각 `[12:34]` 은 흐린 글씨로 남겨 녹취록과 맞춰 볼 수 있게 한다.

use std::collections::HashSet;
use std::io::Write as _;

use crate::session::{fmt_dur, fmt_time, star_note, SessionDoc, TrMode};
use crate::share::{self, Kind};

/// 화자 색 (화면 ui/app.js 의 `COLORS` 와 같은 차례)
const COLORS: [&str; 10] = ["7c6cff", "0ea5e9", "f59e0b", "10b981", "f43f5e", "6366f1", "14b8a6", "f97316", "a855f7", "84cc16"];
/// 화자가 없는 문단의 색
const NO_SPEAKER: &str = "8a8a99";

/// 글 조각 — 굵게 · 근거 시각(흐리게)
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Run {
    pub text: String,
    pub bold: bool,
    pub time: bool,
}

impl Run {
    fn plain(text: impl Into<String>) -> Self {
        Run { text: text.into(), bold: false, time: false }
    }
    fn bold(text: impl Into<String>) -> Self {
        Run { text: text.into(), bold: true, time: false }
    }
    fn time(text: impl Into<String>) -> Self {
        Run { text: text.into(), bold: false, time: true }
    }
}

/// 문서 블록 (Word · PDF 가 같이 쓴다)
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Block {
    /// 회의 제목
    Title(String),
    /// 일시 · 길이 · 참석 … (이름, 값)
    Meta(Vec<(String, String)>),
    /// 큰 제목 (요약 · 메모 · 녹취록 …)
    H1(String),
    /// 요약의 섹션 제목
    H2(String),
    Para(Vec<Run>),
    /// 목록 한 줄 — 깊이(0 = 맨 바깥), 체크 목록이면 끝냈는지
    Item { depth: usize, check: Option<bool>, runs: Vec<Run> },
    /// 표 — 열 이름 · 열 너비(%, 합 100) · 칸마다 글 조각
    Table { head: Vec<String>, widths: Vec<u32>, rows: Vec<Vec<Vec<Run>>> },
    /// 녹취록 문단 머리: 화자 이름 · 색(16진) · 시각
    Turn { name: String, color: &'static str, time: String },
    /// 녹취록 문단 글
    Speech(String),
    /// 녹취록 문단의 번역 (원문 · 번역 둘 다 낼 때 문단 아래)
    Translated(String),
    /// 문장 코멘트 · 중요 표시: 아이콘, 메모에 적힌 시각, 글
    Note { icon: &'static str, time: String, text: String },
}

/// 근거 시각 안쪽 글 ("12:34" · "1:02:03")
fn is_stamp(s: &str) -> bool {
    s.contains(':') && s.chars().all(|c| c.is_ascii_digit() || c == ':')
}

/// 요약 · 답 · 메모의 한 줄 → 글 조각: `**굵게**`, 근거 시각 `[12:34]` (그 밖의 `[` 는 글자 그대로)
pub(crate) fn runs(s: &str) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    let mut bold = false;
    let mut buf = String::new();
    let flush = |out: &mut Vec<Run>, buf: &mut String, bold: bool| {
        if !buf.is_empty() {
            out.push(Run { text: std::mem::take(buf), bold, time: false });
        }
    };
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if let Some(r) = rest.strip_prefix("**") {
            flush(&mut out, &mut buf, bold);
            bold = !bold;
            rest = r;
            continue;
        }
        if c == '[' {
            if let Some(j) = rest[1..].find(']') {
                let inner = &rest[1..1 + j];
                if is_stamp(inner) {
                    flush(&mut out, &mut buf, bold);
                    out.push(Run::time(format!("[{inner}]")));
                    rest = &rest[j + 2..];
                    continue;
                }
            }
        }
        buf.push(c);
        rest = &rest[c.len_utf8()..];
    }
    flush(&mut out, &mut buf, bold);
    out
}

/// Markdown 한 줄의 꼴 (요약 · 답 · 메모에 쓰는 만큼만)
#[derive(Debug, PartialEq)]
enum Md {
    Head(String),
    Item { depth: usize, check: Option<bool>, text: String },
    Para(String),
}

fn md(s: &str) -> Vec<Md> {
    let mut out = Vec::new();
    for raw in s.lines() {
        let line = raw.trim_end();
        let t = line.trim_start();
        if t.is_empty() || t.chars().all(|c| c == '-' || c == '*' || c == '_') && t.len() >= 3 {
            continue; // 빈 줄 · 가로줄
        }
        if t.starts_with('#') {
            let h = t.trim_start_matches('#').trim();
            if !h.is_empty() {
                out.push(Md::Head(h.to_string()));
            }
            continue;
        }
        let indent: usize = line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 2 } else { 1 }).sum();
        match ["- ", "* ", "• ", "· "].iter().find_map(|b| t.strip_prefix(b)) {
            Some(body) => {
                let (check, text) = if let Some(b) = body.strip_prefix("[ ] ") {
                    (Some(false), b)
                } else if let Some(b) = body.strip_prefix("[x] ").or_else(|| body.strip_prefix("[X] ")) {
                    (Some(true), b)
                } else {
                    (None, body)
                };
                out.push(Md::Item { depth: indent / 2, check, text: text.trim().to_string() });
            }
            None => out.push(Md::Para(t.to_string())),
        }
    }
    out
}

/// Markdown 줄들 → 블록 (`head`: 안쪽 제목을 무엇으로 — 요약은 섹션 제목, 답 · 메모는 굵은 줄)
fn md_blocks(lines: Vec<Md>, out: &mut Vec<Block>, head_as_h2: bool) {
    for l in lines {
        out.push(match l {
            Md::Head(h) if head_as_h2 => Block::H2(h),
            Md::Head(h) => Block::Para(vec![Run::bold(h)]),
            Md::Item { depth, check, text } => Block::Item { depth, check, runs: runs(&text) },
            Md::Para(p) => Block::Para(runs(&p)),
        });
    }
}

/// 글 속 근거 시각만 뽑는다 ("[00:46]" 들)
fn stamps(text: &str) -> Vec<Run> {
    runs(text).into_iter().filter(|r| r.time).collect()
}

/// 요약 → 블록. 섹션 제목은 [`Block::H2`], 할 일 섹션은 표(확인 · 할 일 · 담당 · 기한), 나머지는 목록 · 문단.
fn summary_blocks(summary: &str, out: &mut Vec<Block>) {
    // 섹션별로 (이름 없는 맨 앞 줄들은 이름 없이)
    let mut sections: Vec<(Option<String>, Vec<Md>)> = Vec::new();
    for l in md(summary) {
        match l {
            Md::Head(h) => sections.push((Some(h), Vec::new())),
            other => {
                if sections.is_empty() {
                    sections.push((None, Vec::new()));
                }
                sections.last_mut().expect("섹션").1.push(other);
            }
        }
    }
    for (name, lines) in sections {
        let is_todo = name.as_deref().is_some_and(|n| share::kind(&share::clean(n)) == Kind::Todo);
        if let Some(n) = name {
            out.push(Block::H2(n));
        }
        if !is_todo {
            md_blocks(lines, out, true);
            continue;
        }
        // 할 일: 체크 목록 줄은 표로, 나머지 줄(설명 · "없음")은 그대로
        let mut rows: Vec<(bool, share::Todo, Vec<Run>)> = Vec::new();
        let mut rest: Vec<Md> = Vec::new();
        for l in lines {
            match l {
                Md::Item { check, text, .. } if !share::is_none(&share::clean(&text)) => {
                    rows.push((check == Some(true), share::todo_of(&share::clean(&text)), stamps(&text)));
                }
                // "- [ ] 없음" 은 체크 칸 없이
                Md::Item { depth, text, .. } => rest.push(Md::Item { depth, check: None, text }),
                other => rest.push(other),
            }
        }
        if !rows.is_empty() {
            let owners = rows.iter().any(|(_, t, _)| t.owner.is_some());
            let mut head = vec!["확인".to_string(), "할 일".to_string()];
            let widths = if owners {
                head.push("담당".into());
                vec![8, 54, 18, 20]
            } else {
                vec![8, 70, 22]
            };
            head.push("기한".into());
            let rows = rows
                .into_iter()
                .map(|(done, t, times)| {
                    let mut what = vec![Run::plain(t.text)];
                    for r in times {
                        what.push(Run::plain(" "));
                        what.push(r);
                    }
                    let mut row = vec![vec![Run::plain(if done { "☑" } else { "☐" })], what];
                    if owners {
                        row.push(t.owner.map(Run::plain).into_iter().collect());
                    }
                    row.push(t.due.map(Run::plain).into_iter().collect());
                    row
                })
                .collect();
            out.push(Block::Table { head, widths, rows });
        }
        md_blocks(rest, out, true);
    }
}

/// 회의 기록 → 문서 블록. `tr`: 녹취록을 원문 · 번역 · 둘 다 (번역이 없는 문단은 원문)
pub(crate) fn blocks(doc: &SessionDoc, tr: TrMode) -> Vec<Block> {
    let mut out = vec![Block::Title(if doc.title.trim().is_empty() { "회의록".into() } else { doc.title.clone() })];

    // 참석: 말한 차례로, 합친 화자는 하나로
    let mut seen = HashSet::new();
    let mut names: Vec<String> = Vec::new();
    for u in &doc.utterances {
        let s = doc.resolve(u.speaker);
        if s.is_some() && seen.insert(s) {
            let n = doc.speaker_name(s);
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    let mut meta = vec![("일시".to_string(), doc.started_at.clone()), ("길이".to_string(), fmt_dur(doc.duration_ms))];
    if !names.is_empty() {
        meta.push(("참석".into(), format!("{}  ({}명)", names.join(" · "), names.len())));
    }
    if let Some(f) = doc.source_file.as_deref().filter(|f| !f.is_empty()) {
        meta.push(("녹음 파일".into(), f.to_string()));
    }
    if !doc.engine.is_empty() {
        meta.push(("받아 적은 엔진".into(), doc.engine.clone()));
    }
    out.push(Block::Meta(meta));

    if let Some(sum) = doc.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(Block::H1("요약".into()));
        summary_blocks(sum, &mut out);
    }

    let (memo, comments) = doc.memo_and_comments();
    if !memo.trim().is_empty() {
        out.push(Block::H1("메모".into()));
        md_blocks(md(&memo), &mut out, false);
    }

    if !doc.qa.is_empty() {
        out.push(Block::H1("묻고 답하기".into()));
        for x in &doc.qa {
            let q = x.q.split_whitespace().collect::<Vec<_>>().join(" ");
            out.push(Block::Para(vec![Run::bold(format!("Q. {q}"))]));
            md_blocks(md(&x.a), &mut out, false);
        }
    }

    let stats = doc.speaker_stats();
    if stats.len() >= 2 {
        let total: u64 = stats.iter().map(|x| x.ms).sum();
        let rows = stats
            .iter()
            .map(|x| {
                let pct = if total > 0 { (x.ms as f64 * 100.0 / total as f64).round() as u64 } else { 0 };
                vec![
                    vec![Run::plain(doc.speaker_name(Some(x.speaker)))],
                    vec![Run::plain(fmt_dur(x.ms))],
                    vec![Run::plain(format!("{pct}%"))],
                    vec![Run::plain(format!("{}번", x.turns))],
                    vec![Run::plain(format!("{} ", fmt_dur(x.longest_ms))), Run::time(format!("[{}]", fmt_time(x.longest_at)))],
                ]
            })
            .collect();
        out.push(Block::H1("발언 통계".into()));
        out.push(Block::Table {
            head: ["화자", "말한 시간", "비율", "발언 수", "가장 긴 발언"].map(String::from).to_vec(),
            widths: vec![28, 18, 12, 14, 28],
            rows,
        });
    }

    let paras = doc.paragraphs();
    if !paras.is_empty() {
        out.push(Block::H1("녹취록".into()));
    }
    let trs = if tr == TrMode::Orig { Default::default() } else { doc.tr_map() };
    let orig = doc.orig_map();
    for (spk, start, _, text, ids) in paras {
        let color = doc.resolve(spk).map_or(NO_SPEAKER, |n| COLORS[n as usize % COLORS.len()]);
        out.push(Block::Turn { name: doc.speaker_name(spk), color, time: fmt_time(start) });
        match (tr, SessionDoc::tr_join(&trs, &ids)) {
            // 번역 안 된 문장은 원문으로 채운다 (문단 일부만 번역됐어도 문장이 빠지지 않게)
            (TrMode::Tr, Some(_)) => out.push(Block::Speech(SessionDoc::tr_only(&trs, &orig, &ids))),
            (TrMode::Both, Some(t)) => {
                out.push(Block::Speech(text));
                out.push(Block::Translated(t));
            }
            _ => out.push(Block::Speech(text)),
        }
        for id in ids {
            for c in comments.get(&id).into_iter().flatten() {
                let (icon, text) = match star_note(&c.text) {
                    Some("") => ("⭐", "중요 표시".to_string()),
                    Some(note) => ("⭐", note.to_string()),
                    None => ("💬", c.text.clone()),
                };
                out.push(Block::Note { icon, time: fmt_time(c.ms), text });
            }
        }
    }
    out
}

// ───────────────────────── Word (.docx) ─────────────────────────

/// 본문 너비 (A4 210mm − 좌우 여백 23mm, 1/20 pt)
const TEXT_W: u32 = 9298;

/// XML 글자 — 특수 문자를 바꾸고, XML 에 넣을 수 없는 제어 문자는 뺀다.
/// 그림 문자 ⭐ 는 ★ 로 — 맑은 고딕에 없어 Word 가 다른 글꼴로 그리며(PDF 저장 때 그 글꼴을 통째로 넣어 수 MB),
/// 코멘트 아이콘도 같은 까닭으로 ★ · ※ 를 쓴다 (PDF 내보내기는 화면처럼 그림 문자 그대로)
fn xml(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '⭐' => o.push('★'),
            '\u{FE0F}' => {}
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\t' | '\n' | '\r' => o.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => o.push(c),
        }
    }
    o
}

/// 글 조각 → `<w:r>` (줄바꿈은 `<w:br/>`)
fn w_runs(runs: &[Run], extra_rpr: &str) -> String {
    let mut s = String::new();
    for r in runs {
        let mut rpr = String::new();
        if r.bold {
            rpr.push_str("<w:b/>");
        }
        if r.time {
            rpr.push_str("<w:color w:val=\"8A8A99\"/><w:sz w:val=\"17\"/><w:szCs w:val=\"17\"/>");
        } else {
            rpr.push_str(extra_rpr);
        }
        let rpr = if rpr.is_empty() { String::new() } else { format!("<w:rPr>{rpr}</w:rPr>") };
        for (i, part) in r.text.split('\n').enumerate() {
            s.push_str("<w:r>");
            s.push_str(&rpr);
            if i > 0 {
                s.push_str("<w:br/>");
            }
            s.push_str(&format!("<w:t xml:space=\"preserve\">{}</w:t></w:r>", xml(part.trim_end_matches('\r'))));
        }
    }
    s
}

fn w_p(ppr: &str, runs: &str) -> String {
    if ppr.is_empty() {
        format!("<w:p>{runs}</w:p>")
    } else {
        format!("<w:p><w:pPr>{ppr}</w:pPr>{runs}</w:p>")
    }
}

fn w_cell(width: u32, fill: Option<&str>, runs: &str) -> String {
    let shd = fill.map(|f| format!("<w:shd w:val=\"clear\" w:color=\"auto\" w:fill=\"{f}\"/>")).unwrap_or_default();
    format!(
        "<w:tc><w:tcPr><w:tcW w:w=\"{width}\" w:type=\"dxa\"/>{shd}</w:tcPr>{}</w:tc>",
        w_p("<w:pStyle w:val=\"TableText\"/>", runs)
    )
}

fn w_table(head: &[String], widths: &[u32], rows: &[Vec<Vec<Run>>], label_col: bool) -> String {
    let tw: Vec<u32> = widths.iter().map(|p| TEXT_W * p / 100).collect();
    let mut s = format!(
        "<w:tbl><w:tblPr><w:tblStyle w:val=\"Grid\"/><w:tblW w:w=\"{TEXT_W}\" w:type=\"dxa\"/><w:tblLayout w:type=\"fixed\"/></w:tblPr><w:tblGrid>"
    );
    for w in &tw {
        s.push_str(&format!("<w:gridCol w:w=\"{w}\"/>"));
    }
    s.push_str("</w:tblGrid>");
    if !head.is_empty() {
        s.push_str("<w:tr><w:trPr><w:cantSplit/><w:tblHeader/></w:trPr>");
        for (h, w) in head.iter().zip(&tw) {
            s.push_str(&w_cell(*w, Some("F1F1F5"), &w_runs(&[Run::bold(h.clone())], "")));
        }
        s.push_str("</w:tr>");
    }
    for row in rows {
        s.push_str("<w:tr><w:trPr><w:cantSplit/></w:trPr>");
        for (i, (cell, w)) in row.iter().zip(&tw).enumerate() {
            let label = label_col && i == 0;
            let fill = label.then_some("F4F4F7");
            s.push_str(&w_cell(*w, fill, &w_runs(cell, if label { "<w:color w:val=\"4A4A57\"/>" } else { "" })));
        }
        s.push_str("</w:tr>");
    }
    s.push_str("</w:tbl>");
    s
}

fn w_body(blocks: &[Block]) -> String {
    let mut s = String::new();
    for b in blocks {
        match b {
            Block::Title(t) => s.push_str(&w_p("<w:pStyle w:val=\"Title\"/>", &w_runs(&[Run::plain(t.clone())], ""))),
            Block::Meta(rows) => {
                let rows: Vec<Vec<Vec<Run>>> =
                    rows.iter().map(|(k, v)| vec![vec![Run::plain(k.clone())], vec![Run::plain(v.clone())]]).collect();
                s.push_str(&w_table(&[], &[20, 80], &rows, true));
            }
            Block::H1(t) => s.push_str(&w_p("<w:pStyle w:val=\"Heading1\"/>", &w_runs(&[Run::plain(t.clone())], ""))),
            Block::H2(t) => s.push_str(&w_p("<w:pStyle w:val=\"Heading2\"/>", &w_runs(&runs(t), ""))),
            Block::Para(r) => s.push_str(&w_p("", &w_runs(r, ""))),
            Block::Item { depth, check: None, runs } => s.push_str(&w_p(
                &format!(
                    "<w:pStyle w:val=\"ListParagraph\"/><w:numPr><w:ilvl w:val=\"{}\"/><w:numId w:val=\"1\"/></w:numPr>",
                    (*depth).min(3)
                ),
                &w_runs(runs, ""),
            )),
            Block::Item { depth, check: Some(done), runs } => {
                let mut r = vec![Run::plain(if *done { "☑ " } else { "☐ " })];
                r.extend(runs.iter().cloned());
                s.push_str(&w_p(
                    &format!("<w:pStyle w:val=\"ListParagraph\"/><w:ind w:left=\"{}\" w:hanging=\"300\"/>", 400 + 400 * (*depth).min(3)),
                    &w_runs(&r, ""),
                ));
            }
            Block::Table { head, widths, rows } => s.push_str(&w_table(head, widths, rows, false)),
            Block::Turn { name, color, time } => s.push_str(&w_p(
                "<w:pStyle w:val=\"Turn\"/>",
                &format!(
                    "{}{}{}",
                    w_runs(&[Run::bold(name.clone())], &format!("<w:color w:val=\"{}\"/>", color.to_uppercase())),
                    w_runs(&[Run::plain("   ")], ""),
                    w_runs(&[Run::time(time.clone())], "")
                ),
            )),
            Block::Speech(t) => s.push_str(&w_p("<w:pStyle w:val=\"Speech\"/>", &w_runs(&[Run::plain(t.clone())], ""))),
            Block::Translated(t) => {
                s.push_str(&w_p("<w:pStyle w:val=\"Translated\"/>", &w_runs(&[Run::plain(format!("→ {t}"))], "")))
            }
            Block::Note { icon, time, text } => s.push_str(&w_p(
                "<w:pStyle w:val=\"Note\"/>",
                &w_runs(&[Run::plain(format!("{} ", if *icon == "⭐" { "★" } else { "※" })), Run::time(time.clone()), Run::plain(format!("  {text}"))], ""),
            )),
        }
    }
    // 본문은 문단으로 끝나야 한다 (표로 끝나면 Word 가 고쳐 연다)
    s.push_str("<w:p/>");
    s
}

const W_NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
                    xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"";
const XML_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n";

const CONTENT_TYPES: &str = "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/word/numbering.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml\"/>\
<Override PartName=\"/word/footer1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
</Types>";

const PKG_RELS: &str = "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/>\
</Relationships>";

const DOC_RELS: &str = "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering\" Target=\"numbering.xml\"/>\
<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer\" Target=\"footer1.xml\"/>\
</Relationships>";

/// 문단 · 표 스타일 (맑은 고딕 10pt, 제목 · 큰 제목은 Word 기본 제목 스타일 — 탐색 창 · 목차에 잡힌다)
const STYLES: &str = "<w:docDefaults>\
<w:rPrDefault><w:rPr><w:rFonts w:ascii=\"Malgun Gothic\" w:hAnsi=\"Malgun Gothic\" w:eastAsia=\"Malgun Gothic\" w:cs=\"Malgun Gothic\"/>\
<w:color w:val=\"17171C\"/><w:sz w:val=\"20\"/><w:szCs w:val=\"20\"/><w:lang w:val=\"ko-KR\" w:eastAsia=\"ko-KR\" w:bidi=\"ar-SA\"/></w:rPr></w:rPrDefault>\
<w:pPrDefault><w:pPr><w:spacing w:after=\"100\" w:line=\"300\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault>\
</w:docDefaults>\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Title\"><w:name w:val=\"Title\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:qFormat/>\
<w:pPr><w:spacing w:after=\"200\"/></w:pPr><w:rPr><w:b/><w:sz w:val=\"36\"/><w:szCs w:val=\"36\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:qFormat/>\
<w:pPr><w:keepNext/><w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"2\" w:color=\"D5D5DE\"/></w:pBdr><w:spacing w:before=\"360\" w:after=\"120\"/><w:outlineLvl w:val=\"0\"/></w:pPr>\
<w:rPr><w:b/><w:sz w:val=\"26\"/><w:szCs w:val=\"26\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading2\"><w:name w:val=\"heading 2\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:qFormat/>\
<w:pPr><w:keepNext/><w:spacing w:before=\"200\" w:after=\"60\"/><w:outlineLvl w:val=\"1\"/></w:pPr>\
<w:rPr><w:b/><w:color w:val=\"3A3A48\"/><w:sz w:val=\"22\"/><w:szCs w:val=\"22\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"ListParagraph\"><w:name w:val=\"List Paragraph\"/><w:basedOn w:val=\"Normal\"/><w:qFormat/>\
<w:pPr><w:spacing w:after=\"40\"/></w:pPr></w:style>\
<w:style w:type=\"paragraph\" w:customStyle=\"1\" w:styleId=\"TableText\"><w:name w:val=\"Table Text\"/><w:basedOn w:val=\"Normal\"/>\
<w:pPr><w:spacing w:after=\"0\" w:line=\"264\" w:lineRule=\"auto\"/></w:pPr><w:rPr><w:sz w:val=\"19\"/><w:szCs w:val=\"19\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:customStyle=\"1\" w:styleId=\"Turn\"><w:name w:val=\"Speaker\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Speech\"/>\
<w:pPr><w:keepNext/><w:spacing w:before=\"160\" w:after=\"0\"/></w:pPr></w:style>\
<w:style w:type=\"paragraph\" w:customStyle=\"1\" w:styleId=\"Speech\"><w:name w:val=\"Speech\"/><w:basedOn w:val=\"Normal\"/>\
<w:pPr><w:spacing w:after=\"60\"/></w:pPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Footer\"><w:name w:val=\"footer\"/><w:basedOn w:val=\"Normal\"/>\
<w:pPr><w:spacing w:after=\"0\"/><w:jc w:val=\"center\"/></w:pPr><w:rPr><w:color w:val=\"8A8A99\"/><w:sz w:val=\"16\"/><w:szCs w:val=\"16\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:customStyle=\"1\" w:styleId=\"Translated\"><w:name w:val=\"Translation\"/><w:basedOn w:val=\"Normal\"/>\
<w:pPr><w:spacing w:after=\"80\"/></w:pPr><w:rPr><w:color w:val=\"3F5A9E\"/><w:sz w:val=\"19\"/><w:szCs w:val=\"19\"/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:customStyle=\"1\" w:styleId=\"Note\"><w:name w:val=\"Comment Note\"/><w:basedOn w:val=\"Normal\"/>\
<w:pPr><w:pBdr><w:left w:val=\"single\" w:sz=\"12\" w:space=\"6\" w:color=\"C9C9D6\"/></w:pBdr><w:spacing w:after=\"60\"/><w:ind w:left=\"240\"/></w:pPr>\
<w:rPr><w:color w:val=\"4A4A57\"/><w:sz w:val=\"19\"/><w:szCs w:val=\"19\"/></w:rPr></w:style>\
<w:style w:type=\"table\" w:default=\"1\" w:styleId=\"TableNormal\"><w:name w:val=\"Normal Table\"/>\
<w:tblPr><w:tblInd w:w=\"0\" w:type=\"dxa\"/><w:tblCellMar><w:top w:w=\"0\" w:type=\"dxa\"/><w:left w:w=\"108\" w:type=\"dxa\"/>\
<w:bottom w:w=\"0\" w:type=\"dxa\"/><w:right w:w=\"108\" w:type=\"dxa\"/></w:tblCellMar></w:tblPr></w:style>\
<w:style w:type=\"table\" w:styleId=\"Grid\"><w:name w:val=\"Table Grid\"/><w:basedOn w:val=\"TableNormal\"/>\
<w:tblPr><w:tblBorders><w:top w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"D5D5DE\"/><w:left w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"D5D5DE\"/>\
<w:bottom w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"D5D5DE\"/><w:right w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"D5D5DE\"/>\
<w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"D5D5DE\"/><w:insideV w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"D5D5DE\"/></w:tblBorders>\
<w:tblCellMar><w:top w:w=\"50\" w:type=\"dxa\"/><w:left w:w=\"100\" w:type=\"dxa\"/><w:bottom w:w=\"50\" w:type=\"dxa\"/><w:right w:w=\"100\" w:type=\"dxa\"/></w:tblCellMar>\
</w:tblPr></w:style>";

/// 글머리 기호 (깊이 4단계까지)
const NUMBERING: &str = "<w:abstractNum w:abstractNumId=\"0\"><w:multiLevelType w:val=\"hybridMultilevel\"/>\
<w:lvl w:ilvl=\"0\"><w:start w:val=\"1\"/><w:numFmt w:val=\"bullet\"/><w:lvlText w:val=\"•\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"400\" w:hanging=\"300\"/></w:pPr></w:lvl>\
<w:lvl w:ilvl=\"1\"><w:start w:val=\"1\"/><w:numFmt w:val=\"bullet\"/><w:lvlText w:val=\"◦\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"800\" w:hanging=\"300\"/></w:pPr></w:lvl>\
<w:lvl w:ilvl=\"2\"><w:start w:val=\"1\"/><w:numFmt w:val=\"bullet\"/><w:lvlText w:val=\"▪\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"1200\" w:hanging=\"300\"/></w:pPr></w:lvl>\
<w:lvl w:ilvl=\"3\"><w:start w:val=\"1\"/><w:numFmt w:val=\"bullet\"/><w:lvlText w:val=\"–\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"1600\" w:hanging=\"300\"/></w:pPr></w:lvl>\
</w:abstractNum><w:num w:numId=\"1\"><w:abstractNumId w:val=\"0\"/></w:num>";

/// Word 문서 파일 (.docx)
pub(crate) fn docx(doc: &SessionDoc, tr: TrMode) -> Vec<u8> {
    let title = if doc.title.trim().is_empty() { "회의록" } else { doc.title.as_str() };
    let document = format!(
        "{XML_HEAD}<w:document {W_NS}><w:body>{}<w:sectPr><w:footerReference w:type=\"default\" r:id=\"rId3\"/>\
         <w:pgSz w:w=\"11906\" w:h=\"16838\"/>\
         <w:pgMar w:top=\"1304\" w:right=\"1304\" w:bottom=\"1304\" w:left=\"1304\" w:header=\"680\" w:footer=\"567\" w:gutter=\"0\"/>\
         </w:sectPr></w:body></w:document>",
        w_body(&blocks(doc, tr))
    );
    // 흐린 작은 글씨는 문단 스타일(Footer)로 — Word 가 쪽 번호 칸을 다시 셀 때 칸 안 글씨 꾸밈을 버린다
    let footer = format!(
        "{XML_HEAD}<w:ftr {W_NS}><w:p><w:pPr><w:pStyle w:val=\"Footer\"/></w:pPr>\
         <w:r><w:t xml:space=\"preserve\">{}   </w:t></w:r>\
         <w:fldSimple w:instr=\" PAGE \"><w:r><w:t>1</w:t></w:r></w:fldSimple>\
         <w:r><w:t xml:space=\"preserve\"> / </w:t></w:r>\
         <w:fldSimple w:instr=\" NUMPAGES \"><w:r><w:t>1</w:t></w:r></w:fldSimple></w:p></w:ftr>",
        xml(title)
    );
    let core = format!(
        "{XML_HEAD}<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>{}</dc:title><dc:creator>받아쓰기</dc:creator></cp:coreProperties>",
        xml(title)
    );
    let files: [(&str, String); 8] = [
        ("[Content_Types].xml", format!("{XML_HEAD}{CONTENT_TYPES}")),
        ("_rels/.rels", format!("{XML_HEAD}{PKG_RELS}")),
        ("docProps/core.xml", core),
        ("word/document.xml", document),
        ("word/_rels/document.xml.rels", format!("{XML_HEAD}{DOC_RELS}")),
        ("word/styles.xml", format!("{XML_HEAD}<w:styles {W_NS}>{STYLES}</w:styles>")),
        ("word/numbering.xml", format!("{XML_HEAD}<w:numbering {W_NS}>{NUMBERING}</w:numbering>")),
        ("word/footer1.xml", footer),
    ];
    zip(&files)
}

/// zip 으로 묶는다 (deflate, 이름은 ASCII 라 UTF-8 표시 없이)
fn zip(files: &[(&str, String)]) -> Vec<u8> {
    fn u16le(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn u32le(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    // 1980-01-01 00:00 (zip 이 적을 수 있는 가장 이른 때 — 같은 회의는 늘 같은 파일이 되게)
    const DOS_TIME: u16 = 0;
    const DOS_DATE: u16 = (1 << 5) | 1;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, text) in files {
        let data = text.as_bytes();
        let crc = crc32fast::hash(data);
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).expect("메모리에 쓰기");
        let comp = enc.finish().expect("메모리에 쓰기");
        let offset = out.len() as u32;
        for (sig, v) in [(0x0403_4b50u32, &mut out), (0x0201_4b50, &mut central)] {
            u32le(v, sig);
            if sig == 0x0201_4b50 {
                u16le(v, 20); // 만든 판
            }
            u16le(v, 20); // 풀 때 필요한 판
            u16le(v, 0); // 표시
            u16le(v, 8); // deflate
            u16le(v, DOS_TIME);
            u16le(v, DOS_DATE);
            u32le(v, crc);
            u32le(v, comp.len() as u32);
            u32le(v, data.len() as u32);
            u16le(v, name.len() as u16);
            u16le(v, 0); // 덧붙임
            if sig == 0x0201_4b50 {
                u16le(v, 0); // 설명
                u16le(v, 0); // 디스크
                u16le(v, 0); // 내부 속성
                u32le(v, 0); // 외부 속성
                u32le(v, offset);
            }
            v.extend_from_slice(name.as_bytes());
        }
        out.extend_from_slice(&comp);
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    u32le(&mut out, 0x0605_4b50);
    u16le(&mut out, 0);
    u16le(&mut out, 0);
    u16le(&mut out, files.len() as u16);
    u16le(&mut out, files.len() as u16);
    u32le(&mut out, central.len() as u32);
    u32le(&mut out, cd_offset);
    u16le(&mut out, 0);
    out
}

// ───────────────────────── PDF (HTML → WebView2 PrintToPdf) ─────────────────────────

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn h_runs(runs: &[Run]) -> String {
    let mut s = String::new();
    for r in runs {
        let t = esc(&r.text).replace('\n', "<br>");
        if r.time {
            s.push_str(&format!("<span class=\"pd-t\">{t}</span>"));
        } else if r.bold {
            s.push_str(&format!("<b>{t}</b>"));
        } else {
            s.push_str(&t);
        }
    }
    s
}

/// 인쇄용 HTML (화면의 `#print-doc` 에 넣는다 — 꾸밈은 ui/style.css 의 `.pd`)
pub(crate) fn html(doc: &SessionDoc, tr: TrMode) -> String {
    let mut s = String::from("<article class=\"pd\">");
    for b in blocks(doc, tr) {
        match b {
            Block::Title(t) => s.push_str(&format!("<h1 class=\"pd-title\">{}</h1>", esc(&t))),
            Block::Meta(rows) => {
                s.push_str("<table class=\"pd-meta\">");
                for (k, v) in rows {
                    s.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>", esc(&k), esc(&v)));
                }
                s.push_str("</table>");
            }
            Block::H1(t) => s.push_str(&format!("<h2>{}</h2>", esc(&t))),
            Block::H2(t) => s.push_str(&format!("<h3>{}</h3>", h_runs(&runs(&t)))),
            Block::Para(r) => s.push_str(&format!("<p>{}</p>", h_runs(&r))),
            Block::Item { depth, check, runs } => {
                let d = depth.min(3);
                match check {
                    None => s.push_str(&format!("<div class=\"pd-li\" data-d=\"{d}\">{}</div>", h_runs(&runs))),
                    Some(done) => s.push_str(&format!(
                        "<div class=\"pd-li pd-chk\" data-d=\"{d}\"><span class=\"pd-box\">{}</span>{}</div>",
                        if done { "☑" } else { "☐" },
                        h_runs(&runs)
                    )),
                }
            }
            Block::Table { head, widths, rows } => {
                s.push_str("<table class=\"pd-tbl\"><colgroup>");
                for w in &widths {
                    s.push_str(&format!("<col style=\"width:{w}%\">"));
                }
                s.push_str("</colgroup><thead><tr>");
                for h in &head {
                    s.push_str(&format!("<th>{}</th>", esc(h)));
                }
                s.push_str("</tr></thead><tbody>");
                for row in rows {
                    s.push_str("<tr>");
                    for cell in row {
                        s.push_str(&format!("<td>{}</td>", h_runs(&cell)));
                    }
                    s.push_str("</tr>");
                }
                s.push_str("</tbody></table>");
            }
            Block::Turn { name, color, time } => s.push_str(&format!(
                "<div class=\"pd-turn\"><b style=\"color:#{color}\">{}</b><span class=\"pd-t\">{}</span></div>",
                esc(&name),
                esc(&time)
            )),
            Block::Speech(t) => s.push_str(&format!("<p class=\"pd-sp\">{}</p>", esc(&t))),
            Block::Translated(t) => s.push_str(&format!("<p class=\"pd-tr\">🌐 {}</p>", esc(&t))),
            Block::Note { icon, time, text } => s.push_str(&format!(
                "<div class=\"pd-note\">{icon} <span class=\"pd-t\">{}</span> {}</div>",
                esc(&time),
                esc(&text).replace('\n', "<br>")
            )),
        }
    }
    s.push_str("</article>");
    s
}

/// 화면(WebView2)을 PDF 로 저장한다 — 화면이 인쇄용 칸(`#print-doc`)에 회의록을 넣고 `body.printing` 을 단 뒤에
/// 부른다 (인쇄 때는 그 칸만 보인다, ui/style.css). A4 세로 · 배경색 포함 · 머리글에 회의 제목, 바닥글에 쪽 번호.
/// 끝날 때까지 기다리므로 작업 스레드에서 부른다.
#[cfg(windows)]
pub(crate) fn print_pdf(win: &tauri::WebviewWindow, path: &std::path::Path, title: &str) -> Result<(), String> {
    use std::sync::mpsc;
    use std::time::Duration;

    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Environment6, ICoreWebView2_7, COREWEBVIEW2_PRINT_ORIENTATION_PORTRAIT,
    };
    use webview2_com::PrintToPdfCompletedHandler;
    use windows::core::{Interface, HSTRING, PCWSTR};

    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let path = HSTRING::from(path.as_os_str());
    let title = HSTRING::from(title);
    win.with_webview(move |w| {
        let done = tx.clone();
        let start = || -> windows::core::Result<()> {
            unsafe {
                let core = w.controller().CoreWebView2()?;
                let wv: ICoreWebView2_7 = core.cast()?;
                let env: ICoreWebView2Environment6 = w.environment().cast()?;
                let s = env.CreatePrintSettings()?;
                s.SetOrientation(COREWEBVIEW2_PRINT_ORIENTATION_PORTRAIT)?;
                // A4 (인치)
                s.SetPageWidth(8.27)?;
                s.SetPageHeight(11.69)?;
                s.SetMarginTop(0.55)?;
                s.SetMarginBottom(0.55)?;
                s.SetMarginLeft(0.75)?;
                s.SetMarginRight(0.75)?;
                s.SetShouldPrintBackgrounds(true)?;
                // 머리글: 날짜 · 회의 제목, 바닥글: 쪽 번호 (주소 칸은 비운다 — 앱 안 주소라 뜻이 없다)
                s.SetShouldPrintHeaderAndFooter(true)?;
                s.SetHeaderTitle(PCWSTR(title.as_ptr()))?;
                let empty = HSTRING::new();
                s.SetFooterUri(PCWSTR(empty.as_ptr()))?;
                let handler = PrintToPdfCompletedHandler::create(Box::new(move |hr, ok| {
                    let r = match hr {
                        Ok(()) if ok => Ok(()),
                        Ok(()) => Err("PDF 를 만들지 못했어요 (파일을 쓸 수 없거나 다른 인쇄가 진행 중이에요)".to_string()),
                        Err(e) => Err(format!("PDF 를 만들지 못했어요: {e}")),
                    };
                    let _ = done.send(r);
                    Ok(())
                }));
                wv.PrintToPdf(PCWSTR(path.as_ptr()), &s, &handler)?;
            }
            Ok(())
        };
        if let Err(e) = start() {
            let _ = tx.send(Err(format!("PDF 를 만들지 못했어요: {e}")));
        }
    })
    .map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(120)).map_err(|_| "PDF 를 만드는 데 너무 오래 걸려요".to_string())?
}

#[cfg(not(windows))]
pub(crate) fn print_pdf(_win: &tauri::WebviewWindow, _path: &std::path::Path, _title: &str) -> Result<(), String> {
    Err("PDF 저장은 Windows 에서만 돼요".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read as _;

    const SUMMARY: &str = "### 한 줄 요약\n출시 일정 · 응답 속도\n\n### 주요 논의\n- **출시 일정**\n  - ⭐ 15일 출시, 목요일 점검 [00:25]\n\n\
                           ### 결정 사항\n- 출시일 **15일** 확정 [00:25]\n\n### 할 일\n- [ ] 결제 모듈 배포 — 담당: 이 개발 / 기한: 금요일 [00:07]\n\
                           - [x] 온보딩 시안 [00:17] [00:20]\n- [ ] 없음\n\n### 남은 질문\n- 없음";

    fn doc() -> SessionDoc {
        serde_json::from_value(json!({
            "version": 1, "id": "m1", "title": "앱 <출시> & 일정", "startedAt": "2026-11-19 14:30", "durationMs": 125_000,
            "engine": "whisper", "summary": SUMMARY,
            "speakers": { "1": "김 팀장" },
            "utterances": [
                { "id": 1, "startMs": 1000, "endMs": 4000, "speaker": 0, "text": "시작하겠습니다." },
                { "id": 2, "startMs": 5000, "endMs": 9000, "speaker": 0, "text": "결제 모듈은 금요일까지." },
                { "id": 3, "startMs": 30000, "endMs": 40000, "speaker": 1, "text": "<script>alert(1)</script> 좋아요\u{1}" }
            ],
            "notes": "준비물: 노트북\n[00:05] ⭐\n[00:31] 확인 필요",
            "qa": [{ "q": "기한은?", "a": "- 금요일 [00:05]", "at": "2026-11-19 15:00", "model": "x" }]
        }))
        .unwrap()
    }

    #[test]
    fn runs_split_bold_and_time() {
        assert_eq!(
            runs("**출시** 15일 [00:25] [메모] 끝"),
            vec![Run::bold("출시"), Run::plain(" 15일 "), Run::time("[00:25]"), Run::plain(" [메모] 끝")]
        );
        assert_eq!(runs("[1:02:03]"), vec![Run::time("[1:02:03]")]);
        assert_eq!(runs("열린 [괄호"), vec![Run::plain("열린 [괄호")]);
    }

    #[test]
    fn summary_sections_and_todo_table() {
        let b = blocks(&doc(), TrMode::Orig);
        assert_eq!(b[0], Block::Title("앱 <출시> & 일정".into()));
        let Block::Meta(meta) = &b[1] else { panic!("메타") };
        assert!(meta.contains(&("참석".into(), "화자 1 · 김 팀장  (2명)".into())), "{meta:?}");
        assert!(meta.contains(&("길이".into(), "2분 5초".into())));
        assert!(b.contains(&Block::H2("할 일".into())));
        let table = b
            .iter()
            .find_map(|x| match x {
                Block::Table { head, rows, .. } if head[1] == "할 일" => Some((head.clone(), rows.clone())),
                _ => None,
            })
            .expect("할 일 표");
        assert_eq!(table.0, vec!["확인", "할 일", "담당", "기한"]);
        assert_eq!(table.1.len(), 2, "'없음' 줄은 빼고");
        assert_eq!(table.1[0][1], vec![Run::plain("결제 모듈 배포"), Run::plain(" "), Run::time("[00:07]")]);
        assert_eq!(table.1[0][2], vec![Run::plain("이 개발")]);
        assert_eq!(table.1[0][3], vec![Run::plain("금요일")]);
        assert_eq!(table.1[1][0], vec![Run::plain("☑")], "끝낸 할 일");
        assert!(table.1[1][2].is_empty(), "담당 없음");
        // 주요 논의는 목록 (깊이 · 굵게 · 근거 시각)
        assert!(b.contains(&Block::Item { depth: 0, check: None, runs: vec![Run::bold("출시 일정")] }));
        assert!(b.contains(&Block::Item {
            depth: 1,
            check: None,
            runs: vec![Run::plain("⭐ 15일 출시, 목요일 점검 "), Run::time("[00:25]")]
        }));
    }

    #[test]
    fn transcript_comments_memo_and_stats() {
        let b = blocks(&doc(), TrMode::Orig);
        // 메모 칸에는 코멘트가 아닌 줄만
        let memo = b.iter().position(|x| *x == Block::H1("메모".into())).expect("메모");
        assert_eq!(b[memo + 1], Block::Para(vec![Run::plain("준비물: 노트북")]));
        assert!(matches!(b[memo + 2], Block::H1(_)), "코멘트 줄은 메모에 다시 싣지 않는다");
        // 코멘트는 그 문장 문단 아래
        let turn = b.iter().position(|x| matches!(x, Block::Turn { time, .. } if time == "00:01")).expect("문단");
        assert_eq!(b[turn + 1], Block::Speech("시작하겠습니다. 결제 모듈은 금요일까지.".into()));
        assert_eq!(b[turn + 2], Block::Note { icon: "⭐", time: "00:05".into(), text: "중요 표시".into() });
        assert_eq!(b[turn + 3], Block::Turn { name: "김 팀장".into(), color: COLORS[1], time: "00:30".into() });
        assert_eq!(b.last(), Some(&Block::Note { icon: "💬", time: "00:31".into(), text: "확인 필요".into() }));
        assert!(b.contains(&Block::H1("발언 통계".into())));
        assert!(b.contains(&Block::Para(vec![Run::bold("Q. 기한은?")])));
    }

    /// zip 을 풀어 이름 → 글 (테스트용 — 우리가 쓴 형식만 읽는다)
    fn unzip(z: &[u8]) -> Vec<(String, String)> {
        let rd16 = |i: usize| u16::from_le_bytes([z[i], z[i + 1]]) as usize;
        let rd32 = |i: usize| u32::from_le_bytes([z[i], z[i + 1], z[i + 2], z[i + 3]]) as usize;
        let end = z.len() - 22;
        assert_eq!(rd32(end), 0x0605_4b50);
        let (n, mut p) = (rd16(end + 10), rd32(end + 16));
        let mut out = Vec::new();
        for _ in 0..n {
            assert_eq!(rd32(p), 0x0201_4b50);
            let (csize, usize_, nlen, off) = (rd32(p + 20), rd32(p + 24), rd16(p + 28), rd32(p + 42));
            let name = String::from_utf8(z[p + 46..p + 46 + nlen].to_vec()).unwrap();
            assert_eq!(rd32(off), 0x0403_4b50);
            let data = &z[off + 30 + nlen..off + 30 + nlen + csize];
            let mut text = String::new();
            flate2::read::DeflateDecoder::new(data).read_to_string(&mut text).unwrap();
            assert_eq!(text.len(), usize_);
            assert_eq!(crc32fast::hash(text.as_bytes()), rd32(p + 16) as u32);
            out.push((name, text));
            p += 46 + nlen;
        }
        out
    }

    #[test]
    fn docx_is_a_zip_with_word_parts() {
        let files = unzip(&docx(&doc(), TrMode::Orig));
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names[0], "[Content_Types].xml");
        for need in ["_rels/.rels", "word/document.xml", "word/styles.xml", "word/numbering.xml", "word/footer1.xml"] {
            assert!(names.contains(&need), "{need}");
        }
        let body = &files.iter().find(|(n, _)| n == "word/document.xml").unwrap().1;
        assert!(body.contains("앱 &lt;출시&gt; &amp; 일정"), "제목 이스케이프");
        assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt; 좋아요<"), "글 이스케이프 · 제어 문자 빼기");
        assert!(!body.contains('\u{1}'));
        assert!(body.contains("<w:pStyle w:val=\"Heading1\"/>"));
        assert!(body.contains("<w:numId w:val=\"1\"/>"));
        assert!(body.contains(">☑<"), "끝낸 할 일");
        // 그림 문자는 맑은 고딕에 있는 기호로 (⭐ → ★, 코멘트 💬 → ※)
        assert!(!body.contains('⭐') && !body.contains('💬'));
        assert!(body.contains(">★ <") && body.contains(">※ <"));
        let footer = &files.iter().find(|(n, _)| n == "word/footer1.xml").unwrap().1;
        assert!(footer.contains("<w:pStyle w:val=\"Footer\"/>") && footer.contains(" PAGE "), "바닥글 쪽 번호");
        // 여는 · 닫는 태그 수가 맞다 (문단 · 표 · 행 · 칸)
        for tag in ["w:p", "w:tbl", "w:tr", "w:tc", "w:r"] {
            let open = body.matches(&format!("<{tag}>")).count() + body.matches(&format!("<{tag} ")).count();
            let close = body.matches(&format!("</{tag}>")).count();
            let empty = body.matches(&format!("<{tag}/>")).count();
            assert_eq!(open, close, "{tag}: 여는 {open} · 닫는 {close} (빈 {empty})");
        }
        assert!(body.ends_with("</w:sectPr></w:body></w:document>"));
        // 같은 회의는 늘 같은 파일
        assert_eq!(docx(&doc(), TrMode::Orig), docx(&doc(), TrMode::Orig));
    }

    #[test]
    fn html_is_escaped() {
        let h = html(&doc(), TrMode::Orig);
        assert!(h.starts_with("<article class=\"pd\">") && h.ends_with("</article>"));
        assert!(h.contains("<h1 class=\"pd-title\">앱 &lt;출시&gt; &amp; 일정</h1>"));
        assert!(h.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!h.contains("<script>"));
        assert!(h.contains("<span class=\"pd-t\">[00:07]</span>"));
        assert!(!h.contains("pd-chk"), "할 일은 표로, '- [ ] 없음' 은 체크 칸 없이");
        assert!(h.contains("<div class=\"pd-li\" data-d=\"0\">없음</div>"));
        assert!(h.contains("<th>담당</th>"));
    }

    #[test]
    fn transcript_with_translation() {
        let mut d = doc();
        crate::translate::merge(
            &mut d.translation,
            "en",
            vec![
                (1, crate::translate::TrItem { src: "시작하겠습니다.".into(), text: "Let's begin.".into() }),
                // 원문이 바뀐 문장의 번역은 쓰지 않는다
                (2, crate::translate::TrItem { src: "결제 모듈은 목요일까지.".into(), text: "By Thursday.".into() }),
            ],
        );
        let speech = |b: &[Block]| -> Vec<Block> {
            b.iter().filter(|x| matches!(x, Block::Speech(_) | Block::Translated(_))).cloned().collect()
        };
        let both = speech(&blocks(&d, TrMode::Both));
        assert_eq!(both[0], Block::Speech("시작하겠습니다. 결제 모듈은 금요일까지.".into()));
        assert_eq!(both[1], Block::Translated("Let's begin.".into()));
        assert!(matches!(both[2], Block::Speech(_)), "번역이 없는 문단은 원문만");
        // 번역만: 번역 안 된(낡은) 문장은 원문으로 채운다 — 문단 일부만 번역돼도 문장이 빠지지 않게
        let tr = speech(&blocks(&d, TrMode::Tr));
        assert_eq!(tr[0], Block::Speech("Let's begin. 결제 모듈은 금요일까지.".into()));
        assert_eq!(tr.len(), 2);
        assert!(html(&d, TrMode::Both).contains("<p class=\"pd-tr\">🌐 Let's begin.</p>"));
        assert_eq!(speech(&blocks(&d, TrMode::Orig)).len(), 2, "원문만");
        // Markdown · 텍스트 · 자막도
        assert!(d.markdown(TrMode::Both).contains("시작하겠습니다. 결제 모듈은 금요일까지.\n\n🌐 Let's begin.\n\n"));
        assert!(d.text(TrMode::Tr).starts_with("[00:01] 화자 1: Let's begin. 결제 모듈은 금요일까지.\n"));
        assert!(d.markdown(TrMode::Tr).contains("  \nLet's begin. 결제 모듈은 금요일까지.\n\n"));
        assert!(d.srt(TrMode::Both).contains("화자 1: 시작하겠습니다.\nLet's begin.\n\n"));
        assert!(d.srt(TrMode::Both).contains("화자 1: 결제 모듈은 금요일까지.\n\n"), "낡은 번역은 싣지 않는다");
    }

    #[test]
    fn empty_meeting_still_makes_a_document() {
        let d: SessionDoc = serde_json::from_value(json!({ "version": 1, "id": "x", "title": "" })).unwrap();
        let b = blocks(&d, TrMode::Orig);
        assert_eq!(b[0], Block::Title("회의록".into()));
        assert_eq!(b.len(), 2, "제목 · 메타만");
        let files = unzip(&docx(&d, TrMode::Orig));
        assert!(files.iter().any(|(n, t)| n == "word/document.xml" && t.contains("<w:p/><w:sectPr>")));
    }
}
