//! 할 일 · 후속 메일 보내기 좋게 — 이미 만든 요약(Markdown)을 다시 쓴다 (AI 를 다시 부르지 않는다)
//!
//! - 메신저용 짧은 정리 ([`messenger`]): 한 줄 요약 + 정한 것 · 할 일 · 남은 질문 · 다음 단계 — Teams · Slack ·
//!   카카오톡에 붙여 넣기 좋게 (제목 줄 `###` · 굵게 `**` · 근거 시각 `[12:34]` 없이)
//! - 후속 메일 초안 ([`email`] · [`eml`]): 요약 전체를 메일 본문으로. `.eml` 에 `X-Unsent: 1` 을 달면 Outlook 이
//!   받은 메일이 아니라 보낼 새 메일 창으로 연다
//! - 기한이 있는 할 일 → 일정 파일 ([`ics`]): 하루 종일 일정(바쁨 표시 없음), 전날 오전 9시 알림. 기한 글
//!   ("금요일" · "다음 주 화요일" · "11월 25일" · "25일" · "내일")은 회의 날짜를 기준으로 날짜로 바꾼다 ([`due_date`])
//!
//! 요약 틀마다 섹션 이름이 달라(결정 사항 · 합의한 것 · 정한 것 …) 이름으로 종류를 어림한다 ([`kind`])

use base64::Engine as _;
use chrono::{Datelike, Duration, NaiveDate, Weekday};

use crate::session::SessionDoc;

/// 요약의 섹션 하나 ("### 이름" 아래 줄들)
#[derive(Debug, Default)]
struct Section {
    name: String,
    lines: Vec<Line>,
}

/// 섹션 안의 줄 — 목록이면 들여쓰기 깊이(0 = 맨 바깥), 할 일 체크 목록인지
#[derive(Debug)]
struct Line {
    depth: usize,
    text: String,
    todo: bool,
    /// 끝낸 할 일 (`- [x]`) — 메신저 · 메일에는 "(완료)" 를 붙이고, 일정 파일 · 할 일 수에서는 뺀다
    done: bool,
}

/// 섹션 종류 (요약 틀마다 이름이 달라 이름으로 어림한다)
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    OneLine,
    Decision,
    Todo,
    Open,
    Next,
    Body,
}

pub(crate) fn kind(name: &str) -> Kind {
    let has = |w: &[&str]| w.iter().any(|w| name.contains(w));
    if name == "한 줄 요약" {
        Kind::OneLine
    } else if has(&["할 일"]) {
        Kind::Todo
    } else if has(&["결정", "합의", "정한"]) {
        Kind::Decision
    } else if has(&["남은 질문", "확인할", "알아볼"]) {
        Kind::Open
    } else if name.starts_with("다음") {
        Kind::Next
    } else {
        Kind::Body
    }
}

/// 근거 시각 `[12:34]` · `[1:02:03]` · 굵게 `**` 를 빼고 빈칸을 하나로
pub(crate) fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find(']') {
            Some(j) if j > 0 && after[..j].chars().all(|c| c.is_ascii_digit() || c == ':') && after[..j].contains(':') => {
                rest = &after[j + 1..];
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "없음" · "없음." 처럼 내용이 없다는 줄
pub(crate) fn is_none(text: &str) -> bool {
    let t = text.trim_end_matches(['.', '。']).trim();
    t == "없음" || t == "해당 없음" || t == "-"
}

/// 요약 Markdown → 섹션들 (맨 앞 제목 없는 줄은 이름 없는 섹션)
fn parse(summary: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for raw in summary.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            out.push(Section { name: clean(trimmed.trim_start_matches('#')), lines: Vec::new() });
            continue;
        }
        if out.is_empty() {
            out.push(Section::default());
        }
        let indent = line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 2 } else { 1 }).sum::<usize>();
        let (depth, body) = match ["- ", "* ", "• ", "· "].iter().find_map(|b| trimmed.strip_prefix(b)) {
            Some(b) => (indent / 2, b),
            None => (0, trimmed),
        };
        let (todo, done, body) = if let Some(b) = body.strip_prefix("[ ] ") {
            (true, false, b)
        } else if let Some(b) = body.strip_prefix("[x] ").or_else(|| body.strip_prefix("[X] ")) {
            (true, true, b)
        } else {
            (false, false, body)
        };
        let text = clean(body);
        if !text.is_empty() {
            out.last_mut().unwrap().lines.push(Line { depth, text, todo, done });
        }
    }
    out
}

/// 할 일 한 줄
#[derive(Debug, Clone, PartialEq)]
pub struct Todo {
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
}

/// "할 일 — 담당: 이 개발 / 기한: 금요일" → 할 일 · 담당 · 기한 ("(화자)" · "미정" · "(언급된 경우)" 같은 빈 값은 없음)
pub(crate) fn todo_of(text: &str) -> Todo {
    let field = |key: &str| -> Option<String> {
        let i = text.find(key)?;
        let v = text[i + key.len()..].split(" / ").next().unwrap_or("").trim().trim_end_matches(['.', ',']).trim();
        let empty = v.is_empty() || v.starts_with('(') || ["미정", "없음", "-", "?"].contains(&v);
        (!empty).then(|| v.to_string())
    };
    let cut = ["— 담당", "- 담당", "– 담당", "— 기한", "- 기한", " 담당:", " 기한:"]
        .iter()
        .filter_map(|k| text.find(k))
        .min()
        .unwrap_or(text.len());
    Todo { text: text[..cut].trim().trim_end_matches(['—', '-', '–']).trim().to_string(), owner: field("담당:"), due: field("기한:") }
}

fn todos(sections: &[Section]) -> Vec<Todo> {
    // 끝낸 할 일은 빼고
    sections.iter().flat_map(|s| &s.lines).filter(|l| l.todo && !l.done && !is_none(&l.text)).map(|l| todo_of(&l.text)).collect()
}

/// 메신저 · 메일에 쓰는 줄 — 할 일은 [`todo_line`], 끝낸 할 일은 끝에 "(완료)"
fn line_text(l: &Line) -> String {
    match (l.todo, l.done) {
        (true, false) => todo_line(&todo_of(&l.text)),
        (true, true) => format!("{} (완료)", todo_line(&todo_of(&l.text))),
        _ => l.text.clone(),
    }
}

/// 메신저 · 메일에 쓰는 할 일 한 줄: "할 일 — 이 개발 · 금요일까지"
fn todo_line(t: &Todo) -> String {
    let mut tail = Vec::new();
    if let Some(o) = &t.owner {
        tail.push(o.clone());
    }
    if let Some(d) = &t.due {
        tail.push(if d.ends_with("까지") { d.clone() } else { format!("{d}까지") });
    }
    if tail.is_empty() {
        t.text.clone()
    } else {
        format!("{} — {}", t.text, tail.join(" · "))
    }
}

/// 회의 날짜 ("2026-11-19 14:30" → 2026-11-19)
fn meeting_date(doc: &SessionDoc) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(doc.started_at.get(..10)?, "%Y-%m-%d").ok()
}

const WEEKDAYS: [&str; 7] = ["월", "화", "수", "목", "금", "토", "일"];

/// "앱 출시 일정 회의" 는 그대로, "앱 출시 일정" 은 "앱 출시 일정 회의" ("… 회의 회의" 가 되지 않게)
fn meeting_name(title: &str) -> String {
    let t = title.trim();
    if ["회의", "미팅", "면담", "면접", "인터뷰", "강의", "세미나", "워크숍", "보고"].iter().any(|w| t.ends_with(w)) {
        t.to_string()
    } else {
        format!("{t} 회의")
    }
}

/// "11/19(수)"
fn short_date(doc: &SessionDoc) -> String {
    match meeting_date(doc) {
        Some(d) => format!("{}/{}({})", d.month(), d.day(), WEEKDAYS[d.weekday().num_days_from_monday() as usize]),
        None => doc.started_at.clone(),
    }
}

/// 메신저용 짧은 정리. 요약이 없으면 None
pub fn messenger(doc: &SessionDoc) -> Option<String> {
    let sections = parse(doc.summary.as_deref().filter(|s| !s.trim().is_empty())?);
    let mut blocks = vec![format!("[{}] {} 회의 정리", doc.title.trim(), short_date(doc))];
    if let Some(one) = sections.iter().find(|s| kind(&s.name) == Kind::OneLine).and_then(|s| s.lines.first()) {
        blocks[0].push('\n');
        blocks[0].push_str(&one.text);
    }
    let block = |s: &Section, icon: &str| -> Option<String> {
        let items: Vec<String> = s
            .lines
            .iter()
            .filter(|l| l.depth == 0 && !is_none(&l.text))
            .map(|l| format!("• {}", line_text(l)))
            .collect();
        (!items.is_empty()).then(|| format!("{icon} {}\n{}", s.name, items.join("\n")))
    };
    let mut wrote = false;
    for (k, icon) in [(Kind::Decision, "✅"), (Kind::Todo, "📌"), (Kind::Open, "❓"), (Kind::Next, "➡️")] {
        for s in sections.iter().filter(|s| kind(&s.name) == k) {
            if let Some(b) = block(s, icon) {
                blocks.push(b);
                wrote = true;
            }
        }
    }
    // 정한 것 · 할 일이 없는 틀(강의 등)은 본문 첫 섹션의 큰 줄기만
    if !wrote {
        if let Some(b) = sections.iter().find(|s| kind(&s.name) == Kind::Body && !s.name.is_empty()).and_then(|s| block(s, "📝")) {
            blocks.push(b);
        }
    }
    Some(blocks.join("\n\n"))
}

/// 후속 메일 (제목, 본문). 요약이 없으면 None
pub fn email(doc: &SessionDoc) -> Option<(String, String)> {
    let sections = parse(doc.summary.as_deref().filter(|s| !s.trim().is_empty())?);
    let date = meeting_date(doc);
    let when = date.map(|d| format!("{}월 {}일 ", d.month(), d.day())).unwrap_or_default();
    let subject = format!("[회의록] {} ({})", doc.title.trim(), date.map(|d| format!("{}/{}", d.month(), d.day())).unwrap_or_default());
    let mut body = format!("안녕하세요.\n{when}{} 내용을 정리해 공유드립니다.\n", meeting_name(&doc.title));
    if let Some(one) = sections.iter().find(|s| kind(&s.name) == Kind::OneLine).and_then(|s| s.lines.first()) {
        body.push_str(&format!("\n{}\n", one.text));
    }
    for s in sections.iter().filter(|s| kind(&s.name) != Kind::OneLine) {
        let lines: Vec<&Line> = s.lines.iter().filter(|l| !is_none(&l.text)).collect();
        if lines.is_empty() {
            continue;
        }
        if !s.name.is_empty() {
            body.push_str(&format!("\n■ {}\n", s.name));
        }
        for l in lines {
            let text = line_text(l);
            match l.depth {
                0 => body.push_str(&format!("- {text}\n")),
                _ => body.push_str(&format!("{}· {text}\n", "   ".repeat(l.depth))),
            }
        }
    }
    body.push_str("\n감사합니다.\n");
    Some((subject, body))
}

/// 새 메일로 여는 .eml — `X-Unsent: 1` 이면 Outlook 이 보낼 메일 창으로 연다 (받는 사람은 비워 둔다)
pub fn eml(subject: &str, body: &str) -> String {
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    let crlf = body.replace("\r\n", "\n").replace('\n', "\r\n");
    let encoded = b64(&crlf);
    let wrapped: Vec<&str> =
        encoded.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).expect("base64 는 ASCII")).collect();
    format!(
        "X-Unsent: 1\r\nSubject: =?UTF-8?B?{}?=\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
        b64(subject),
        wrapped.join("\r\n")
    )
}

/// 할 일 수 (전체, 기한을 날짜로 읽을 수 있는 것)
pub fn todo_counts(doc: &SessionDoc) -> (usize, usize) {
    let Some(sum) = doc.summary.as_deref() else { return (0, 0) };
    let all = todos(&parse(sum));
    let base = meeting_date(doc);
    let dated = all.iter().filter(|t| base.zip(t.due.as_deref()).and_then(|(b, d)| due_date(d, b)).is_some()).count();
    (all.len(), dated)
}

fn weekday_of(s: &str) -> Option<(usize, Weekday)> {
    let days = [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat, Weekday::Sun];
    WEEKDAYS.iter().zip(days).find_map(|(k, w)| s.find(&format!("{k}요일")).map(|i| (i, w)))
}

/// 앞에서 이어지는 숫자 (값, 다음 위치)
fn number(s: &str) -> Option<(u32, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    (end > 0).then(|| (s[..end].parse().ok(), &s[end..])).and_then(|(n, rest)| Some((n?, rest)))
}

fn last_day(y: i32, m: u32) -> Option<NaiveDate> {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1)?.pred_opt()
}

/// 월 · 일만 있는 날짜: 회의 날짜보다 일주일 넘게 앞이면 다음 해
fn month_day(base: NaiveDate, m: u32, d: u32) -> Option<NaiveDate> {
    let this = NaiveDate::from_ymd_opt(base.year(), m, d)?;
    if this < base - Duration::days(7) {
        NaiveDate::from_ymd_opt(base.year() + 1, m, d)
    } else {
        Some(this)
    }
}

/// 기한 글 → 날짜 (회의 날짜 기준). 읽을 수 없으면 None — 그 할 일은 일정 파일에 넣지 않는다
pub fn due_date(text: &str, base: NaiveDate) -> Option<NaiveDate> {
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let compact: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    // 2026-11-25 · 2026.11.25 · 2026/11/25
    for sep in ['-', '.', '/'] {
        let parts: Vec<&str> = compact.split(sep).collect();
        if parts.len() >= 3 && parts[0].len() == 4 {
            if let (Some((y, _)), Some((m, _)), Some((d, _))) = (number(parts[0]), number(parts[1]), number(parts[2])) {
                return NaiveDate::from_ymd_opt(y as i32, m, d);
            }
        }
    }
    // `end` 바로 앞에 붙은 숫자 ("2027년" 의 2027 · "12월" 의 12)
    let digits_before = |end: usize| -> Option<u32> {
        let start = compact[..end].rfind(|c: char| !c.is_ascii_digit()).map_or(0, |j| j + compact[j..].chars().next().unwrap().len_utf8());
        number(&compact[start..end]).map(|(n, _)| n)
    };
    // "2027년" 처럼 적힌 해 (두 자리면 2000년대 — 27년은 2027년)
    // "내년" · "올해" 도 해로
    let year_before = |end: usize| {
        let head = &compact[..end];
        if head.contains("내년") {
            return Some(base.year() as u32 + 1);
        }
        if head.contains("올해") || head.contains("금년") {
            return Some(base.year() as u32);
        }
        head.find('년').and_then(|y| digits_before(y)).map(|y| if y < 100 { 2000 + y } else { y })
    };
    // 11월 25일 · 2027년 11월 25일 (연도가 적혀 있으면 그 해)
    if let Some(i) = compact.find('월') {
        if let (Some(m), Some((d, rest))) = (digits_before(i), number(&compact[i + '월'.len_utf8()..])) {
            if rest.starts_with('일') {
                let year = year_before(i);
                return match year {
                    Some(y) => NaiveDate::from_ymd_opt(y as i32, m, d),
                    None => month_day(base, m, d),
                };
            }
        }
    }
    // 이번 달 28일 · 다음 달 1일 · 다다음 달 5일
    let months = if compact.contains("다다음달") {
        Some(2)
    } else if compact.contains("다음달") {
        Some(1)
    } else if compact.contains("이번달") {
        Some(0)
    } else {
        None
    };
    if let (Some(k), Some(i)) = (months, compact.find('달')) {
        if let Some((d, rest)) = number(&compact[i + '달'.len_utf8()..]) {
            if rest.starts_with('일') {
                let m0 = base.month0() + k;
                return NaiveDate::from_ymd_opt(base.year() + (m0 / 12) as i32, m0 % 12 + 1, d);
            }
        }
    }
    // 11/25 · 11.25
    for sep in ['/', '.'] {
        if let Some((a, b)) = compact.split_once(sep) {
            if let (Some((m, "")), Some((d, _))) = (number(a.trim_start_matches(|c: char| !c.is_ascii_digit())), number(b)) {
                if (1..=12).contains(&m) {
                    return month_day(base, m, d);
                }
            }
        }
    }
    let plus = |n: i64| Some(base + Duration::days(n));
    // 오늘 · 내일 · 모레 · 글피
    for (w, n) in [("오늘", 0), ("내일", 1), ("모레", 2), ("글피", 3)] {
        if compact.starts_with(w) {
            return plus(n);
        }
    }
    // 3일 뒤 · 2주 후 · 일주일 내
    if compact.starts_with("일주일") {
        return plus(7);
    }
    if let Some((n, rest)) = number(&compact) {
        let after = ["후", "뒤", "내", "안", "이내"].iter().any(|w| rest.trim_start_matches(['일', '주']).starts_with(w));
        if rest.starts_with('주') && after {
            return plus(7 * n as i64);
        }
        if rest.starts_with('일') && after {
            return plus(n as i64);
        }
        // 25일 — 이번 달, 이미 지났으면 다음 달
        if rest.starts_with('일') {
            let this = NaiveDate::from_ymd_opt(base.year(), base.month(), n);
            return match this {
                Some(d) if d >= base => Some(d),
                _ => {
                    let (y, m) = if base.month() == 12 { (base.year() + 1, 1) } else { (base.year(), base.month() + 1) };
                    NaiveDate::from_ymd_opt(y, m, n)
                }
            };
        }
    }
    // 월말 · 말일 · 이번 달 말 · 다음 달 말 · 12월 말 · 2027년 1월 말일
    if compact.contains("월말") || compact.contains("말일") || compact.contains("달말") {
        // 달이 적혀 있으면 그 달의 마지막 날 (해가 없으면 회의보다 일주일 넘게 앞일 때 다음 해)
        if let Some(m) = compact.find('월').and_then(|i| Some((digits_before(i)?, i))) {
            let (m, i) = m;
            return match year_before(i) {
                Some(y) => last_day(y as i32, m),
                None => last_day(base.year(), m)
                    .and_then(|d| if d < base - Duration::days(7) { last_day(base.year() + 1, m) } else { Some(d) }),
            };
        }
        // 이번 달 · 다음 달 · 다다음 달 (위에서 읽은 달 차이)
        let m0 = base.month0() + months.unwrap_or(0);
        return last_day(base.year() + (m0 / 12) as i32, m0 % 12 + 1);
    }
    // 달 · 날짜가 적혀 있는데 위에서 다 읽지 못했으면("11월 셋째 주 금요일" · "다음 달 첫째 주 화요일") 요일만 보고
    // 짐작하지 않는다 — 더 이른 날짜로 내보내느니 빼는 게 낫다
    let has_day = compact.char_indices().any(|(i, c)| c == '일' && i > 0 && compact[..i].ends_with(|p: char| p.is_ascii_digit()));
    if compact.contains('달') || compact.find('월').and_then(|i| digits_before(i)).is_some() || has_day {
        return None;
    }
    // (이번 주 · 다음 주 · 다다음 주) 금요일 — 주는 월요일부터
    let monday = base - Duration::days(base.weekday().num_days_from_monday() as i64);
    let weeks = if compact.contains("다다음주") {
        Some(2)
    } else if compact.contains("다음주") || compact.contains("차주") {
        Some(1)
    } else if compact.contains("이번주") || compact.contains("금주") {
        Some(0)
    } else {
        None
    };
    if let Some((_, w)) = weekday_of(&compact) {
        let day = w.num_days_from_monday() as i64;
        return match weeks {
            Some(k) => Some(monday + Duration::days(7 * k + day)),
            // 요일만: 회의 다음 날부터 처음 오는 그 요일
            None => {
                let ahead = (day - base.weekday().num_days_from_monday() as i64 + 7) % 7;
                plus(if ahead == 0 { 7 } else { ahead })
            }
        };
    }
    // 주말 → 토요일
    if compact.contains("주말") {
        let k = weeks.unwrap_or(0);
        let sat = monday + Duration::days(7 * k + 5);
        return Some(if sat < base { sat + Duration::days(7) } else { sat });
    }
    None
}

/// 일정 파일 글 한 줄을 75바이트로 접는다 (RFC 5545) — 이어지는 줄은 빈칸 하나로 시작
fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in line.chars() {
        let n = c.len_utf8();
        if used + n > 75 {
            out.push_str("\r\n ");
            used = 1;
        }
        out.push(c);
        used += n;
    }
    out.push_str("\r\n");
    out
}

fn ics_text(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n")
}

/// 기한이 있는 할 일 → 일정 파일 (하루 종일 · 바쁨 표시 없음 · 전날 오전 9시 알림). (파일 글, 넣은 수)
pub fn ics(doc: &SessionDoc, now_utc: &str) -> (String, usize) {
    let base = meeting_date(doc);
    let all = doc.summary.as_deref().map(|s| todos(&parse(s))).unwrap_or_default();
    let mut out = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//badasugi//todo//KO\r\nCALSCALE:GREGORIAN\r\nMETHOD:PUBLISH\r\n");
    let mut n = 0;
    for (i, t) in all.iter().enumerate() {
        let Some(day) = base.zip(t.due.as_deref()).and_then(|(b, d)| due_date(d, b)) else { continue };
        n += 1;
        let who = t.owner.as_deref().map(|o| format!(" ({o})")).unwrap_or_default();
        let mut desc = format!("회의: {} · {}", doc.title.trim(), short_date(doc));
        if let Some(o) = &t.owner {
            desc.push_str(&format!("\n담당: {o}"));
        }
        desc.push_str(&format!("\n기한: {}", t.due.as_deref().unwrap_or("")));
        out.push_str("BEGIN:VEVENT\r\n");
        out.push_str(&fold(&format!("UID:{}-todo-{i}@badasugi", doc.id)));
        out.push_str(&format!("DTSTAMP:{now_utc}\r\n"));
        out.push_str(&format!("DTSTART;VALUE=DATE:{}\r\n", day.format("%Y%m%d")));
        out.push_str(&format!("DTEND;VALUE=DATE:{}\r\n", (day + Duration::days(1)).format("%Y%m%d")));
        out.push_str(&fold(&format!("SUMMARY:{}", ics_text(&format!("[할 일] {}{who}", t.text)))));
        out.push_str(&fold(&format!("DESCRIPTION:{}", ics_text(&desc))));
        out.push_str("TRANSP:TRANSPARENT\r\n");
        out.push_str("BEGIN:VALARM\r\nACTION:DISPLAY\r\n");
        out.push_str(&fold(&format!("DESCRIPTION:{}", ics_text(&format!("[할 일] {}", t.text)))));
        out.push_str("TRIGGER:-PT15H\r\nEND:VALARM\r\n");
        out.push_str("END:VEVENT\r\n");
    }
    out.push_str("END:VCALENDAR\r\n");
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SUMMARY: &str = "### 한 줄 요약\n다음 달 15일 앱 출시 확정, 목요일 최종 점검\n\n### 주요 논의\n- **출시 일정**\n  - ⭐ 15일 출시, 목요일 점검 (김 팀장) [00:25]\n- **키오스크 응답 속도**\n  - 인덱스 추가로 2초 → 0.4초 (이 개발) [00:34]\n\n### 결정 사항\n- ⭐ 출시일 **15일** 확정 [00:25]\n\n### 할 일\n- [ ] 결제 모듈 배포 — 담당: 이 개발 / 기한: 금요일 [00:07]\n- [ ] 온보딩 시안 2종 — 담당: 박 디자인 / 기한: 11월 25일 [00:17]\n- [ ] 매장 피드백 정리 — 담당: (화자) / 기한: (언급된 경우) [00:46] [00:52]\n\n### 남은 질문\n- 없음";

    fn doc(summary: &str) -> SessionDoc {
        serde_json::from_value(json!({
            "version": 1, "id": "m1", "title": "앱 출시 일정 회의", "startedAt": "2026-11-19 14:30", "durationMs": 60_000,
            "engine": "x", "utterances": [], "summary": summary
        }))
        .unwrap()
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn todos_are_split_into_text_owner_and_due() {
        let all = todos(&parse(SUMMARY));
        assert_eq!(all.len(), 3);
        assert_eq!(all[0], Todo { text: "결제 모듈 배포".into(), owner: Some("이 개발".into()), due: Some("금요일".into()) });
        assert_eq!(all[2], Todo { text: "매장 피드백 정리".into(), owner: None, due: None }, "자리 표시는 빈 값");
        assert_eq!(todo_of("⭐ 공지 올리기"), Todo { text: "⭐ 공지 올리기".into(), owner: None, due: None });
        assert_eq!(todo_of("견적서 — 기한: 다음 주 화요일").due.as_deref(), Some("다음 주 화요일"));
    }

    #[test]
    fn messenger_text_is_short_and_plain() {
        let m = messenger(&doc(SUMMARY)).unwrap();
        assert_eq!(
            m,
            "[앱 출시 일정 회의] 11/19(목) 회의 정리\n다음 달 15일 앱 출시 확정, 목요일 최종 점검\n\n\
             ✅ 결정 사항\n• ⭐ 출시일 15일 확정\n\n\
             📌 할 일\n• 결제 모듈 배포 — 이 개발 · 금요일까지\n• 온보딩 시안 2종 — 박 디자인 · 11월 25일까지\n• 매장 피드백 정리"
        );
        assert!(!m.contains("[00:") && !m.contains("**") && !m.contains("###"));
        // 정한 것 · 할 일이 없는 틀(강의)은 본문 첫 섹션의 큰 줄기
        let lecture = "### 한 줄 요약\n인덱스 사례\n\n### 핵심 내용\n- **응답 속도**\n  - 2초 → 0.4초 [00:34]\n- **캐시**\n\n### 더 볼 것\n- 없음";
        let m = messenger(&doc(lecture)).unwrap();
        assert!(m.ends_with("📝 핵심 내용\n• 응답 속도\n• 캐시"), "{m}");
        assert!(messenger(&doc("")).is_none());
    }

    #[test]
    fn email_has_every_section_without_time_tags() {
        let (subject, body) = email(&doc(SUMMARY)).unwrap();
        assert_eq!(subject, "[회의록] 앱 출시 일정 회의 (11/19)");
        assert!(body.starts_with("안녕하세요.\n11월 19일 앱 출시 일정 회의 내용을 정리해 공유드립니다.\n\n다음 달 15일"), "{body}");
        assert_eq!(meeting_name("주간 점검"), "주간 점검 회의");
        assert!(body.contains("■ 주요 논의\n- 출시 일정\n   · ⭐ 15일 출시, 목요일 점검 (김 팀장)\n"), "{body}");
        assert!(body.contains("■ 할 일\n- 결제 모듈 배포 — 이 개발 · 금요일까지\n"), "{body}");
        assert!(!body.contains("■ 남은 질문"), "없음뿐인 섹션은 뺀다");
        assert!(!body.contains("[00:") && body.ends_with("감사합니다.\n"));
        let e = eml(&subject, &body);
        assert!(e.starts_with("X-Unsent: 1\r\nSubject: =?UTF-8?B?"));
        let b64 = e.split("\r\n\r\n").nth(1).unwrap().replace("\r\n", "");
        let back = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b64).unwrap()).unwrap();
        assert_eq!(back, body.replace('\n', "\r\n"));
        assert!(e.split("\r\n").all(|l| l.len() <= 998));
    }

    #[test]
    fn due_words_become_dates() {
        let base = d("2026-11-19"); // 목요일
        for (text, want) in [
            ("금요일", "2026-11-20"),
            ("금요일까지", "2026-11-20"),
            ("목요일", "2026-11-26"),
            ("다음 주 화요일", "2026-11-24"),
            ("차주 월요일", "2026-11-23"),
            ("이번 주 금요일", "2026-11-20"),
            ("다다음 주 수요일", "2026-12-02"),
            ("11월 25일", "2026-11-25"),
            ("11월 25일(화) 오전", "2026-11-25"),
            ("1월 5일", "2027-01-05"),
            ("2027년 11월 25일", "2027-11-25"),
            ("2026년 12월 1일까지", "2026-12-01"),
            ("27년 1월 5일", "2027-01-05"),
            ("내년 12월 1일", "2027-12-01"),
            ("올해 12월 24일", "2026-12-24"),
            ("내년 1월 말", "2027-01-31"),
            ("금요일(11/21)", "2026-11-21"),
            ("12/3", "2026-12-03"),
            ("2026-12-10", "2026-12-10"),
            ("2027.01.04", "2027-01-04"),
            ("25일", "2026-11-25"),
            ("10일", "2026-12-10"),
            ("내일", "2026-11-20"),
            ("모레", "2026-11-21"),
            ("오늘 중", "2026-11-19"),
            ("3일 뒤", "2026-11-22"),
            ("2주 후", "2026-12-03"),
            ("일주일 내", "2026-11-26"),
            ("월말", "2026-11-30"),
            ("다음 달 말", "2026-12-31"),
            ("12월 말", "2026-12-31"),
            ("1월 말일", "2027-01-31"),
            ("2027년 2월 말", "2027-02-28"),
            ("이번 달 말일", "2026-11-30"),
            ("다음 달 1일(화요일)", "2026-12-01"),
            ("이번 달 28일", "2026-11-28"),
            ("다다음 달 5일", "2027-01-05"),
            ("다다음 달 말", "2027-01-31"),
            ("다음 달 말일", "2026-12-31"),
            ("이번 주말", "2026-11-21"),
        ] {
            assert_eq!(due_date(text, base), Some(d(want)), "{text}");
        }
        for text in ["미정", "다음 주", "분기 내", "확인 후", "가능한 빨리", "11월 셋째 주 금요일", "다음 달 첫째 주 화요일", "다음 달", "이번 달 중", "12시까지", "내년 상반기"] {
            assert_eq!(due_date(text, base), None, "{text}");
        }
    }

    /// 끝낸 할 일(`- [x]`)은 메신저 · 메일에 "(완료)" 로 남기고, 일정 파일 · 할 일 수에서는 뺀다
    #[test]
    fn done_todos_are_marked_and_not_scheduled() {
        let s = "### 할 일\n- [x] 공지 올리기 — 담당: 박 디자인 / 기한: 내일 [00:10]\n- [X] 견적서 — 기한: 금요일\n- [ ] 결제 모듈 배포 — 담당: 이 개발 / 기한: 금요일";
        let m = messenger(&doc(s)).unwrap();
        assert!(m.contains("• 공지 올리기 — 박 디자인 · 내일까지 (완료)\n• 견적서 — 금요일까지 (완료)\n• 결제 모듈 배포"), "{m}");
        assert!(email(&doc(s)).unwrap().1.contains("- 공지 올리기 — 박 디자인 · 내일까지 (완료)\n"));
        assert_eq!(todo_counts(&doc(s)), (1, 1));
        let (ics, n) = ics(&doc(s), "20261120T000000Z");
        assert_eq!(n, 1);
        assert!(!ics.contains("공지 올리기") && ics.contains("결제 모듈 배포"));
    }

    #[test]
    fn ics_has_only_dated_todos_and_is_well_formed() {
        let (ics, n) = ics(&doc(SUMMARY), "20261120T010203Z");
        assert_eq!(n, 2, "기한을 읽을 수 없는 할 일은 뺀다");
        assert_eq!(todo_counts(&doc(SUMMARY)), (3, 2));
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n") && ics.ends_with("END:VCALENDAR\r\n"));
        assert!(ics.contains("DTSTART;VALUE=DATE:20261120\r\nDTEND;VALUE=DATE:20261121\r\n"), "금요일");
        assert!(ics.contains("DTSTART;VALUE=DATE:20261125\r\n"), "11월 25일");
        assert!(ics.contains("SUMMARY:[할 일] 결제 모듈 배포 (이 개발)\r\n"));
        assert!(ics.contains("UID:m1-todo-0@badasugi\r\n") && ics.contains("DTSTAMP:20261120T010203Z\r\n"));
        // 줄은 75바이트 안쪽(접힌 줄은 빈칸으로 시작), 쉼표 · 줄바꿈은 이스케이프
        assert!(ics.split("\r\n").all(|l| l.len() <= 75), "{ics}");
        let unfolded = ics.replace("\r\n ", "");
        assert!(unfolded.contains("DESCRIPTION:회의: 앱 출시 일정 회의 · 11/19(목)\\n담당: 이 개발\\n기한: 금요일\r\n"), "{unfolded}");
        assert_eq!(ics_text("a,b;c\\d\ne"), "a\\,b\\;c\\\\d\\ne");
    }
}
