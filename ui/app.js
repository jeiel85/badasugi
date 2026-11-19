'use strict';

/* ───────────── 기반 ───────────── */
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const T = window.__TAURI__;
// 기록을 읽고 쓰는 명령은 앱이 작업 스레드에서 처리한다 — 보낸 순서대로 끝나도록 하나씩 보낸다
// (편집을 저장하고 바로 다시 읽어도 저장된 내용을 받게)
const ORDERED_CMDS = new Set(['save_session', 'load_session', 'undo_refinement', 'delete_session', 'export_session', 'session_text', 'session_audio', 'delete_qa']);
// 앞서 보낸 기록 명령(편집 저장 등)이 끝난 뒤에 시작하지만, 뒤에 오는 명령은 붙잡지 않는다 (요약 · 묻기는 몇 분 걸릴 수 있다)
const AFTER_CMDS = new Set(['summarize', 'ask_meeting']);
let orderedChain = Promise.resolve();
function orderedInvoke(cmd, args) {
  if (AFTER_CMDS.has(cmd)) return orderedChain.then(() => T.core.invoke(cmd, args));
  if (!ORDERED_CMDS.has(cmd)) return T.core.invoke(cmd, args);
  const p = orderedChain.then(() => T.core.invoke(cmd, args));
  orderedChain = p.catch(() => {});
  return p;
}
const api = T ? {
  invoke: orderedInvoke,
  listen: T.event.listen,
  win: T.window.getCurrentWindow(),
  save: (o) => T.dialog.save(o),
  open: (o) => T.dialog.open(o),
  ask: (m, o) => T.dialog.ask(m, o),
  onDrop: (fn) => T.webview.getCurrentWebview().onDragDropEvent(fn),
} : createMock();

const COLORS = ['#7c6cff', '#0ea5e9', '#f59e0b', '#10b981', '#f43f5e', '#6366f1', '#14b8a6', '#f97316', '#a855f7', '#84cc16'];
const spkColor = (s) => (s == null ? 'var(--muted)' : COLORS[s % COLORS.length]);
const SOURCE_LABEL = { mic: '마이크', system: '시스템 소리', both: '마이크 + 시스템' };
const SOURCE_ICON = { mic: '#i-mic', system: '#i-speaker', both: '#i-both' };
// 설명은 앱이 정한 문구만 넣으므로 innerHTML 로 그린다 (링크 · 단계 안내)
const KEY_STEPS = {
  groq: '<ol class="howto"><li><button class="link" data-url="https://console.groq.com/keys">console.groq.com</button> 에 가입 · 로그인 (구글 계정 가능)</li><li><b>API Keys</b> → <b>Create API Key</b> → 이름 아무거나</li><li>나온 키(<code>gsk_…</code>)를 복사해 아래 칸에 붙여 넣기 — 다시 볼 수 없으니 바로 붙여 넣으세요</li></ol>',
  openai: '<ol class="howto"><li><button class="link" data-url="https://platform.openai.com/api-keys">platform.openai.com</button> 에 로그인</li><li><b>Billing</b> 에서 결제 수단 등록 (사용한 만큼 과금)</li><li><b>Create new secret key</b> → 키(<code>sk-…</code>)를 복사해 붙여 넣기</li></ol>',
  gemini: '<ol class="howto"><li><button class="link" data-url="https://aistudio.google.com/apikey">aistudio.google.com</button> 에 구글 계정으로 로그인</li><li><b>Create API key</b> (또는 Get API key)</li><li>나온 키(<code>AIza…</code>)를 복사해 아래 칸에 붙여 넣기</li></ol>',
  anthropic: '<ol class="howto"><li><button class="link" data-url="https://console.anthropic.com/settings/keys">console.anthropic.com</button> 에 가입 · 로그인</li><li><b>Billing</b> 에서 크레딧 충전 (사용한 만큼 차감)</li><li><b>Create Key</b> → 키(<code>sk-ant-…</code>)를 복사해 붙여 넣기</li></ol>',
};
const KEY_NOTE = '<span class="keynote">키는 이 PC의 Windows 자격 증명 관리자에만 보관돼요.</span>';
const ANT_LOGIN = '<span class="keynote"><b>키 대신 로그인</b>: <button class="link" data-url="https://github.com/anthropics/anthropic-cli/releases/latest">Anthropic CLI(ant) ↗</button>의 windows_amd64.zip 을 풀어 <code>ant.exe</code> 를 PATH 폴더(또는 <code>%LOCALAPPDATA%\\Programs\\ant</code>)에 두고, 명령 프롬프트에서 <code>ant auth login</code> → 저장된 키가 없으면 그 로그인으로 요약해요 (저장한 키는 <b>지우기</b> 로 지워요). 과금은 키와 같은 Console 크레딧이에요.</span>';
const LLM_DESC = {
  ollama: 'PC에 따로 설치한 <b>Ollama</b>로 요약해요 (앱에 들어 있지 않아요). 인터넷 없이 동작하지만 Ollama 설치와 모델(약 5GB)이 필요하고, PC 사양에 따라 몇 분 걸릴 수 있어요. <button class="link" data-url="https://ollama.com/download">Ollama 받기 ↗</button> → 설치 후 명령 프롬프트에서 <code>ollama pull exaone3.5:7.8b</code>',
  groq: '<b>무료 등급</b>으로 바로 쓸 수 있고 매우 빨라요. 음성 인식용 Groq 키를 같이 써요.' + KEY_STEPS.groq + KEY_NOTE,
  gemini: 'Google Gemini — <b>무료 등급</b>이 있고 한국어 요약 품질이 좋아요.' + KEY_STEPS.gemini + KEY_NOTE,
  openai: 'OpenAI — 음성 인식용 OpenAI 키를 같이 써요. 사용한 만큼 과금돼요.' + KEY_STEPS.openai + KEY_NOTE,
  anthropic: 'Anthropic Claude — 긴 회의도 꼼꼼하게 정리해요. 사용한 만큼 과금돼요.' + KEY_STEPS.anthropic + KEY_NOTE + ANT_LOGIN,
  custom: 'LM Studio · vLLM · OpenRouter 같은 OpenAI 호환 서버를 써요.',
};
const LLM_PRIVACY = '<br>API를 쓰면 녹취록이 해당 서비스로 전송돼요.';

const ENGINE_DESC = {
  local: '인터넷 없이 이 PC에서 인식합니다. 회의 내용이 밖으로 나가지 않아요. 첫 사용 때 모델을 한 번 내려받습니다.',
  openai: 'OpenAI gpt-4o-transcribe — 한국어 정확도가 가장 높습니다. 사용한 만큼 과금돼요. 회의 소리가 OpenAI 로 전송돼요.' + KEY_STEPS.openai + KEY_NOTE,
  groq: 'Groq Whisper — 매우 빨라 회의가 길어도 밀리지 않아요. <b>무료 등급</b>으로 시작할 수 있어요(사용량 한도 있음). 회의 소리가 Groq 로 전송돼요.' + KEY_STEPS.groq + KEY_NOTE,
  custom: 'OpenAI 호환 음성 인식 서버(faster-whisper-server, LocalAI, 사내 서버 등)를 사용합니다.',
};

const S = {
  app: null,
  settings: null,
  rec: 'idle',
  live: true,
  liveDoc: null,
  viewDoc: null,
  pending: new Map(),
  failed: new Map(),
  timerStart: 0,
  speaking: false,
  downloads: {},
  dlQueue: [],
  summarizing: false,
  paused: false,
  fileMode: false,
  progress: null,
  update: null,
  updateDismissed: false,
  findQ: '',
  findIdx: 0,
  mini: false,
  // 노트: 지금 탭 · 고치는 중인 요약 { doc } · 쓰는 중인 코멘트 { doc, id, ms, line, raw } · 녹음 위치(ms)
  ntab: 'memo',
  sumEdit: null,
  cmt: null,
  livePosMs: 0,
};
const doc = () => (S.live ? S.liveDoc : S.viewDoc);

/* ───────────── 유틸 ───────────── */
function fmt(ms) {
  const s = Math.floor((ms || 0) / 1000);
  const h = Math.floor(s / 3600), m = Math.floor((s / 60) % 60), x = s % 60;
  const p = (n) => String(n).padStart(2, '0');
  return h ? `${h}:${p(m)}:${p(x)}` : `${p(m)}:${p(x)}`;
}
// 'YYYY-MM-DD' → 오늘 · 어제 · 11월 12일 (목) · (다른 해) 2025년 11월 12일 (수)
const WEEKDAY = ['일', '월', '화', '수', '목', '금', '토'];
function dayLabel(ymd) {
  const [y, m, d] = String(ymd || '').split('-').map(Number);
  if (!y || !m || !d) return String(ymd || '');
  const day = new Date(y, m - 1, d);
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const ago = Math.round((today - day) / 86400000);
  if (ago === 0) return '오늘';
  if (ago === 1) return '어제';
  const s = `${m}월 ${d}일 (${WEEKDAY[day.getDay()]})`;
  return y === today.getFullYear() ? s : `${y}년 ${s}`;
}
// 기록의 시작 시각 'YYYY-MM-DD HH:MM' → '오늘 13:59'
function whenLabel(startedAt) {
  const s = String(startedAt || '');
  return `${dayLabel(s.slice(0, 10))} ${s.slice(11, 16)}`.trim();
}
// 회의 길이 — 시각(13:59)과 헷갈리지 않게 말로: 45초 · 16분 · 1시간 7분
function durLabel(ms) {
  const s = Math.round((ms || 0) / 1000);
  if (s < 60) return `${s}초`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}분`;
  return m % 60 ? `${Math.floor(m / 60)}시간 ${m % 60}분` : `${m / 60}시간`;
}
function mb(bytes) {
  return bytes >= 1e9 ? `${(bytes / 1e9).toFixed(1)}GB` : `${Math.round(bytes / 1e6)}MB`;
}
function esc(s) {
  return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
}
function speakerName(d, s) {
  if (s == null) return '발화';
  return (d && d.speakers && d.speakers[s]) || `화자 ${s >= 100 ? s - 99 + maxAutoSpeaker(d) : s + 1}`;
}
// 사용자가 만든 새 화자(100번부터)는 자동 화자 번호 뒤에 이어 붙인다
function maxAutoSpeaker(d) {
  let m = 0;
  for (const u of d?.utterances || []) if (u.speaker != null && u.speaker < 100) m = Math.max(m, u.speaker + 1);
  return m;
}
// 합친 화자를 따라간다
function resolveSpk(d, s) {
  if (s == null || !d || !d.speakerMap) return s;
  for (let i = 0; i < 16 && d.speakerMap[s] != null && d.speakerMap[s] !== s; i++) s = d.speakerMap[s];
  return s;
}
function hl(text) {
  const t = esc(text);
  if (!S.findQ) return t;
  const q = esc(S.findQ).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return t.replace(new RegExp(q, 'gi'), (m) => `<mark>${m}</mark>`);
}
let toastTimer;
function toast(msg, kind = '') {
  const t = $('#toast');
  t.textContent = msg;
  t.className = `toast ${kind}`;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), kind === 'error' || kind === 'warn' ? 6500 : 2600);
}
async function call(cmd, args) {
  try {
    return await api.invoke(cmd, args);
  } catch (e) {
    toast(String(e), 'error');
    throw e;
  }
}
// 마지막 호출만 ms 뒤에 실행. .flush() 는 기다리는 호출을 바로 실행한다
function debounce(fn, ms) {
  let t, pending = null;
  const run = () => {
    clearTimeout(t);
    if (!pending) return;
    const a = pending;
    pending = null;
    fn(...a);
  };
  const f = (...a) => { pending = a; clearTimeout(t); t = setTimeout(run, ms); };
  f.flush = run;
  f.cancel = () => { clearTimeout(t); pending = null; };
  return f;
}

function md(src) {
  const inline = (s) => esc(s).replace(/\*\*(.+?)\*\*/g, '<b>$1</b>').replace(/`(.+?)`/g, '<code>$1</code>');
  let html = '', list = false;
  for (const raw of String(src).split('\n')) {
    const line = raw.trimEnd();
    const li = line.match(/^(\s*)[-*]\s+(\[[ xX]\]\s+)?(.*)$/);
    if (li) {
      if (!list) { html += '<ul>'; list = true; }
      const sub = li[1].replace(/\t/g, '  ').length >= 2 ? ' sub' : '';
      html += li[2] ? `<li class="task${sub}"><span>${inline(li[3])}</span></li>` : `<li${sub ? ' class="sub"' : ''}>${inline(li[3])}</li>`;
      continue;
    }
    if (list) { html += '</ul>'; list = false; }
    const h = line.match(/^#{1,6}\s+(.*)$/);
    if (h) html += `<h4>${inline(h[1])}</h4>`;
    else if (line.trim()) html += `<p>${inline(line)}</p>`;
  }
  if (list) html += '</ul>';
  return html;
}

/* ───────────── 테마 ───────────── */
const darkMq = matchMedia('(prefers-color-scheme: dark)');
function applyTheme() {
  const t = S.settings?.theme || 'system';
  document.documentElement.dataset.theme = t === 'system' ? (darkMq.matches ? 'dark' : 'light') : t;
}
darkMq.addEventListener('change', applyTheme);

/* ───────────── 본문 렌더 ───────────── */
const body = $('#body');
let editing = false;
let renderQueued = false;

function nearBottom() {
  return body.scrollHeight - body.scrollTop - body.clientHeight < 80;
}

// 받아 적는 동안에는 늘 맨 아래(최신 문장)를 따라간다. 사용자가 위로 올려 읽는 동안만 멈추고 '새 발화 ↓' 를 띄운다.
// 따라갈지는 그릴 때의 스크롤 위치로 짐작하지 않고 사용자의 스크롤로만 정한다 —
// 부드러운 스크롤이 내려가는 도중에 새 문장이 오면 '맨 아래가 아님'으로 읽혀 따라가기를 놓쳤고,
// '듣는 중' 표시·요약 카드처럼 그리기 밖에서 높이가 바뀌면 맨 아래에서 밀려나 있었다
let follow = true;
let lastTop = 0;
const following = () => S.live && S.rec !== 'idle';
function pinBottom() {
  body.scrollTo({ top: body.scrollHeight, behavior: 'instant' });
  $('#jump').hidden = true;
}
function followLatest() { follow = true; pinBottom(); }

function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => { renderQueued = false; renderDoc(); });
}

// 긴 회의에서 새 문장이 올 때마다 녹취록 전체를 다시 그리지 않게, 앞에서부터 같은 문단은 그대로 두고
// 처음 달라진 문단부터만 다시 그린다 (새 문장은 대개 맨 끝 문단만 바꾼다)
let shownGroups = [];
function patchTranscript(tr, html) {
  const kids = tr.children;
  let i = 0;
  while (i < html.length && i < shownGroups.length && i < kids.length && html[i] === shownGroups[i]) i++;
  while (kids.length > i) tr.lastElementChild.remove();
  if (i < html.length) tr.insertAdjacentHTML('beforeend', html.slice(i).join(''));
  shownGroups = html;
}
// 화면에서 직접 고친 문단(편집 등)이 있으면 다음에는 처음부터 다시 그린다
function invalidateTranscript() { shownGroups = []; }

function renderDoc() {
  if (editing) return;
  const d = doc();
  const items = [];
  if (d) for (const u of d.utterances) items.push({ kind: 'u', ...u, speaker: resolveSpk(d, u.speaker) });
  if (S.live) {
    for (const [id, p] of S.pending) items.push({ kind: 'p', id, ...p, speaker: resolveSpk(d, p.speaker) });
    for (const [id, f] of S.failed) items.push({ kind: 'f', id, ...f, speaker: resolveSpk(d, f.speaker) });
  }
  items.sort((a, b) => a.startMs - b.startMs || a.id - b.id);

  const hasContent = items.length > 0;
  const showEmpty = !hasContent && S.rec === 'idle' && S.live;
  // 첫 화면이 다시 보일 때마다 최근 회의를 새로 읽는다 (방금 마친 회의 · 바꾼 제목이 들어가게)
  if (showEmpty && $('#empty').hidden) renderRecent();
  $('#empty').hidden = !showEmpty;
  $('#meta').hidden = !d || (!hasContent && S.rec === 'idle');
  renderSetup();

  const stick = following() ? follow : nearBottom();
  const tr = $('#transcript');
  const cms = memoComments(d);
  const groups = [];
  for (const it of items) {
    const g = groups[groups.length - 1];
    if (g && g.speaker === it.speaker && it.startMs - g.endMs < 15000) {
      g.items.push(it);
      g.endMs = Math.max(g.endMs, it.endMs);
    } else {
      groups.push({ speaker: it.speaker, startMs: it.startMs, endMs: it.endMs, items: [it] });
    }
  }
  patchTranscript(tr, groups.map((g) => `
    <div class="group" style="--c:${spkColor(g.speaker)}">
      <div class="g-head"><span class="g-name" data-spk="${g.speaker ?? ''}" data-ids="${g.items.filter((x) => x.kind === 'u').map((x) => x.id).join(',')}" title="이름 바꾸기 · 화자 바꾸기">${esc(speakerName(d, g.speaker))}</span><span class="g-time">${fmt(g.startMs)}</span></div>
      ${g.items.map((it) => it.kind === 'u'
        ? `<div class="u" data-id="${it.id}">${hl(it.text)}${it.latencyMs != null ? `<span class="lat">${(it.latencyMs / 1000).toFixed(1)}초</span>` : ''}</div>${commentHtml(cms.byU.get(it.id))}`
        : it.kind === 'p'
          ? `<div class="u pending" style="--w:${40 + ((it.id * 37) % 50)}%"></div>`
          : `<div class="u err">인식 실패: ${esc(it.message)}</div>`).join('')}
    </div>`));

  $('#listening').hidden = !(S.live && S.rec === 'recording' && S.speaking && !S.paused && !S.fileMode);
  renderMeta();
  renderNotes();
  renderSummary();
  updateActions();
  renderMiniCap();
  syncAudio();
  markPlaying(false);
  if (S.findQ) { updateFind(false); return; }
  if (stick) pinBottom();
  else if (S.live && S.rec === 'recording') $('#jump').hidden = false;
}

function renderMeta() {
  const d = doc();
  if (!d) return;
  const ti = $('#title-input');
  if (document.activeElement !== ti) ti.value = d.title || '';
  const spk = new Set();
  for (const u of d.utterances) if (u.speaker != null) spk.add(resolveSpk(d, u.speaker));
  if (S.live) for (const p of S.pending.values()) if (p.speaker != null) spk.add(resolveSpk(d, p.speaker));
  const box = $('#speakers');
  if (box.querySelector('input')) return;
  // 회의 후 화자 다시 정리가 화자를 바꿨으면, 녹음 중에 정해진 화자로 되돌릴 수 있다
  const undo = d.beforeRefine && !(S.live && S.rec !== 'idle')
    ? '<button class="link spk-undo" id="btn-undo-refine" title="회의가 끝난 뒤 다시 정리하기 전, 녹음 중에 정해진 화자 배정과 이름으로 돌아가요">화자 정리 되돌리기</button>'
    : '';
  box.innerHTML = [...spk].sort((a, b) => a - b)
    .map((s) => `<button class="spk-chip" data-spk="${s}" style="--c:${spkColor(s)}" title="이름 바꾸기 · 다른 화자와 합치기"><i></i>${esc(speakerName(d, s))}</button>`)
    .join('') + undo;
}

// 받침이 있으면 '을', 없으면 '를' (한글로 끝나지 않으면 '을')
function objJosa(w) {
  const c = String(w).charCodeAt(String(w).length - 1) - 0xac00;
  return c >= 0 && c < 11172 && c % 28 === 0 ? '를' : '을';
}
// 요약 진행 문구 — 모델이 쓰고 있는 섹션("주요 논의를 정리하는 중…"),
// 긴 회의는 구간으로 나눠 정리한다 → "긴 회의라 구간으로 나눠 정리하는 중… (2/5)"
function summaryStep(p) {
  const at = p && p.total > 1 ? ` (${Math.min(p.done + 1, p.total)}/${p.total})` : '';
  if (p && p.waitSecs > 0) return `요청 한도 때문에 ${p.waitSecs}초 기다렸다가 이어서 정리해요…${at}`;
  if (!p || p.total <= 1) {
    if (!p || !p.writing) return '회의 내용을 읽는 중…';
    if (p.section === '제목') return '회의 제목을 정하는 중…';
    return p.section ? `${p.section}${objJosa(p.section)} 정리하는 중…` : '회의록을 쓰는 중…';
  }
  if (p.done >= p.total - 1) return `구간 메모를 합쳐 회의록을 만드는 중…${at}`;
  return `긴 회의라 구간으로 나눠 정리하는 중…${at}`;
}
// 진행률 0~99 — 끝난 요청 + 지금 요청에서 쓴 만큼 (100은 요약이 끝나 카드가 바뀔 때)
function summaryPct(p) {
  if (!p) return 0;
  const pct = Math.floor((100 * (p.done + (p.frac || 0))) / Math.max(p.total, 1));
  return Math.max(0, Math.min(99, pct));
}
function renderSummary() {
  const d = doc();
  const card = $('#summary-card');
  const empty = $('#summary-empty');
  // 같은 기록을 앱이 다시 읽어 준 사본으로 바뀌었으면 고치던 요약을 새 사본에 이어 붙인다 (입력 칸을 덮지 않게)
  if (S.sumEdit && d && S.sumEdit.doc !== d && S.sumEdit.doc.id === d.id) S.sumEdit.doc = d;
  // 고치는 중이면 입력 칸을 그대로 둔다
  if (S.sumEdit && d && S.sumEdit.doc === d) { card.hidden = false; empty.hidden = true; return; }
  $('#btn-edit-summary').hidden = false;
  // 요약하는 동안 다른 기록으로 옮겨 가도, 그 기록에 '정리하는 중'을 띄우지 않는다
  if (S.summarizing && d && d.id === S.sumId) {
    card.hidden = false;
    empty.hidden = true;
    $('#btn-edit-summary').hidden = true;
    $('#sum-mark').hidden = true;
    const box = $('#summary-body');
    // 진행은 1초에 몇 번씩 바뀌므로 틀은 한 번만 그리고 글자 · 막대만 바꾼다 (멈추기 버튼을 누르는 중에 다시 그려지지 않게)
    if (!box.querySelector('.sum-progress')) {
      box.innerHTML = '<div class="sum-progress">'
        + '<div class="sum-row"><span class="sum-step"></span><button class="link" id="btn-cancel-summary" title="보내 둔 요청은 버리고 여기서 그만둬요">멈추기</button></div>'
        + '<div class="sum-track"><div class="sum-bar" role="progressbar" aria-label="요약 진행" aria-valuemin="0" aria-valuemax="100"><i></i></div><span class="sum-meta"></span></div>'
        + '</div>';
    }
    renderSumProgress();
    return;
  }
  card.hidden = !(d && d.summary);
  // 노트의 요약 탭: 요약이 없으면 AI 로 요약 · 직접 쓰기
  empty.hidden = !card.hidden;
  $('#btn-summary-ai').disabled = !(d && d.utterances.length);
  $('#sum-mark').hidden = !(d && d.summary && d.summaryEdited);
  if (d && d.summary) $('#summary-body').innerHTML = md(d.summary);
}
// 진행 막대 · 퍼센트 · 걸린 시간. 막대는 되돌아가지 않는다 (긴 회의 구간이 더 잘게 나뉘어 전체가 늘어도)
function renderSumProgress() {
  const box = $('#summary-body .sum-progress');
  if (!box || !S.summarizing) return;
  const p = S.sumProgress;
  S.sumPct = Math.max(S.sumPct || 0, summaryPct(p));
  $('.sum-step', box).textContent = summaryStep(p);
  const bar = $('.sum-bar', box);
  // 아직 읽는 중(첫 글자 전) · 한도 때문에 기다리는 중에는 얼마나 걸릴지 모르니 막대 위로 빛만 흐른다
  bar.classList.toggle('busy', !p || !p.writing);
  bar.firstElementChild.style.width = `${S.sumPct}%`;
  const known = S.sumPct > 0 || (p && p.total > 1);
  if (known) bar.setAttribute('aria-valuenow', S.sumPct); else bar.removeAttribute('aria-valuenow');
  $('.sum-meta', box).textContent = (known ? `${S.sumPct}% · ` : '') + fmt(Date.now() - S.sumStarted);
  const stop = $('#btn-cancel-summary', box);
  stop.disabled = !!S.sumCancelling;
  stop.textContent = S.sumCancelling ? '멈추는 중…' : '멈추기';
}

function updateActions() {
  const d = doc();
  const has = !!(d && d.utterances.length);
  $('#btn-copy').disabled = !has;
  $('#btn-save').disabled = !has;
  $('#btn-summary').disabled = !has || S.summarizing;
  $('#btn-ask').disabled = !canAsk(d);
  renderBanners(has);
}

// 위쪽 배너: 지난 회의를 보는 중 / 방금 회의를 마침 (저장됐다고 알리고 → 새 회의 · 회의 기록)
function renderBanners(has) {
  const v = !S.live ? S.viewDoc : null;
  $('#viewing').hidden = !v;
  if (v) {
    // 제목은 바로 아래에 있으니 배너에는 언제 · 얼마나 한 회의인지를 보여 준다 (다시 볼 때는 도크의 시계를 숨긴다)
    $('#viewing-label').textContent = `지난 회의 · ${whenLabel(v.startedAt)}${v.durationMs ? ` · ${durLabel(v.durationMs)}` : ''}`;
    const back = $('#btn-back-live');
    const live = S.rec !== 'idle';
    back.textContent = live ? '실시간으로 돌아가기' : '닫기';
    back.title = live ? '받아 적는 중인 회의로 돌아가요' : '이 기록을 닫아요';
  }
  // 받아 적힌 문장이 없어도 메모를 남긴 회의는 기록에 저장된다
  const ended = S.live && S.rec === 'idle' && (has || !!String(S.liveDoc?.notes || '').trim());
  $('#ended').hidden = !ended;
  if (ended) {
    const dur = S.liveDoc?.durationMs ? ` · ${durLabel(S.liveDoc.durationMs)}` : '';
    $('#ended-label').textContent = `${S.liveDoc?.sourceFile ? '파일 받아쓰기를' : '회의를'} 마쳤어요${dur} · 기록에 저장됨`;
  }
  // 회의를 다시 보는 중(지난 회의 · 마친 회의)에는 녹음 도크를 한 줄로 줄인다
  const review = S.rec === 'idle' && (!!v || ended);
  $('#app').classList.toggle('review', review);
  $('#btn-rec').title = review ? '새 회의 녹음 시작' : '녹음 시작/중지';
}

// 첫 화면의 최근 회의 (최신 5개)
let recentReq = 0;
async function renderRecent() {
  const my = ++recentReq;
  let list = [];
  try { list = (await api.invoke('list_sessions')) || []; } catch { /* 비움 */ }
  if (my !== recentReq) return;
  const items = list.slice(0, 5);
  $('#recent').hidden = !items.length;
  $('#btn-recent-all').textContent = list.length > items.length ? `전체 보기 (${list.length})` : '전체 보기';
  $('#recent-list').innerHTML = items.map((m) => `<button class="recent-item" data-id="${esc(m.id)}" title="${esc(m.title)}">
      <div><b>${esc(m.title)}</b><small>${esc(whenLabel(m.startedAt))} · ${durLabel(m.durationMs)}${m.speakers > 1 ? ` · 화자 ${m.speakers}명` : ''}</small></div>
      <svg><use href="#i-chevron"/></svg>
    </button>`).join('');
}

// 회의를 마친 화면을 비우고 다음 회의를 준비한다 — 방금 회의는 이미 기록에 저장돼 있다
function newMeeting() {
  if (S.rec !== 'idle') return;
  commitEdits(); // 방금 고친 내용(문장 · 요약 · 메모 · 코멘트)이 있으면 먼저 저장
  S.live = true;
  S.viewDoc = null;
  S.liveDoc = null;
  S.pending.clear();
  S.failed.clear();
  follow = true;
  if (!$('#findbar').hidden) { $('#findbar').hidden = true; S.findQ = ''; $('#find-input').value = ''; }
  $('#speakers').innerHTML = '';
  $('#summary-card').classList.remove('folded');
  hidePopovers();
  renderDock();
  renderDoc();
  body.scrollTop = 0;
  toast('화면을 비웠어요.\n방금 회의는 회의 기록에 남아 있어요');
}

function renderSetup() {
  const box = $('#setup');
  if (!S.app || !S.settings) return;
  const cards = [];
  const s = S.settings;
  if (s.engine === 'local') {
    const m = S.app.models.find((x) => x.id === s.localModel);
    const custom = !m && S.app.customModels.includes(s.localModel);
    const needSpk = s.diarization.enabled && !S.app.speakerModelReady;
    if ((m && !m.ready) || needSpk) {
      const ids = [];
      if (needSpk) ids.push('campplus-zh-en');
      if (m && !m.ready) ids.push(m.id);
      const total = (needSpk ? 28281164 : 0) + (m && !m.ready ? m.size : 0);
      const active = ids.find((id) => S.downloads[id]);
      const dl = active ? S.downloads[active] : null;
      cards.push(`<div class="setup-card">
        <b>처음 한 번, 모델을 내려받아 주세요</b>
        <p>${m && !m.ready ? `음성 인식 ${esc(m.label)}` : ''}${m && !m.ready && needSpk ? ' + ' : ''}${needSpk ? '화자 구분' : ''} 모델 · 약 ${mb(total)}. 받고 나면 인터넷 없이 동작해요.</p>
        ${dl ? `<div class="progress"><i style="width:${(dl.done / Math.max(dl.total, 1)) * 100}%"></i></div><p style="margin:6px 0 0">${mb(dl.done)} / ${mb(dl.total)}</p>`
          : `<div class="actions"><button class="btn sm" data-dl="${ids.join(',')}">내려받기</button><button class="link" data-open-settings>API로 쓰기</button></div>`}
      </div>`);
    } else if (!m && !custom) {
      cards.push('<div class="setup-card"><b>모델을 선택해 주세요</b><p>설정에서 음성 인식 모델을 고를 수 있어요.</p><div class="actions"><button class="btn sm" data-open-settings>설정 열기</button></div></div>');
    }
  } else {
    const keyName = { openai: 'openai', groq: 'groq' }[s.engine];
    if (keyName && !S.app.keys[keyName]) {
      const keyUrl = s.engine === 'openai' ? 'https://platform.openai.com/api-keys' : 'https://console.groq.com/keys';
      cards.push(`<div class="setup-card"><b>${s.engine === 'openai' ? 'OpenAI' : 'Groq'} API 키가 필요해요</b><p>${s.engine === 'openai' ? 'OpenAI 사이트에서 키를 만들어' : 'Groq 사이트에서 무료로 키를 만들어'} 설정에 붙여 넣으면 바로 쓸 수 있어요. 키는 Windows 자격 증명 관리자에 안전하게 보관됩니다.</p><div class="actions"><button class="btn sm" data-url="${keyUrl}">키 받기 ↗</button><button class="link" data-open-settings>설정 열기</button></div></div>`);
    }
  }
  box.innerHTML = cards.join('');
}

/* ───────────── 도크 ───────────── */
function renderDock() {
  const app = $('#app');
  const rec = S.rec === 'recording';
  app.classList.toggle('recording', rec);
  app.classList.toggle('busy', S.rec === 'starting' || S.rec === 'stopping');
  app.classList.toggle('paused', rec && S.paused);
  app.classList.toggle('filemode', S.rec !== 'idle' && S.fileMode);
  const src = S.settings?.source || 'mic';
  const fileName = S.fileMode && S.rec !== 'idle' ? S.liveDoc?.sourceFile : null;
  $('#source-label').textContent = fileName || SOURCE_LABEL[src];
  $('#btn-source use').setAttribute('href', fileName ? '#i-file' : SOURCE_ICON[src]);
  $('#btn-source').disabled = !!fileName;
  const pb = $('#btn-pause');
  pb.hidden = !rec;
  pb.classList.toggle('on', S.paused);
  pb.title = S.paused ? '이어서 받아쓰기' : '일시정지';
  $('#btn-pause use').setAttribute('href', S.paused ? '#i-play' : '#i-pause');
  $('#paused-tag').hidden = !(rec && S.paused);
  $('#paused-tag').textContent = S.sleepPaused ? '절전으로 멈춤' : '일시정지';
  $('#fprog').hidden = !(S.fileMode && S.rec !== 'idle');
  renderProgress();
  renderMiniCap();
  $('#btn-pin').classList.toggle('on', !!S.settings?.alwaysOnTop);
  $('#hotkey-hint').textContent = S.settings?.hotkey || '단축키 없음';
  const lbl = S.rec === 'stopping' ? '남은 발화를 정리하는 중…' : S.rec === 'starting' ? '준비하는 중…'
    : S.fileMode && S.rec === 'recording' ? `파일 받아쓰기 · ${engineLabel()}` : engineLabel();
  $('#engine-label').textContent = lbl;
  tickTimer();
}

function engineLabel() {
  const s = S.settings;
  if (!s) return '';
  const spk = s.diarization.enabled ? ' · 화자 구분' : '';
  if (s.engine === 'local') {
    const m = S.app?.models.find((x) => x.id === s.localModel);
    return `로컬 · ${m ? m.label : s.localModel}${spk}`;
  }
  const model = { openai: s.openaiModel, groq: s.groqModel, custom: s.customModel }[s.engine];
  const name = { openai: 'OpenAI', groq: 'Groq', custom: '사용자 지정' }[s.engine];
  return `${name} · ${model}${spk}`;
}

function renderProgress() {
  const p = S.progress;
  if (!S.fileMode || !p) { $('#fprog-bar').style.width = '0%'; return; }
  const pct = p.totalMs ? Math.min(100, (p.doneMs / p.totalMs) * 100) : 0;
  $('#fprog-bar').style.width = `${pct}%`;
  $('#fprog').title = p.totalMs ? `${fmt(p.doneMs)} / ${fmt(p.totalMs)} (${Math.round(pct)}%)` : fmt(p.doneMs);
}

function tickTimer() {
  if (S.fileMode && S.rec !== 'idle') {
    $('#timer').textContent = S.progress ? fmt(S.progress.doneMs) : '00:00';
    return;
  }
  const d = doc();
  const ms = S.rec === 'recording' || S.rec === 'stopping' ? Date.now() - S.timerStart : d ? d.durationMs : 0;
  $('#timer').textContent = fmt(S.live || S.rec !== 'idle' ? ms : d?.durationMs);
}
setInterval(() => { if (S.rec === 'recording' && !document.hidden) tickTimer(); }, 250);

const meterBars = $$('#meter i');
const ring = $('#rec-ring');
function setLevel(rms) {
  const db = 20 * Math.log10(rms + 1e-6);
  const v = Math.min(1, Math.max(0, (db + 58) / 46));
  const shape = [0.45, 0.7, 0.9, 1, 0.9, 0.7, 0.45];
  meterBars.forEach((b, i) => {
    const jitter = 0.75 + Math.random() * 0.5;
    b.style.height = `${4 + v * 16 * shape[i] * jitter}px`;
  });
  ring.style.setProperty('--lv', (1 + v * 0.14).toFixed(3));
}
function resetLevel() {
  meterBars.forEach((b) => (b.style.height = '4px'));
  ring.style.setProperty('--lv', 1);
}

/* ───────────── 이벤트 (백엔드) ───────────── */
// 모델·키 상태만 새로 받는다 (화면에서 고치는 중인 설정은 그대로)
async function refreshApp() {
  const st = await api.invoke('get_state');
  if (S.settings) st.settings = S.settings;
  S.app = st;
}

async function refreshState() {
  S.app = await api.invoke('get_state');
  S.settings = S.app.settings;
  S.rec = S.app.rec;
  S.paused = !!S.app.paused;
  setMini(!!S.settings.miniMode);
  S.fileMode = !!S.app.fileMode;
  if (S.app.update) S.update = S.app.update;
  applyTheme();
  if (S.app.current) S.liveDoc = S.app.current;
  if (S.rec === 'recording') S.timerStart = Date.now() - S.app.elapsedMs;
  renderDock();
  scheduleRender();
}

function onRec(state) {
  const prev = S.rec;
  S.rec = state;
  if (state === 'starting' && prev === 'idle') {
    commitEdits();
    S.live = true;
    S.pending.clear();
    S.failed.clear();
    S.liveDoc = null;
    S.slowHinted = false;
    S.paused = false;
    S.sleepPaused = false;
    S.progress = null;
    S.livePosMs = 0;
    follow = true;
  }
  if (state === 'recording' && prev !== 'recording') {
    S.timerStart = Date.now();
    api.invoke('get_state').then((st) => {
      S.app = st;
      S.liveDoc = st.current;
      S.fileMode = !!st.fileMode;
      renderDock();
      scheduleRender();
    });
  }
  if (state === 'idle') {
    S.speaking = false;
    S.paused = false;
    const note = S.refineMsg ? `\n${S.refineMsg}` : '';
    if (S.fileMode && prev !== 'idle' && S.liveDoc?.utterances.length) toast(`파일 받아쓰기를 마쳤어요${note}`);
    else if (note && prev !== 'idle') toast(S.refineMsg);
    S.refineMsg = '';
    S.fileMode = false;
    S.progress = null;
    resetLevel();
  }
  renderDock();
  scheduleRender();
}

const renderLive = () => { if (S.live) scheduleRender(); };

function onEvent(e) {
  const d = S.liveDoc;
  switch (e.type) {
    case 'level':
      S.livePosMs = e.positionMs || 0;
      // 창이 트레이에 숨어 있으면 소리 막대는 그리지 않는다 (초당 14번)
      if (!document.hidden) setLevel(e.rms);
      if (S.speaking !== e.speaking) {
        S.speaking = e.speaking;
        $('#listening').hidden = !(S.live && S.speaking);
        renderMiniCap();
      }
      break;
    case 'pending':
      S.pending.set(e.id, { startMs: e.startMs, endMs: e.endMs, speaker: e.speaker });
      // PC 가 받아 적는 속도보다 말이 빨라 대기열이 쌓이면 한 번만 안내
      if (S.pending.size >= 4 && !S.slowHinted && !S.fileMode && S.settings?.engine === 'local') {
        S.slowHinted = true;
        toast('이 PC에선 인식이 말보다 느려 조금씩 밀리고 있어요.\n설정에서 가벼운 모델이나 Groq API를 쓰면 더 빨라요.');
      }
      renderLive();
      break;
    case 'transcript':
      S.pending.delete(e.id);
      // 연결 문제로 미뤄 뒀다가 녹음이 끝난 뒤 다시 받아 적은 문장
      S.failed.delete(e.id);
      if (d) {
        const u = { id: e.id, startMs: e.startMs, endMs: e.endMs, speaker: resolveSpk(d, e.speaker), text: e.text, latencyMs: e.latencyMs };
        const i = d.utterances.findIndex((x) => x.id === e.id);
        if (i >= 0) d.utterances[i] = u;
        else {
          // API 엔진은 여러 발화를 동시에 처리해 결과가 뒤섞여 온다 → 시간 순서 자리에 끼워 넣는다
          const at = d.utterances.findIndex((x) => x.startMs > u.startMs || (x.startMs === u.startMs && x.id > u.id));
          if (at < 0) d.utterances.push(u); else d.utterances.splice(at, 0, u);
        }
        d.durationMs = Math.max(d.durationMs || 0, e.endMs);
      }
      renderLive();
      break;
    case 'discarded':
      S.pending.delete(e.id);
      S.failed.delete(e.id);
      renderLive();
      break;
    case 'failed': {
      // 미뤄 둔 발화를 녹음 뒤 다시 받아 적다 또 실패하면 같은 id 로 한 번 더 온다
      const p = S.pending.get(e.id) || S.failed.get(e.id);
      S.pending.delete(e.id);
      if (p) S.failed.set(e.id, { startMs: p.startMs, endMs: p.endMs, speaker: p.speaker, message: e.message });
      if (S.failed.size === 1) toast(`인식 실패: ${e.message}`, e.retry ? 'warn' : 'error');
      renderLive();
      break;
    }
    case 'speakerNamed':
      // 기억해 둔 목소리로 알아본 화자
      if (d) {
        d.speakers = d.speakers || {};
        if (d.speakers[e.speaker] == null) {
          d.speakers[e.speaker] = e.name;
          $('#speakers').innerHTML = '';
          toast(`목소리로 '${e.name}'님을 알아봤어요`);
          renderLive();
        }
      }
      break;
    case 'speakersRefined': {
      // 녹음이 끝난 뒤 회의 전체를 보고 화자를 다시 정리했다 → 정리된 기록으로 바꿔 둔다
      const parts = [];
      if (e.moved) parts.push(`문장 ${e.moved}개의 화자를 바로잡았어요`);
      if (e.named) parts.push(`목소리로 ${e.named}명의 이름을 붙였어요`);
      S.refineMsg = parts.length ? `회의 전체를 다시 듣고 ${parts.join(' · ')}\n마음에 들지 않으면 화자 옆 '화자 정리 되돌리기'를 눌러 주세요` : '';
      const id = S.liveDoc?.id;
      if (id) {
        flushSaves(); // 기다리던 저장(메모 등)을 먼저 보내 정리된 기록에 들어가게
        api.invoke('load_session', { id }).then((fresh) => {
          if (S.liveDoc?.id !== id) return;
          S.liveDoc = adoptFresh(S.liveDoc, fresh);
          $('#speakers').innerHTML = '';
          renderLive();
        }).catch(() => {});
      }
      break;
    }
    case 'status':
      toast(e.message);
      break;
    case 'warning':
      toast(e.message, 'warn');
      break;
    case 'progress':
      S.progress = { doneMs: e.doneMs, totalMs: e.totalMs };
      renderProgress();
      tickTimer();
      break;
    case 'error':
      toast(e.message, 'error');
      break;
    case 'stopped': {
      S.pending.clear();
      // 화면이 들고 있던 기록(도착 순서대로 쌓임)을 저장된 기록으로 바꾼다 — 이후 편집이 순서가 뒤섞인 사본을 저장하지 않게
      const id = S.liveDoc?.id;
      flushSaves();
      api.invoke('get_state').then(async (st) => {
        S.app = st;
        if (id) {
          try {
            const fresh = await api.invoke('load_session', { id });
            if (S.liveDoc?.id === id) S.liveDoc = adoptFresh(S.liveDoc, fresh);
          } catch { /* 내용이 없어 지운 기록 */ }
        }
        renderDock();
        scheduleRender();
      });
      break;
    }
  }
}

function onDownload(e) {
  if (e.state === 'progress') {
    S.downloads[e.id] = { done: e.done, total: e.total };
  } else {
    delete S.downloads[e.id];
    if (e.state === 'error') { toast(e.message, 'error'); S.dlQueue = []; }
    if (e.state === 'done') {
      refreshApp().then(() => { renderModels(); renderSetup(); nextDownload(); });
      return;
    }
  }
  renderSetup();
  renderModels();
}

function queueDownloads(ids) {
  for (const id of ids) if (!S.dlQueue.includes(id)) S.dlQueue.push(id);
  if (!Object.keys(S.downloads).length) nextDownload();
}
function nextDownload() {
  const id = S.dlQueue.shift();
  if (!id) return;
  S.downloads[id] = { done: 0, total: 1 };
  renderSetup();
  renderModels();
  call('download_model', { id }).catch(() => { delete S.downloads[id]; renderSetup(); renderModels(); });
}

/* ───────────── 동작 ───────────── */
$('#btn-rec').addEventListener('click', async () => {
  try {
    await api.invoke('toggle_recording');
  } catch (e) {
    toast(String(e), 'error');
    if (/모델|API 키/.test(String(e))) openPanel('settings', 'record');
  }
});

$('#btn-min').addEventListener('click', () => api.win.minimize());
$('#btn-close').addEventListener('click', () => api.invoke('hide_window'));
$('#btn-pin').addEventListener('click', () => {
  S.settings.alwaysOnTop = !S.settings.alwaysOnTop;
  saveSettings();
  renderDock();
  toast(S.settings.alwaysOnTop ? '항상 위에 표시합니다' : '항상 위 표시를 껐습니다');
});
$('#btn-back-live').addEventListener('click', () => { commitEdits(); S.live = true; S.viewDoc = null; follow = true; renderDock(); scheduleRender(); });
$('#btn-view-list').addEventListener('click', () => openPanel('history'));
$('#btn-ended-list').addEventListener('click', () => openPanel('history'));
$('#btn-new').addEventListener('click', newMeeting);
$('#btn-recent-all').addEventListener('click', () => openPanel('history'));
$('#recent-list').addEventListener('click', (e) => {
  const b = e.target.closest('.recent-item');
  if (b) openSession(b.dataset.id);
});
$('#jump').addEventListener('click', followLatest);
body.addEventListener('scroll', () => {
  const top = body.scrollTop;
  // 끝까지 내려오면 다시 따라가고, 위로 올렸을 때만 멈춘다.
  // (내용이 늘어 맨 아래에서 벗어난 것뿐이면 그대로 둔다 — 곧 다시 맨 아래로 붙인다)
  if (body.scrollHeight - top - body.clientHeight < 8) follow = true;
  else if (top < lastTop - 1) follow = false;
  lastTop = top;
  if (follow) $('#jump').hidden = true;
});
// 휠을 위로 굴리면 바로 멈춘다 — 부드러운 휠 스크롤이 몇 픽셀 움직이는 사이에 새 문장이 와서 도로 끌려 내려가지 않게
body.addEventListener('wheel', (e) => {
  if (e.deltaY < 0 && !e.ctrlKey && body.scrollHeight > body.clientHeight) follow = false;
}, { passive: true });
// '듣는 중' 표시·요약 카드·글자 크기·창 크기처럼 그리기 밖에서 높이가 바뀌어도 맨 아래에 붙어 있게
const keepPinned = new ResizeObserver(() => { if (follow && following() && !S.findQ && !editing) pinBottom(); });
for (const el of [body, $('#summary-card'), $('#transcript'), $('#listening')]) keepPinned.observe(el);

// 제목
$('#title-input').addEventListener('change', (e) => {
  const d = doc();
  if (!d) return;
  d.title = e.target.value.trim() || d.title;
  d.autoTitle = false;
  persistDoc(d);
});
$('#title-input').addEventListener('keydown', (e) => { if (e.key === 'Enter') e.target.blur(); });

// 화자 이름 바꾸기
function startRename(spk) {
  const d = doc();
  if (!d || spk === '' || spk == null) return;
  const chip = $(`.spk-chip[data-spk="${spk}"]`);
  if (!chip) return;
  chip.innerHTML = `<i></i><input value="${esc(speakerName(d, +spk))}" />`;
  const inp = chip.querySelector('input');
  inp.focus();
  inp.select();
  const done = (ok) => {
    if (ok) {
      const v = inp.value.trim();
      d.speakers = d.speakers || {};
      const def = speakerName({ utterances: d.utterances, speakers: {} }, +spk);
      if (v && v !== def) d.speakers[spk] = v; else delete d.speakers[spk];
      persistDoc(d);
    }
    chip.innerHTML = '';
    $('#speakers').innerHTML = '';
    scheduleRender();
  };
  inp.addEventListener('keydown', (e) => { if (e.key === 'Enter') done(true); if (e.key === 'Escape') done(false); });
  inp.addEventListener('blur', () => done(true), { once: true });
}
// 화자 메뉴: 칩 → 이름 바꾸기 · 합치기 / 문단 이름 → 이름 바꾸기 · 이 문단 화자 바꾸기
function knownSpeakers(d) {
  const set = new Set();
  for (const u of d.utterances) if (u.speaker != null) set.add(resolveSpk(d, u.speaker));
  if (S.live) for (const p of S.pending.values()) if (p.speaker != null) set.add(resolveSpk(d, p.speaker));
  return [...set].sort((a, b) => a - b);
}
function speakerMenu(anchor, spk, ids) {
  const d = doc();
  if (!d || spk === '' || spk == null) return;
  spk = +spk;
  const others = knownSpeakers(d).filter((s) => s !== spk);
  const pop = $('#pop-spk');
  const item = (s, action) => `<button data-act="${action}" data-to="${s}"><i style="--c:${spkColor(s)}"></i>${esc(speakerName(d, s))}</button>`;
  let html = `<button data-act="rename"><svg><use href="#i-pencil"/></svg>이름 바꾸기</button>`;
  if (ids) {
    html += '<hr><div class="pop-title">이 문단의 화자를</div>' + others.map((s) => item(s, 'move')).join('')
      + `<button data-act="move" data-to="new"><svg><use href="#i-mic"/></svg>새 화자로 나누기</button>`;
  } else if (others.length) {
    html += '<hr><div class="pop-title">다른 화자와 합치기 (같은 사람일 때)</div>' + others.map((s) => item(s, 'merge')).join('');
  }
  pop.innerHTML = html;
  pop.dataset.spk = spk;
  pop.dataset.ids = ids || '';
  showPopover(pop, anchor, 'left', true);
}
$('#pop-spk').addEventListener('click', (e) => {
  const b = e.target.closest('button[data-act]');
  const d = doc();
  if (!b || !d) return;
  const pop = $('#pop-spk');
  const spk = +pop.dataset.spk;
  hidePopovers();
  if (b.dataset.act === 'rename') return startRename(spk);
  if (b.dataset.act === 'merge') {
    const to = +b.dataset.to;
    d.speakerMap = d.speakerMap || {};
    d.speakerMap[spk] = to;
    for (const u of d.utterances) if (resolveSpk(d, u.speaker) === to) u.speaker = to;
    if (d.speakers) delete d.speakers[spk];
    toast(`'${speakerName(d, to)}'에 합쳤어요${S.rec !== 'idle' && S.live ? ' · 앞으로 들어오는 발화도 함께 적용돼요' : ''}`);
  } else if (b.dataset.act === 'move') {
    let to = b.dataset.to;
    if (to === 'new') {
      let n = 100;
      const used = new Set(d.utterances.map((u) => u.speaker));
      while (used.has(n) || (d.speakerMap && d.speakerMap[n] != null)) n++;
      to = n;
    }
    const ids = new Set(pop.dataset.ids.split(',').filter(Boolean).map(Number));
    for (const u of d.utterances) if (ids.has(u.id)) u.speaker = +to;
  }
  persistDoc(d);
  $('#speakers').innerHTML = '';
  scheduleRender();
});
$('#speakers').addEventListener('click', (e) => {
  if (e.target.closest('#btn-undo-refine')) return undoRefine();
  const c = e.target.closest('.spk-chip');
  if (c && !c.querySelector('input')) speakerMenu(c, c.dataset.spk, null);
});
async function undoRefine() {
  const d = doc();
  if (!d?.beforeRefine) return;
  const ok = await api.ask('회의가 끝난 뒤 다시 정리한 화자를 되돌릴까요?\n녹음 중에 정해진 화자 배정과 이름으로 돌아가요. 고친 글자는 그대로예요.', { title: '받아쓰기', kind: 'info' });
  if (!ok) return;
  // 쓰던 요약 · 코멘트 · 메모를 먼저 저장한다 — 되돌린 기록으로 화면을 바꿔도 사라지지 않게
  commitEdits();
  try {
    const fresh = await api.invoke('undo_refinement', { id: d.id });
    if (S.live && S.liveDoc?.id === d.id) S.liveDoc = adoptFresh(S.liveDoc, fresh);
    else if (S.viewDoc?.id === d.id) S.viewDoc = adoptFresh(S.viewDoc, fresh);
    invalidateTranscript();
    $('#speakers').innerHTML = '';
    scheduleRender();
    toast('녹음 중에 정해진 화자로 되돌렸어요');
  } catch (err) {
    toast(String(err), 'error');
  }
}
$('#transcript').addEventListener('click', (e) => {
  const n = e.target.closest('.g-name');
  if (n) speakerMenu(n, n.dataset.spk, n.dataset.ids || '');
});

// 발화 고치기 (더블클릭 · 문장 위 '고치기')
function startEdit(el) {
  clearTimeout(playClick);
  if (!audio.paused && P.until != null) audio.pause();
  const d = doc();
  if (!el || !d || el.isContentEditable) return;
  const u = d.utterances.find((x) => x.id === +el.dataset.id);
  if (!u) return;
  editing = true;
  el.textContent = u.text;
  el.contentEditable = 'true';
  el.focus();
  // 커서를 문장 끝으로 (도구 버튼으로 시작하면 커서가 맨 앞에 놓인다)
  const sel = window.getSelection();
  sel.selectAllChildren(el);
  sel.collapseToEnd();
  showTools(el, true);
  let done = false;
  const finish = (save) => {
    if (done) return;
    done = true;
    el.contentEditable = 'false';
    editing = false;
    hideTools();
    invalidateTranscript();
    // innerText 는 화면 배치에 기대므로(가려진 문단은 빈 값) 글자 그대로인 textContent 로 읽는다 — 빈 값이면 발화가 지워지므로 중요
    const v = (el.textContent || '').replace(/\s+/g, ' ').trim();
    if (save && !v) {
      d.utterances = d.utterances.filter((x) => x.id !== u.id);
      d.deleted = [...(d.deleted || []), u.id];
      persistDoc(d); // 이 문장에 붙어 있던 코멘트(메모의 시각 줄)는 가장 가까운 문장으로 옮겨 붙는다
      toast('발화를 지웠어요');
    } else if (save && v !== u.text) { u.text = v; persistDoc(d); }
    scheduleRender();
  };
  el.addEventListener('keydown', (k) => {
    if (k.key === 'Enter' && !k.shiftKey) { k.preventDefault(); el.blur(); }
    if (k.key === 'Escape') { k.stopPropagation(); el.textContent = u.text; finish(false); }
  });
  el.addEventListener('blur', () => finish(true), { once: true });
}
$('#transcript').addEventListener('dblclick', (e) => {
  if (e.target.closest('.cmt')) return;
  const el = e.target.closest('.u[data-id]');
  if (ghostHit(el, e)) return;
  startEdit(el);
});

const persistDoc = debounce((d) => {
  api.invoke('save_session', { doc: d }).catch((e) => toast(String(e), 'error'));
}, 300);

/* ───────────── 노트: 메모 · 요약 고치기 · 코멘트 ───────────── */
// 기다리던 저장을 바로 보낸다 (다른 기록으로 옮겨 가거나 기록을 다시 읽기 전에)
function flushSaves() {
  memoSave.flush();
  persistDoc.flush();
}
// 쓰는 중인 코멘트 · 요약을 저장해 닫고, 기다리던 저장도 보낸다
function commitEdits() {
  if (S.cmt) closeComment(true);
  if (S.sumEdit) endSummaryEdit(true);
  flushSaves();
}
// 지운 기록: 저장하지 않고 닫는다
function dropEdits() {
  if (S.cmt) closeComment(false);
  if (S.sumEdit) endSummaryEdit(false);
  memoSave.cancel();
}
// 앱이 다시 읽어 준 기록으로 바꿀 때 메모(코멘트 포함)는 화면 것을 지킨다
// (앱은 메모를 고치지 않으므로 화면 것이 가장 새것 — 다시 읽는 사이에 친 글자가 사라지지 않게)
function adoptFresh(old, fresh) {
  if (!old || !fresh || old.id !== fresh.id) return fresh;
  fresh.notes = old.notes;
  if (S.sumEdit?.doc === old) S.sumEdit.doc = fresh;
  if (S.cmt?.doc === old) S.cmt.doc = fresh;
  return fresh;
}

/* 메모와 코멘트: 원본은 메모 하나. `[12:34]` 로 시작하는 메모 줄이 그 시각 문장의 코멘트다
   (시각 줄부터 다음 시각 줄 · 빈 줄 바로 앞까지가 한 코멘트, `- [12:34]` 처럼 목록 표시가 앞에 있어도 된다).
   녹취록 문장에서 단 코멘트는 메모에 `[문장 시각] 글` 줄로 들어간다. (앱 쪽 session.rs 의 note_comments 와 같은 규칙) */
const STAMP_RE = /^(\s*(?:[-*]\s+)?\[(?:(\d{1,3}):)?(\d{1,3}):(\d{2})\] ?)(.*)$/;
function parseStamp(line) {
  const m = STAMP_RE.exec(line.replace(/\r$/, ''));
  if (!m) return null;
  const h = m[2] != null ? +m[2] : 0, mi = +m[3], s = +m[4];
  if (s >= 60 || (m[2] != null && mi >= 60)) return null;
  return { ms: (h * 3600 + mi * 60 + s) * 1000, prefix: m[1], rest: m[5] };
}
// 메모 속 코멘트들: { ms, from, to (줄 번호, to 는 그 줄 앞까지), text, prefix } — 글이 없는 시각 줄은 빼고
function parseMemo(notes) {
  const lines = (notes || '').split('\n');
  const out = [];
  for (let i = 0; i < lines.length;) {
    const st = parseStamp(lines[i]);
    if (!st) { i++; continue; }
    const from = i;
    const text = [st.rest.trimEnd()];
    for (i++; i < lines.length && lines[i].trim() && !parseStamp(lines[i]); i++) text.push(lines[i].trimEnd());
    const t = text.join('\n').trim();
    if (t) out.push({ ms: st.ms, from, to: i, text: t, prefix: st.prefix });
  }
  return out;
}
// 코멘트 칸에 쓴 글 → 메모 줄들 (빈 줄은 코멘트를 끝내므로 뺀다)
function commentLines(v) {
  const ls = String(v).split('\n').map((l) => l.trimEnd()).filter((l) => l.trim());
  if (ls.length) ls[0] = ls[0].trimStart();
  return ls;
}
// 새 코멘트를 시각 순서에 맞는 자리에: 같거나 이른 코멘트 중 메모에서 마지막 것 뒤 → 없으면 더 늦은 첫 코멘트 앞 → 없으면 맨 끝
function insertMemoComment(notes, ms, v) {
  const body = commentLines(v);
  if (!body.length) return notes;
  body[0] = `[${fmt(ms)}] ${body[0]}`;
  const blocks = parseMemo(notes);
  const lines = notes ? notes.split('\n') : [];
  const before = blocks.filter((b) => b.ms <= ms).pop();
  let at = before ? before.to : blocks.find((b) => b.ms > ms)?.from;
  if (at == null) at = lines.length && lines[lines.length - 1] === '' ? lines.length - 1 : lines.length;
  lines.splice(at, 0, ...body);
  return lines.join('\n');
}
// 메모의 코멘트 하나를 고친다 (시각 · 목록 표시는 그대로) — 빈 글이면 지운다
function rewriteMemoComment(notes, b, v) {
  const lines = notes.split('\n');
  const body = commentLines(v);
  if (body.length) {
    body[0] = b.prefix + body[0];
    lines.splice(b.from, b.to - b.from, ...body);
    return lines.join('\n');
  }
  lines.splice(b.from, b.to - b.from);
  // 지운 자리 앞뒤로 빈 줄이 겹치면 하나만
  const blank = (s) => s != null && !s.trim();
  const prevBlank = b.from === 0 || blank(lines[b.from - 1]);
  if (prevBlank && blank(lines[b.from])) lines.splice(b.from, 1);
  else if (prevBlank && b.from > 0 && b.from === lines.length) lines.splice(b.from - 1, 1);
  return lines.join('\n');
}
// 녹취록에서 코멘트가 붙는 문장: 그 시각에 말하던 문장, 말 사이 빈 곳이면 앞뒤 중 가까운 문장
// (시각은 초 단위라 [20:35] 는 20:35.000~20:35.999 — 문장에서 단 코멘트는 문장 시작 초를 적으므로 다시 그 문장에 붙는다)
function anchorOf(sorted, ms) {
  let lo = 0, hi = sorted.length;
  while (lo < hi) { const mid = (lo + hi) >> 1; if (sorted[mid].startMs < ms + 1000) lo = mid + 1; else hi = mid; }
  const p = sorted[lo - 1], n = sorted[lo];
  if (!p) return n || null;
  if (!n || p.endMs >= ms) return p;
  return ms - p.endMs <= n.startMs - ms ? p : n;
}
// 메모의 코멘트들과 붙는 문장: { list: [{ ...코멘트, u }], byU: 문장 id → [코멘트] }
function memoComments(d) {
  const list = parseMemo(d?.notes);
  const byU = new Map();
  if (!list.length) return { list, byU };
  let sorted = d.utterances;
  for (let i = 1; i < sorted.length; i++) {
    if (sorted[i].startMs < sorted[i - 1].startMs) { sorted = [...sorted].sort((a, b) => a.startMs - b.startMs || a.id - b.id); break; }
  }
  for (const c of list) {
    c.u = anchorOf(sorted, c.ms);
    if (!c.u) continue;
    if (!byU.has(c.u.id)) byU.set(c.u.id, []);
    byU.get(c.u.id).push(c);
  }
  for (const cs of byU.values()) cs.sort((a, b) => a.ms - b.ms || a.from - b.from);
  return { list, byU };
}

function commentHtml(cs) {
  if (!cs?.length) return '';
  return `<div class="cmt">${cs.map((c) => `<div class="ce" data-line="${c.from}" title="코멘트 고치기 — 메모의 [${fmt(c.ms)}] 줄"><svg><use href="#i-comment"/></svg><span><time>${fmt(c.ms)}</time>${esc(c.text)}</span></div>`).join('')}</div>`;
}

// 노트를 펼칠지: 넓은 창은 처음부터 펼쳐 두고, 좁은 창은 눌러서 연다 (각각 마지막으로 고른 대로)
const wideMq = matchMedia('(min-width: 760px)');
const NTABS = ['memo', 'summary', 'ask', 'comments'];
const notesPref = { wide: null, narrow: null };
let notesW = 0; // 0: 창 크기에 맞춰 (CSS) · 끌어서 정하면 그 너비
try {
  notesPref.wide = localStorage.getItem('notes.wide');
  notesPref.narrow = localStorage.getItem('notes.narrow');
  const t = localStorage.getItem('notes.tab');
  if (NTABS.includes(t)) S.ntab = t;
  notesW = +localStorage.getItem('notes.w') || 0;
} catch { /* 저장소를 못 쓰면 기본값 */ }
// 본문 칸이 넉넉하면(side-cmt) 코멘트를 녹취록 옆 여백에 두고, 노트 칸의 코멘트 탭은 숨긴다.
// 그때 코멘트 탭을 골라 두었으면 메모를 보여 준다 (고른 것은 기억해 두었다가 좁아지면 다시)
const SIDE_CMT_MIN = 940;
let sideCmt = false;
const curTab = () => (sideCmt && S.ntab === 'comments' ? 'memo' : S.ntab);
const shownTabs = () => NTABS.filter((t) => !(sideCmt && t === 'comments'));
const layoutKey = () => (wideMq.matches ? 'wide' : 'narrow');
function notesWanted() {
  const v = notesPref[layoutKey()];
  return v == null ? wideMq.matches : v === '1';
}
function setNotesWanted(on) {
  const k = layoutKey();
  notesPref[k] = on ? '1' : '0';
  try { localStorage.setItem(`notes.${k}`, notesPref[k]); } catch { /* 이번만 */ }
}
wideMq.addEventListener('change', () => scheduleRender());
const notesShown = () => !$('#notes').hidden;

let memoFor = null;
let cmtShown = '';
function renderNotes() {
  const d = doc();
  const show = !!d && notesWanted() && !S.mini;
  $('#notes').hidden = !show;
  $('#notes-grip').hidden = !show;
  const nb = $('#btn-notes');
  nb.classList.toggle('on', show);
  nb.disabled = !d;
  placeSummary(show);
  if (!show) return;
  const tab = curTab();
  $$('#notes-tabs [data-ntab]').forEach((b) => {
    const on = b.dataset.ntab === tab;
    b.classList.toggle('on', on);
    b.setAttribute('aria-selected', String(on));
    b.tabIndex = on ? 0 : -1;
  });
  $$('#notes [data-npage]').forEach((p) => (p.hidden = p.dataset.npage !== tab));
  // 메모 칸: 다른 기록으로 바뀌었거나 쓰고 있지 않을 때만 채운다 (치는 중인 글자를 덮지 않게)
  const m = $('#memo');
  if (memoFor !== d.id || document.activeElement !== m) {
    const v = d.notes || '';
    if (m.value !== v) m.value = v;
    if (memoFor !== d.id) { m.scrollTop = 0; setMemoState(''); }
    memoFor = d.id;
  }
  renderCommentList(d);
  renderAsk();
}
// 요약 카드: 노트가 열려 있으면 노트의 요약 탭으로, 닫혀 있으면 녹취록 위로
function placeSummary(inNotes) {
  const card = $('#summary-card');
  const slot = inNotes ? $('#notes-summary-slot') : body;
  if (card.parentElement === slot) return;
  slot.prepend(card);
  if (inNotes) card.classList.remove('folded');
}
// 코멘트 탭 (좁은 창): 메모의 시각 줄을 시각 순으로, 붙은 문장과 함께
function renderCommentList(d) {
  const items = memoComments(d).list.slice().sort((a, b) => a.ms - b.ms || a.from - b.from);
  $('#cmt-count').textContent = items.length || '';
  if (curTab() !== 'comments') return;
  const html = items.length
    ? items.map(({ u, ms, text }) => {
      if (!u) {
        return `<div class="cmt-item"><div class="cmt-meta"><span>${fmt(ms)}</span><span>· 아직 붙을 문장이 없어요</span></div><div class="cmt-text">${esc(text)}</div></div>`;
      }
      const s = resolveSpk(d, u.speaker);
      return `<button class="cmt-item" data-goto="${u.id}" style="--c:${spkColor(s)}" title="녹취록에서 이 문장 보기">
        <div class="cmt-meta"><b>${esc(speakerName(d, s))}</b><span>${fmt(ms)}</span></div>
        <div class="cmt-src">${esc(u.text)}</div><div class="cmt-text">${esc(text)}</div></button>`;
    }).join('')
    : '<div class="n-empty"><svg><use href="#i-comment"/></svg><b>아직 코멘트가 없어요</b><p>녹취록 문장에 마우스를 올리고 \'코멘트\'를 누르거나<br>메모에 <code>[12:34]</code> 처럼 시각으로 시작하는 줄을 쓰면<br>그 시각 문장에 코멘트로 붙어요.</p></div>';
  if (html !== cmtShown) { $('#cmt-list').innerHTML = html; cmtShown = html; }
}
function showNotesTab(t) {
  if (!NTABS.includes(t)) return;
  S.ntab = t;
  try { localStorage.setItem('notes.tab', t); } catch { /* 이번만 */ }
  cmtShown = '';
  renderNotes();
  renderSummary();
}
// on: 펼치기/접기 (없으면 뒤집기) · memo: 펼치면서 메모 칸에 바로 쓰게
function toggleNotes(on, memo) {
  if (!doc() || S.mini) return;
  const want = typeof on === 'boolean' ? on : !notesShown();
  setNotesWanted(want);
  renderNotes();
  renderSummary();
  if (want && memo) { showNotesTab('memo'); $('#memo').focus(); }
}
$('#btn-notes').addEventListener('click', () => toggleNotes());
$('#btn-notes-close').addEventListener('click', () => toggleNotes(false));
$('#notes-tabs').addEventListener('click', (e) => {
  const b = e.target.closest('[data-ntab]');
  if (b) showNotesTab(b.dataset.ntab);
});
$('#notes-tabs').addEventListener('keydown', (e) => {
  const step = { ArrowRight: 1, ArrowLeft: -1 }[e.key];
  if (!step) return;
  e.preventDefault();
  const tabs = shownTabs();
  const next = tabs[(tabs.indexOf(curTab()) + step + tabs.length) % tabs.length];
  showNotesTab(next);
  $(`#notes-tabs [data-ntab="${next}"]`).focus();
});

// 노트 너비 (넓은 창): 사이 막대를 끌어서
const work = $('#work');
const applyNotesW = () => (notesW ? work.style.setProperty('--notes-w', `${notesW}px`) : work.style.removeProperty('--notes-w'));
applyNotesW();
new ResizeObserver(([e]) => {
  const on = e.contentRect.width >= SIDE_CMT_MIN;
  if (on === sideCmt) return;
  sideCmt = on;
  work.classList.toggle('side-cmt', on);
  if (!editing) hideTools();
  cmtShown = '';
  renderNotes();
}).observe($('#main-col'));
$('#notes-grip').addEventListener('pointerdown', (e) => {
  if (e.button !== 0) return;
  e.preventDefault();
  const grip = e.currentTarget;
  grip.setPointerCapture(e.pointerId);
  grip.classList.add('drag');
  $('#app').classList.add('resizing');
  const right = work.getBoundingClientRect().right;
  const move = (m) => {
    const max = Math.max(280, work.clientWidth - 360);
    notesW = Math.round(Math.min(max, Math.max(280, right - m.clientX)));
    applyNotesW();
  };
  const up = () => {
    grip.removeEventListener('pointermove', move);
    grip.classList.remove('drag');
    $('#app').classList.remove('resizing');
    try { localStorage.setItem('notes.w', String(notesW)); } catch { /* 이번만 */ }
  };
  grip.addEventListener('pointermove', move);
  grip.addEventListener('pointerup', up, { once: true });
  grip.addEventListener('pointercancel', up, { once: true });
});
$('#notes-grip').addEventListener('dblclick', () => {
  notesW = 0;
  applyNotesW();
  try { localStorage.removeItem('notes.w'); } catch { /* 이번만 */ }
});

// 메모: 치는 대로 (잠깐 멈추면) 저장
const memo = $('#memo');
let memoReq = 0;
function setMemoState(t, err = false) {
  const s = $('#memo-state');
  s.textContent = t;
  s.classList.toggle('err', err);
}
const memoSave = debounce((d) => {
  const my = ++memoReq;
  api.invoke('save_session', { doc: d })
    .then(() => { if (my === memoReq && memoFor === d.id) setMemoState('저장됨'); })
    .catch((e) => { if (memoFor === d.id) setMemoState('저장하지 못했어요', true); toast(String(e), 'error'); });
}, 700);
// 코멘트 칸에서 메모를 바꿨을 때 메모 칸도 바로 맞춘다 — 코멘트 칸 바깥(메모 칸)을 눌러 저장하면 메모 칸이
// 먼저 포커스를 받아 renderNotes 가 건너뛰므로, 다음 타이핑이 예전 글로 코멘트 수정을 되돌리지 않게.
// 커서 · 선택은 바뀐 곳 뒤라면 늘거나 준 만큼 옮긴다
function syncMemoBox(d) {
  if (memoFor !== d.id || memo.value === (d.notes || '')) return;
  const old = memo.value, nv = d.notes || '';
  let p = 0;
  const n = Math.min(old.length, nv.length);
  while (p < n && old[p] === nv[p]) p++;
  const fix = (i) => (i <= p ? i : Math.max(p, i + nv.length - old.length));
  const s = fix(memo.selectionStart ?? 0), e = fix(memo.selectionEnd ?? 0), top = memo.scrollTop;
  memo.value = nv;
  memo.setSelectionRange(s, e);
  memo.scrollTop = top;
}
// 메모의 시각 줄이 녹취록 옆 코멘트로 보이게 — 치는 동안 매번이 아니라 잠깐 멈추면 다시 그린다
const memoRender = debounce(() => scheduleRender(), 250);
memo.addEventListener('input', () => {
  const d = doc();
  if (!d || memoFor !== d.id) return;
  d.notes = memo.value;
  setMemoState('저장 중…');
  memoSave(d);
  memoRender();
});
memo.addEventListener('blur', () => memoSave.flush());
// 지금 녹음 위치 · 듣고 있는 곳 (ms) — 둘 다 아니면 null
function nowMs() {
  if (S.live && S.rec === 'recording' && !S.fileMode) return S.livePosMs || Math.max(0, Date.now() - S.timerStart);
  if (P.src && P.started) return audio.currentTime * 1000;
  return null;
}
function insertTime() {
  const ms = nowMs();
  if (!doc()) return;
  if (ms == null) { toast('녹음 중이거나 녹음을 듣는 중일 때 시각을 넣을 수 있어요'); return; }
  const s = memo.selectionStart ?? memo.value.length;
  const e = memo.selectionEnd ?? s;
  const before = memo.value.slice(0, s);
  memo.focus();
  memo.setRangeText(`${before && !before.endsWith('\n') ? '\n' : ''}[${fmt(ms)}] `, s, e, 'end');
  memo.dispatchEvent(new Event('input'));
}
$('#btn-memo-time').addEventListener('mousedown', (e) => e.preventDefault()); // 메모 칸의 커서 자리를 지킨다
$('#btn-memo-time').addEventListener('click', insertTime);

// 메모의 커서가 코멘트(시각 줄) 안에 있으면 '12:34 문장 보기' — 누르거나 그 줄을 Ctrl+클릭하면 녹취록의 그 문장으로
function memoCaretComment() {
  const d = doc();
  if (!d || memoFor !== d.id) return null;
  const line = memo.value.slice(0, memo.selectionStart ?? 0).split('\n').length - 1;
  return memoComments(d).list.find((c) => c.from <= line && line < c.to && c.u) || null;
}
let memoGotoId = null;
function syncMemoGoto() {
  const c = document.activeElement === memo ? memoCaretComment() : null;
  const b = $('#btn-memo-goto');
  memoGotoId = c ? c.u.id : null;
  b.hidden = !c;
  if (c) b.querySelector('span').textContent = `${fmt(c.ms)} 문장 보기`;
}
for (const ev of ['keyup', 'click', 'input', 'focus', 'blur']) memo.addEventListener(ev, syncMemoGoto);
memo.addEventListener('click', (e) => {
  if (!e.ctrlKey && !e.metaKey) return;
  const c = memoCaretComment();
  if (c) gotoUtterance(c.u.id);
});
$('#btn-memo-goto').addEventListener('mousedown', (e) => e.preventDefault());
$('#btn-memo-goto').addEventListener('click', () => { if (memoGotoId != null) gotoUtterance(memoGotoId); });

// 요약 고치기
function startSummaryEdit() {
  const d = doc();
  if (!d) return;
  if (S.summarizing && S.sumId === d.id) { toast('요약을 만드는 중이에요. 끝난 뒤에 고칠 수 있어요'); return; }
  if (S.sumEdit) {
    const ta = $('#sum-edit');
    if (ta) { ta.focus(); return; }
    S.sumEdit = null; // 입력 칸이 사라진 편집 상태는 버리고 새로 연다 (버튼이 먹통이 되지 않게)
  }
  S.sumEdit = { doc: d, orig: d.summary || '' };
  const card = $('#summary-card');
  card.hidden = false;
  card.classList.remove('folded');
  card.classList.add('editing');
  $('#summary-empty').hidden = true;
  $('#sum-mark').hidden = true;
  $('#summary-body').innerHTML = `<textarea class="sum-edit" id="sum-edit" spellcheck="false" placeholder="회의 요약을 적어 주세요.&#10;### 소제목 · - 목록 · - [ ] 할 일 · **굵게** 를 쓸 수 있어요."></textarea>
    <div class="sum-edit-foot"><small>Ctrl+Enter 저장 · Esc 취소</small><button class="btn ghost sm" data-sum="cancel">취소</button><button class="btn sm" data-sum="save">저장</button></div>`;
  const ta = $('#sum-edit');
  ta.value = S.sumEdit.orig;
  ta.style.height = `${Math.min(Math.max(220, ta.scrollHeight + 4), Math.round(window.innerHeight * 0.6))}px`;
  ta.focus();
  ta.setSelectionRange(0, 0);
  ta.scrollTop = 0;
}
function endSummaryEdit(save) {
  const e = S.sumEdit;
  if (!e) return;
  const v = ($('#sum-edit')?.value ?? e.orig).replace(/\s+$/, '');
  S.sumEdit = null;
  $('#summary-card').classList.remove('editing');
  if (save && v.trim() !== e.orig.trim()) {
    e.doc.summary = v.trim() ? v : null;
    e.doc.summaryEdited = !!v.trim();
    persistDoc(e.doc);
    toast(v.trim() ? '요약을 저장했어요' : '요약을 비웠어요');
  }
  renderSummary();
}
async function cancelSummaryEdit() {
  const e = S.sumEdit;
  if (!e) return;
  const v = $('#sum-edit')?.value ?? '';
  if (v.trim() !== e.orig.trim() && !(await api.ask('고친 요약을 버릴까요?', { title: '받아쓰기', kind: 'warning' }))) return;
  if (S.sumEdit === e) endSummaryEdit(false);
}
$('#btn-edit-summary').addEventListener('click', startSummaryEdit);
$('#btn-summary-write').addEventListener('click', startSummaryEdit);
$('#summary-card').addEventListener('click', (e) => {
  const b = e.target.closest('[data-sum]');
  if (!b) return;
  if (b.dataset.sum === 'save') endSummaryEdit(true);
  else cancelSummaryEdit();
});
$('#summary-card').addEventListener('keydown', (k) => {
  if (k.target.id !== 'sum-edit') return;
  if (k.key === 'Enter' && k.ctrlKey) { k.preventDefault(); endSummaryEdit(true); }
  if (k.key === 'Escape') { k.stopPropagation(); cancelSummaryEdit(); }
});

// 문장 코멘트: 팝오버에서 쓰고, 바깥을 누르거나 Enter 로 저장 — 저장하면 메모의 [시각] 줄이 바뀐다.
// line: 고칠 코멘트(메모의 줄 번호) · 없으면 그 문장의 첫 코멘트, 코멘트가 없으면 새로
function openComment(id, anchor, line = null) {
  const d = doc();
  const u = d?.utterances.find((x) => x.id === id);
  if (!u || !anchor) return;
  if (S.cmt) closeComment(true);
  const mine = memoComments(d).byU.get(id) || [];
  const c = line != null ? mine.find((x) => x.from === line) || mine[0] : mine[0];
  const s = resolveSpk(d, u.speaker);
  const q = $('#cmt-quote');
  q.style.setProperty('--c', spkColor(s));
  q.innerHTML = `<b>${esc(speakerName(d, s))}</b><span>${fmt(c ? c.ms : u.startMs)}</span><p>${esc(u.text)}</p>`;
  const cur = c?.text || '';
  const ta = $('#cmt-input');
  ta.value = cur;
  $('#cmt-del').hidden = !c;
  showPopover($('#pop-cmt'), anchor, 'left', true);
  S.cmt = { doc: d, id, ms: c ? c.ms : u.startMs, line: c ? c.from : null, raw: c ? (d.notes || '').split('\n').slice(c.from, c.to).join('\n') : null };
  // 녹음 중이어도 쓰는 동안 화면이 새 문장 쪽으로 끌려 내려가지 않게
  follow = false;
  hideTools();
  ta.focus();
  ta.setSelectionRange(cur.length, cur.length);
}
function closeComment(save) {
  const c = S.cmt;
  if (!c) return;
  S.cmt = null;
  $('#pop-cmt').hidden = true;
  if (!save) return;
  const v = commentLines($('#cmt-input').value).join('\n');
  const d = c.doc;
  const notes = d.notes || '';
  let next;
  if (c.line == null) {
    if (!v) return;
    next = insertMemoComment(notes, c.ms, v);
  } else {
    // 여는 사이에 메모가 바뀌었을 수 있다 → 같은 글의 코멘트를 다시 찾는다
    const lines = notes.split('\n');
    const same = (b) => lines.slice(b.from, b.to).join('\n') === c.raw;
    const blocks = parseMemo(notes);
    const b = blocks.find((x) => x.from === c.line && same(x)) || blocks.find(same);
    if (!b) { toast('그 사이 메모가 바뀌어서 코멘트를 저장하지 못했어요. 메모에서 고쳐 주세요', 'warn'); return; }
    if (v === b.text) return;
    next = rewriteMemoComment(notes, b, v);
  }
  d.notes = next;
  syncMemoBox(d);
  persistDoc(d);
  scheduleRender();
  toast(v ? (c.line != null ? '코멘트를 고쳤어요' : '코멘트를 달았어요 — 메모에도 시각 줄로 적었어요') : '코멘트를 지웠어요');
}
$('#cmt-input').addEventListener('keydown', (k) => {
  // 한글 조합 중의 Enter 는 글자를 마무리하는 것 — 저장하지 않는다
  if (k.isComposing || k.keyCode === 229) return;
  if (k.key === 'Enter' && !k.shiftKey) { k.preventDefault(); closeComment(true); }
  if (k.key === 'Escape') { k.stopPropagation(); closeComment(false); }
});
$('#cmt-save').addEventListener('click', () => closeComment(true));
$('#cmt-del').addEventListener('click', () => { $('#cmt-input').value = ''; closeComment(true); });
$('#transcript').addEventListener('click', (e) => {
  // 코멘트 카드: 누른 코멘트를 고친다 (카드 여백이면 첫 코멘트) — 카드는 그 문장 바로 뒤에 붙어 있다
  const box = e.target.closest('.cmt');
  if (box) {
    const uel = box.previousElementSibling;
    const ce = e.target.closest('.ce[data-line]') || box.querySelector('.ce[data-line]');
    if (uel?.matches('.u[data-id]') && ce) openComment(+uel.dataset.id, ce, +ce.dataset.line);
    return;
  }
  // 넓은 본문: 문장 오른쪽 여백의 '+ 코멘트' — 그 자리 아래에 코멘트 칸을 연다
  const el = e.target.closest('.u[data-id]');
  if (editing || !ghostHit(el, e)) return;
  clearTimeout(playClick);
  const r = el.getBoundingClientRect();
  const x = r.right + 24;
  openComment(+el.dataset.id, { getBoundingClientRect: () => ({ left: x, right: x + 90, top: r.top, bottom: r.top + 26, width: 90, height: 26 }) });
});
// 넓은 본문에서 문장의 '+ 코멘트'(문장 상자 오른쪽 밖에 그린 ::after)를 가리키는지
function ghostHit(el, e, slack = 0) {
  return sideCmt && !!el && e.clientX > el.getBoundingClientRect().right + slack;
}
// '+ 코멘트' 위에 올리면 눌리는 버튼처럼 보이게
let ghostEl = null;
body.addEventListener('mousemove', (e) => {
  if (!sideCmt && !ghostEl) return;
  const el = e.target.closest?.('#transcript .u[data-id]');
  const hot = el && ghostHit(el, e, 20) ? el : null;
  if (hot === ghostEl) return;
  ghostEl?.classList.remove('ghost-hot');
  ghostEl = hot;
  hot?.classList.add('ghost-hot');
});
$('#cmt-list').addEventListener('click', (e) => {
  const b = e.target.closest('[data-goto]');
  if (b) gotoUtterance(+b.dataset.goto);
});
// 녹취록에서 그 문장으로 가서 잠깐 빛나게
function gotoUtterance(id) {
  const el = $(`#transcript .u[data-id="${id}"]`);
  if (!el) return;
  follow = false;
  const center = () => {
    const r = el.getBoundingClientRect(), b = body.getBoundingClientRect();
    body.scrollTo({ top: body.scrollTop + r.top - b.top - body.clientHeight / 3, behavior: 'instant' });
  };
  center();
  requestAnimationFrame(center); // 화면 밖 문단은 그려지면서 높이가 바뀐다
  el.classList.remove('flash');
  void el.offsetWidth;
  el.classList.add('flash');
  setTimeout(() => el.classList.remove('flash'), 1900);
}

// 문장 위에 마우스를 올리면 뜨는 도구: 고치기 · 코멘트 (문장마다 버튼을 두지 않고 하나를 옮겨 다닌다)
const tools = $('#u-tools');
let toolsId = null;
function showTools(el, hint = false) {
  if (!el || S.mini) return;
  toolsId = +el.dataset.id;
  tools.classList.toggle('hinting', hint);
  tools.hidden = false;
  const r = el.getBoundingClientRect(), b = body.getBoundingClientRect();
  const top = r.top - b.top + body.scrollTop;
  tools.style.top = `${hint ? top + r.height + 4 : Math.max(0, top - 18)}px`;
  tools.style.left = `${Math.max(4, r.right - b.left + body.scrollLeft - tools.offsetWidth + 6)}px`;
}
function hideTools() {
  tools.hidden = true;
  toolsId = null;
}
body.addEventListener('mouseover', (e) => {
  if (editing || e.target.closest('#u-tools')) return;
  const el = e.target.closest('#transcript .u[data-id]');
  if (!el) { if (toolsId != null) hideTools(); return; }
  if (+el.dataset.id !== toolsId || tools.hidden) showTools(el);
});
body.addEventListener('mouseleave', () => { if (!editing) hideTools(); });
tools.addEventListener('mousedown', (e) => e.preventDefault());
tools.addEventListener('click', (e) => {
  const b = e.target.closest('button[data-tool]');
  // 녹음 중에는 문단이 다시 그려질 수 있어 id 로 다시 찾는다
  const el = toolsId != null && $(`#transcript .u[data-id="${toolsId}"]`);
  if (!b || !el) { hideTools(); return; }
  if (b.dataset.tool === 'edit') startEdit(el);
  else openComment(toolsId, el);
});

/* ───────────── 창 크게 (최대화 · 전체 화면) ───────────── */
const WIN = { max: false, full: false };
async function syncMaxed() {
  try {
    WIN.max = !!(await api.win.isMaximized());
    WIN.full = !!(await api.win.isFullscreen());
  } catch { return; }
  const big = WIN.max || WIN.full;
  $('#app').classList.toggle('maxed', big);
  $('#btn-max use').setAttribute('href', big ? '#i-restore' : '#i-maximize');
  $('#btn-max').title = WIN.full ? '전체 화면 끝내기 (F11 · Esc)' : WIN.max ? '원래 크기로 (F11: 전체 화면)' : '화면 가득 크게 보기 (F11: 전체 화면)';
}
async function toggleMax() {
  if (S.mini) return;
  try {
    if (WIN.full) await api.win.setFullscreen(false);
    else await api.win.toggleMaximize();
  } catch (e) { toast(String(e), 'error'); }
  setTimeout(syncMaxed, 80);
}
async function toggleFullscreen() {
  if (S.mini) return;
  try { await api.win.setFullscreen(!WIN.full); } catch (e) { toast(String(e), 'error'); }
  setTimeout(syncMaxed, 80);
}
$('#btn-max').addEventListener('click', toggleMax);
api.win.onResized?.(debounce(syncMaxed, 120));

// 복사 · 저장
$('#btn-copy').addEventListener('click', async () => {
  const d = doc();
  if (!d) return;
  const text = await call('session_text', { id: d.id, format: 'txt' });
  await navigator.clipboard.writeText(text);
  toast('전체 녹취록을 복사했어요');
});
$('#btn-copy-summary').addEventListener('click', async () => {
  const d = doc();
  if (d?.summary) { await navigator.clipboard.writeText(d.summary); toast('요약을 복사했어요'); }
});
$('#btn-fold-summary').addEventListener('click', () => $('#summary-card').classList.toggle('folded'));

function showPopover(pop, anchor, align = 'left', below = false) {
  hidePopovers();
  pop.hidden = false;
  const a = anchor.getBoundingClientRect();
  const p = pop.getBoundingClientRect();
  const appR = $('#app').getBoundingClientRect();
  let left = align === 'right' ? a.right - p.width : a.left;
  left = Math.max(8, Math.min(left, appR.width - p.width - 8));
  pop.style.left = `${left}px`;
  let top = below ? a.bottom + 6 : a.top - p.height - 8;
  if (below && top + p.height > appR.height - 8) top = Math.max(8, a.top - p.height - 6);
  pop.style.top = `${top}px`;
}
function hidePopovers() {
  if (S.cmt) closeComment(true); // 쓰던 코멘트는 바깥을 눌러도 저장
  $$('.popover').forEach((p) => (p.hidden = true));
}
document.addEventListener('mousedown', (e) => { if (!e.target.closest('.popover, #btn-source, #btn-save, .spk-chip, .g-name')) hidePopovers(); });

$('#btn-save').addEventListener('click', (e) => showPopover($('#pop-save'), e.currentTarget, 'right'));
$('#pop-save').addEventListener('click', async (e) => {
  const b = e.target.closest('button[data-fmt]');
  const d = doc();
  if (!b || !d) return;
  hidePopovers();
  const fmtName = b.dataset.fmt;
  const safe = (d.title || d.id).replace(/[\\/:*?"<>|]/g, '_');
  const path = await api.save({ defaultPath: `${safe}.${fmtName}`, filters: [{ name: fmtName.toUpperCase(), extensions: [fmtName] }] });
  if (!path) return;
  await call('export_session', { id: d.id, format: fmtName, path });
  toast('저장했어요');
});

$('#btn-source').addEventListener('click', (e) => {
  if (S.mini) {
    const order = ['mic', 'system', 'both'];
    S.settings.source = order[(order.indexOf(S.settings.source) + 1) % order.length];
    saveSettings();
    renderDock();
    toast(`입력: ${SOURCE_LABEL[S.settings.source]}${S.rec !== 'idle' ? ' (다음 녹음부터)' : ''}`);
    return;
  }
  $$('#pop-source button').forEach((b) => b.classList.toggle('on', b.dataset.source === S.settings.source));
  showPopover($('#pop-source'), e.currentTarget);
});
$('#pop-source').addEventListener('click', (e) => {
  const b = e.target.closest('button[data-source]');
  if (!b) return;
  hidePopovers();
  S.settings.source = b.dataset.source;
  saveSettings();
  renderDock();
  if (S.rec !== 'idle') toast('다음 녹음부터 적용됩니다');
  else if (b.dataset.source === 'both') toast('스피커 소리가 마이크로 다시 들어가면 중복될 수 있어요. 헤드셋을 권장해요.');
});

// 요약
$('#btn-summary').addEventListener('click', () => runSummary());
$('#btn-summary-ai').addEventListener('click', () => runSummary());
async function runSummary() {
  const d = doc();
  if (!d || S.summarizing) return;
  if (S.sumEdit?.doc === d) { toast('요약을 고치는 중이에요. 저장하거나 취소한 뒤에 AI 로 요약할 수 있어요', 'warn'); return; }
  // 직접 고친 요약은 묻고 바꾼다
  if (d.summary && d.summaryEdited
    && !(await api.ask('직접 고친 요약이 있어요.\nAI 로 새로 요약하면 지금 요약이 바뀌어요. 계속할까요?', { title: '받아쓰기', kind: 'warning' }))) return;
  if (doc() !== d || S.summarizing) return;
  if (S.rec !== 'idle' && S.live) toast('지금까지의 내용으로 요약할게요');
  // 방금 고친 문장이 요약에 들어가게, 기다리던 저장을 먼저 보낸다 (요약은 그 저장이 끝난 뒤 기록을 읽는다)
  persistDoc.flush();
  S.summarizing = true;
  S.sumCancelling = false;
  S.sumProgress = null;
  S.sumPct = 0;
  S.sumStarted = Date.now();
  // 걸린 시간은 진행 소식이 없어도 (녹취록을 읽는 동안에도) 1초마다
  clearInterval(S.sumTimer);
  S.sumTimer = setInterval(renderSumProgress, 1000);
  S.sumId = d.id;
  // 요약마다 번호 — 앞선 저장을 기다리는 동안 멈춰도 앱이 그 요약을 알아보고 시작하자마자 멈춘다
  S.sumToken = Date.now();
  $('#summary-card').classList.remove('folded');
  renderSummary();
  updateActions();
  if (notesShown()) {
    // 노트가 열려 있으면 요약은 노트의 요약 탭에 — 녹취록은 보던 곳 그대로
    showNotesTab('summary');
  } else {
    // 녹음 중에 요약해도 위에 뜬 요약 카드를 보고 있게 따라가기를 멈춘다
    follow = false;
    body.scrollTop = 0;
  }
  try {
    const r = await api.invoke('summarize', { id: d.id, token: S.sumToken });
    d.summary = r.text;
    d.summaryEdited = false;
    if (r.title) { d.title = r.title; d.autoTitle = false; renderMeta(); }
  } catch (e) {
    // 사람이 멈춘 것은 오류가 아니다
    const msg = String(e);
    toast(msg, msg === '요약을 멈췄어요' ? undefined : 'error');
  } finally {
    clearInterval(S.sumTimer);
    S.summarizing = false;
    S.sumCancelling = false;
    S.sumProgress = null;
    renderSummary();
    updateActions();
  }
}
$('#summary-card').addEventListener('click', (e) => {
  const b = e.target.closest('#btn-cancel-summary');
  if (!b || !S.summarizing || S.sumCancelling) return;
  S.sumCancelling = true;
  renderSummary();
  api.invoke('cancel_summary', { token: S.sumToken }).catch(() => {});
});

/* ───────────── 회의에 묻기 ───────────── */
// 요약에 쓰는 AI(설정 › 요약)로 이 회의에 묻는다. 묻고 받은 답은 앱이 기록에 남기고(화면은 고치지 않는다)
// Markdown 회의록에도 들어간다. 답의 [12:34] 는 눌러서 녹취록 그 문장으로 간다.
// S.ask: 지금 받는 답 { id, doc, token, q, text, waitSecs, restarted, started, cancelling, shown }
const askList = $('#ask-list');
const askInput = $('#ask-input');
const ASK_CANCELLED = '답을 멈췄어요';
let askShown = '';
const TS_RE = /\[(?:(\d{1,3}):)?(\d{1,3}):(\d{2})\]/g;
// 답 → HTML (목록 · 굵게). 녹취록 시각은 누를 수 있게
function askHtml(text) {
  return md(text).replace(TS_RE, (m, h, mi, s) => {
    if (+s >= 60 || (h != null && +mi >= 60)) return m;
    const ms = ((+h || 0) * 3600 + +mi * 60 + +s) * 1000;
    return `<button class="ts" data-ms="${ms}" title="녹취록에서 이 문장 보기">${fmt(ms)}</button>`;
  });
}
// 물어볼 만한 것 — 화자 이름을 붙였으면 그 사람 의견도
function askSuggestions(d) {
  const out = ['결정된 것만 정리해 줘', '할 일과 담당자 · 기한은?', '숫자나 금액이 나온 곳은?', '의견이 갈린 부분은?'];
  const spk = d.utterances.map((u) => resolveSpk(d, u.speaker)).find((s) => s != null && d.speakers?.[s]);
  if (spk != null) out.splice(2, 0, `${speakerName(d, spk)} 의견만 정리해 줘`);
  return out;
}
const canAsk = (d) => !!(d && (d.utterances.length || (d.summary || '').trim() || (d.notes || '').trim()));
function qaHtml(x, i) {
  const meta = [String(x.at || '').slice(11, 16), x.model].filter(Boolean).join(' · ');
  return `<div class="qa" data-i="${i}"><div class="qa-q">${esc(x.q)}</div><div class="qa-a md">${askHtml(x.a)}</div>`
    + (x.partial ? '<div class="qa-part">회의가 길어 질문과 관련 있어 보이는 구간 위주로 보고 답했어요</div>' : '')
    + `<div class="qa-meta"><span title="${esc(meta)}">${esc(meta)}</span>`
    + '<button data-qa="memo" title="질문과 답을 메모 끝에 붙여요">메모에 넣기</button><button data-qa="copy">복사</button>'
    + '<button data-qa="del" title="이 질문과 답 지우기">지우기</button></div></div>';
}
function renderAsk() {
  const d = doc();
  if (!d || curTab() !== 'ask' || !notesShown()) return;
  const pend = S.ask && S.ask.id === d.id ? S.ask : null;
  const items = d.qa || [];
  let html;
  if (!items.length && !pend) {
    html = '<div class="n-empty"><svg><use href="#i-ask"/></svg><b>이 회의에 물어보세요</b>'
      + (canAsk(d)
        ? '<p>요약에 쓰는 AI 가 녹취록 · 요약 · 메모를 보고 답해요.<br>답에 붙은 시각을 누르면 녹취록 그 문장으로 가요.</p>'
          + `<div class="ask-sugs">${askSuggestions(d).map((s) => `<button class="chip-btn" data-sug="${esc(s)}">${esc(s)}</button>`).join('')}</div>`
        : '<p>받아 적은 내용이 생기면 물어볼 수 있어요.</p>')
      + '</div>';
  } else {
    html = items.map(qaHtml).join('')
      + (pend ? `<div class="qa" id="qa-pending" data-token="${pend.token}"></div>` : '')
      + (items.length && !pend ? '<div class="qa-tail"><button class="link" data-qa="clear">묻고 답한 것 모두 지우기</button></div>' : '');
  }
  if (html !== askShown) {
    const atEnd = askList.scrollHeight - askList.scrollTop - askList.clientHeight < 60;
    const grew = askShown && html.length > askShown.length;
    askList.innerHTML = html;
    askShown = html;
    if (atEnd || grew || pend) askList.scrollTop = askList.scrollHeight;
  }
  if (pend) renderAskPending();
  renderAskSend();
  fitAsk();
}
// 보내기 단추: 답을 받는 동안은 멈추기
function renderAskSend() {
  const d = doc();
  const pend = S.ask && d && S.ask.id === d.id ? S.ask : null;
  const send = $('#ask-send');
  send.classList.toggle('stop', !!pend);
  send.title = pend ? (pend.cancelling ? '멈추는 중…' : '답 멈추기') : '보내기 (Enter · 줄바꿈은 Shift+Enter)';
  send.disabled = pend ? !!pend.cancelling : (!!S.ask || !canAsk(d) || !askInput.value.trim());
  askInput.disabled = !canAsk(d);
  $$('#ask-list [data-sug]').forEach((b) => (b.disabled = !!S.ask));
}
// 받는 중인 답 — 글이 올 때마다 그 칸만 다시 그린다
let askFrame = 0;
function renderAskPending() {
  askFrame = 0;
  const p = S.ask;
  const el = $('#qa-pending');
  if (!p || !el || +el.dataset.token !== p.token) return;
  const atEnd = askList.scrollHeight - askList.scrollTop - askList.clientHeight < 60;
  if (!el.firstChild) {
    el.innerHTML = `<div class="qa-q">${esc(p.q)}</div><div class="qa-a md"></div><div class="qa-meta"><span class="qa-status"></span></div>`;
    p.shown = null;
  }
  if (p.shown !== p.text) {
    $('.qa-a', el).innerHTML = p.text ? askHtml(p.text) : '<div class="qa-dots"><i></i><i></i><i></i></div>';
    p.shown = p.text;
  }
  let status;
  if (p.cancelling) status = '멈추는 중…';
  else if (p.waitSecs > 0) status = `요청 한도 때문에 ${p.waitSecs}초 기다렸다가 다시 물어요…`;
  else if (!p.text) status = p.restarted ? '회의 기록을 줄여서 다시 묻는 중…' : '회의 기록을 읽는 중…';
  else status = '답을 쓰는 중…';
  $('.qa-status', el).textContent = `${status} ${fmt(Date.now() - p.started)}`;
  if (atEnd) askList.scrollTop = askList.scrollHeight;
}
// 입력 칸을 글 길이에 맞게 (최대 다섯 줄쯤). 칸이 안 보이거나 아주 좁을 때 재면 높게 굳으므로 그때는 건너뛴다
function fitAsk() {
  if (askInput.offsetWidth < 80) return;
  askInput.style.height = '';
  if (askInput.scrollHeight > askInput.clientHeight) askInput.style.height = `${Math.min(askInput.scrollHeight + 2, 132)}px`;
}
async function askMeeting(question) {
  const d = doc();
  const q = String(question || '').trim();
  if (!d || !q || !canAsk(d)) return;
  if (S.ask) { toast('앞 질문의 답을 받는 중이에요'); return; }
  // 방금 고친 문장 · 메모가 들어가게 기다리던 저장을 먼저 보낸다
  flushSaves();
  const my = { id: d.id, doc: d, token: Date.now(), q, text: '', waitSecs: 0, restarted: false, started: Date.now(), cancelling: false, shown: null };
  S.ask = my;
  if (askInput.value.trim() === q) { askInput.value = ''; fitAsk(); }
  clearInterval(S.askTimer);
  S.askTimer = setInterval(renderAskPending, 1000);
  renderAsk();
  try {
    const item = await api.invoke('ask_meeting', { id: d.id, question: q, token: my.token });
    // 그 사이 앱이 기록을 다시 읽어 준 사본으로 바뀌었으면 지금 사본에
    const cur = doc();
    const target = cur && cur.id === my.id ? cur : my.doc;
    target.qa = [...(target.qa || []), item];
  } catch (e) {
    const msg = String(e);
    if (msg !== ASK_CANCELLED) toast(msg, 'error');
    // 다시 물을 수 있게 질문을 입력 칸에 돌려놓는다 (그 사이 새로 친 글이 없으면)
    if (!askInput.value.trim() && doc()?.id === my.id) { askInput.value = q; fitAsk(); }
  } finally {
    if (S.ask === my) S.ask = null;
    clearInterval(S.askTimer);
    if (askFrame) { cancelAnimationFrame(askFrame); askFrame = 0; }
    renderAsk();
  }
}
function cancelAsk() {
  const a = S.ask;
  if (!a || a.cancelling) return;
  a.cancelling = true;
  renderAskPending();
  renderAskSend();
  api.invoke('cancel_ask', { token: a.token }).catch(() => {});
}
function openAsk() {
  if (!doc() || S.mini) return;
  if (!notesShown()) { setNotesWanted(true); renderNotes(); renderSummary(); }
  showNotesTab('ask');
  if (!askInput.disabled) askInput.focus();
}
// 녹취록 시각 → 그 시각에 말하던 문장 (말 사이면 가까운 문장)
function gotoMs(ms) {
  const d = doc();
  if (!d?.utterances.length) return;
  const sorted = [...d.utterances].sort((a, b) => a.startMs - b.startMs || a.id - b.id);
  const u = anchorOf(sorted, ms);
  if (u) gotoUtterance(u.id);
}
$('#btn-ask').addEventListener('click', openAsk);
$('#ask-form').addEventListener('submit', (e) => {
  e.preventDefault();
  if (S.ask && S.ask.id === doc()?.id) cancelAsk();
  else askMeeting(askInput.value);
});
askInput.addEventListener('keydown', (e) => {
  // 한글 조합 중 Enter 는 글자를 끝내는 것 — 보내지 않는다
  if (e.key !== 'Enter' || e.shiftKey || e.isComposing || e.keyCode === 229) return;
  e.preventDefault();
  if (!S.ask) askMeeting(askInput.value);
});
askInput.addEventListener('input', () => { fitAsk(); renderAskSend(); });
// 지우는 중에는 다시 지우지 않는다 (앱은 번호가 아니라 그 질문 · 답 내용으로 찾아 지운다)
let qaDeleting = false;
async function deleteQa(d, item) {
  if (qaDeleting) return;
  qaDeleting = true;
  try {
    await api.invoke('delete_qa', { id: d.id, item });
    if (item) {
      const k = (d.qa || []).indexOf(item);
      if (k >= 0) d.qa.splice(k, 1);
    } else d.qa = [];
  } catch (err) {
    toast(String(err), 'error');
  } finally {
    qaDeleting = false;
    renderAsk();
  }
}
askList.addEventListener('click', async (e) => {
  const ts = e.target.closest('.ts');
  if (ts) { gotoMs(+ts.dataset.ms); return; }
  const sug = e.target.closest('[data-sug]');
  if (sug) { askMeeting(sug.dataset.sug); return; }
  const b = e.target.closest('[data-qa]');
  const d = doc();
  if (!b || !d) return;
  const act = b.dataset.qa;
  if (act === 'clear') {
    if (qaDeleting || !(await api.ask('이 회의에 묻고 답한 것을 모두 지울까요?', { title: '받아쓰기', kind: 'warning' }))) return;
    deleteQa(d, null);
    return;
  }
  const i = +b.closest('.qa')?.dataset.i;
  const x = d.qa?.[i];
  if (!x) return;
  if (act === 'copy') {
    await navigator.clipboard.writeText(`Q. ${x.q}\n${x.a}`);
    toast('질문과 답을 복사했어요');
  } else if (act === 'memo') {
    const cur = (d.notes || '').replace(/\s+$/, '');
    d.notes = `${cur ? `${cur}\n\n` : ''}Q. ${x.q}\n${x.a.trim()}`;
    syncMemoBox(d);
    persistDoc(d);
    scheduleRender();
    toast('메모 끝에 질문과 답을 넣었어요');
  } else if (act === 'del') {
    deleteQa(d, x);
  }
});

/* ───────────── 패널 ───────────── */
// tab: 설정을 열 때 보여 줄 분류 (없으면 마지막으로 본 분류)
function openPanel(name, tab) {
  closePanels();
  hidePopovers();
  $('#scrim').hidden = false;
  $(`#panel-${name}`).hidden = false;
  if (name === 'settings') { showSetTab(tab || S.setTab || 'record'); renderSettings(); }
  if (name === 'history') renderHistory();
}
// 설정 분류: 녹음 · 화자 · 요약 · 사용량 · 일반
const SET_TABS = ['record', 'speaker', 'summary', 'usage', 'general'];
function showSetTab(tab) {
  const changed = S.setTab !== tab;
  S.setTab = tab;
  $$('#set-tabs [data-tab]').forEach((b) => {
    const on = b.dataset.tab === tab;
    b.classList.toggle('on', on);
    b.setAttribute('aria-selected', String(on));
    b.tabIndex = on ? 0 : -1;
  });
  $$('#settings .set-page').forEach((p) => (p.hidden = p.dataset.page !== tab));
  if (changed) $('#settings').scrollTop = 0;
  if (tab === 'usage') renderUsage();
}
$('#set-tabs').addEventListener('click', (e) => {
  const b = e.target.closest('[data-tab]');
  if (b) showSetTab(b.dataset.tab);
});
$('#set-tabs').addEventListener('keydown', (e) => {
  const d = { ArrowRight: 1, ArrowLeft: -1 }[e.key];
  if (!d) return;
  e.preventDefault();
  const next = SET_TABS[(SET_TABS.indexOf(S.setTab) + d + SET_TABS.length) % SET_TABS.length];
  showSetTab(next);
  $(`#set-tabs [data-tab="${next}"]`).focus();
});
function closePanels() {
  $$('.panel').forEach((p) => (p.hidden = true));
  $('#scrim').hidden = true;
  stopHotkeyCapture();
}
$('#btn-settings').addEventListener('click', () => openPanel('settings'));
$('#btn-history').addEventListener('click', () => openPanel('history'));
$('#scrim').addEventListener('click', closePanels);
$$('[data-close]').forEach((b) => b.addEventListener('click', closePanels));
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !$('#update-modal').hidden) { closeUpdateModal(); return; }
  if (e.key === 'Escape' && !hotkeyCapturing) {
    // 아무것도 열려 있지 않으면 전체 화면을 끝낸다
    const nothingOpen = $('#findbar').hidden && $$('.panel, .popover').every((p) => p.hidden);
    if (nothingOpen && WIN.full) toggleFullscreen();
    if (!$('#findbar').hidden && $$('.panel').every((p) => p.hidden)) closeFind();
    closePanels();
    hidePopovers();
  }
  if (e.key === 'F11' && !S.mini) { e.preventDefault(); toggleFullscreen(); return; }
  // Ctrl+M: 노트 열기 · 메모로 / 메모를 쓰는 중이면 닫기
  if (e.ctrlKey && !e.altKey && !e.shiftKey && e.code === 'KeyM' && !S.mini) {
    e.preventDefault();
    if (notesShown() && document.activeElement === memo) { memo.blur(); toggleNotes(false); } else toggleNotes(true, true);
    return;
  }
  // Ctrl+; : 메모에 지금 시각
  if (e.ctrlKey && !e.altKey && e.code === 'Semicolon' && !S.mini && doc()) {
    e.preventDefault();
    if (!notesShown() || curTab() !== 'memo') toggleNotes(true, true);
    insertTime();
    return;
  }
  if (e.ctrlKey && !e.altKey && (e.key === 'f' || e.key === 'F')) {
    e.preventDefault();
    if (!S.mini && $$('.panel').every((p) => p.hidden)) openFind();
  }
  const typing = e.target.closest?.('input:not([type=range]):not([type=checkbox]), textarea, select, [contenteditable="true"]');
  if (e.code === 'Space' && !typing && !e.ctrlKey && !e.altKey && P.src && !S.mini) { e.preventDefault(); togglePlay(); }
  if (e.ctrlKey && (e.key === '=' || e.key === '+')) { e.preventDefault(); zoom(1); }
  if (e.ctrlKey && e.key === '-') { e.preventDefault(); zoom(-1); }
  if (e.ctrlKey && e.key === '0') { e.preventDefault(); zoom(0); }
});
document.addEventListener('wheel', (e) => { if (e.ctrlKey) { e.preventDefault(); zoom(e.deltaY < 0 ? 1 : -1); } }, { passive: false });
document.addEventListener('click', (e) => {
  const dl = e.target.closest('[data-dl]');
  if (dl) queueDownloads(dl.dataset.dl.split(','));
  if (e.target.closest('[data-open-settings]')) openPanel('settings', 'record');
  const url = e.target.closest('[data-url]');
  if (url) { e.preventDefault(); call('open_url', { url: url.dataset.url }); }
});

/* 기록 */
let histReq = 0;
async function renderHistory() {
  const my = ++histReq;
  const q = $('#history-q').value.trim();
  const list = q ? await call('search_sessions', { query: q }) : await call('list_sessions');
  if (my !== histReq) return; // 그 사이 검색어가 바뀜
  const box = $('#history-list');
  if (!list.length) {
    box.innerHTML = q
      ? `<div class="h-empty">'${esc(q)}' 이(가) 들어간 기록이 없어요.</div>`
      : '<div class="h-empty">아직 회의 기록이 없어요.<br>녹음을 마치면 자동으로 저장돼요.</div>';
    return;
  }
  const mark = (t) => {
    const e = esc(t);
    if (!q) return e;
    const re = new RegExp(esc(q).replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi');
    return e.replace(re, (m) => `<mark>${m}</mark>`);
  };
  let day = '';
  const liveId = S.rec !== 'idle' ? S.liveDoc?.id : null;
  // 지금 화면에 띄워 둔 회의는 목록에서 표시해 둔다
  const shownId = doc()?.id;
  box.innerHTML = list.map((m) => {
    const dd = m.startedAt.slice(0, 10);
    const head = dd !== day ? `<div class="h-day">${esc(dayLabel(dd))}</div>` : '';
    day = dd;
    return `${head}<div class="h-item${m.id === shownId ? ' on' : ''}" role="button" data-id="${esc(m.id)}">
      <div class="h-title"><span>${mark(m.title)}${m.hits ? `<span class="h-badge">${m.hits}곳</span>` : ''}</span>${m.id === liveId ? '<span class="h-live">● 녹음 중</span>' : `<button class="icon-btn sm" data-del="${esc(m.id)}" title="삭제"><svg><use href="#i-trash"/></svg></button>`}</div>
      <div class="h-sub">${esc(m.startedAt.slice(11))} · ${durLabel(m.durationMs)} · 화자 ${m.speakers}명 · 발화 ${m.count}개</div>
      ${m.snippet ? `<div class="h-snip">${mark(m.snippet)}</div>` : `<div class="h-prev">${esc(m.preview)}</div>`}
    </div>`;
  }).join('');
  // 보고 있던 회의가 목록 아래쪽이면 보이는 곳까지 내린다 (목록 ↔ 회의를 오가며 확인할 때)
  const on = !q && box.querySelector('.h-item.on');
  if (on) {
    const r = on.getBoundingClientRect(), b = box.getBoundingClientRect();
    if (r.bottom > b.bottom || r.top < b.top) box.scrollTop += r.top - b.top - box.clientHeight / 3;
  }
}
// 기록 하나를 연다 — 받아 적는 중인 회의는 최신 문장으로, 지난 회의는 처음부터 (q: 찾을 말)
async function openSession(id, q = '') {
  commitEdits();
  if (S.liveDoc && id === S.liveDoc.id) { S.live = true; S.viewDoc = null; }
  else { S.viewDoc = await call('load_session', { id }); S.live = false; }
  closePanels();
  renderDock();
  const toLatest = following() && !q;
  if (toLatest) follow = true;
  renderDoc();
  if (!toLatest) body.scrollTop = 0;
  if (q) openFind(q);
}
$('#history-list').addEventListener('click', async (e) => {
  const del = e.target.closest('[data-del]');
  if (del) {
    e.stopPropagation();
    const ok = await api.ask('이 회의 기록을 삭제할까요?\n(녹취록·요약·녹음 파일이 함께 지워지며 되돌릴 수 없어요)', { title: '받아쓰기', kind: 'warning' });
    if (ok) {
      const gone = del.dataset.del;
      await call('delete_session', { id: gone });
      if (S.viewDoc?.id === gone) { dropEdits(); S.viewDoc = null; S.live = true; }
      if (S.liveDoc?.id === gone && S.rec === 'idle') S.liveDoc = null;
      renderDock();
      renderDoc();
      renderHistory();
      if (!$('#empty').hidden) renderRecent();
    }
    return;
  }
  const item = e.target.closest('.h-item');
  if (item) openSession(item.dataset.id, $('#history-q').value.trim());
});
$('#btn-open-folder').addEventListener('click', () => call('open_folder', { kind: 'sessions' }));
$('#history-q').addEventListener('input', debounce(renderHistory, 220));
$('#btn-import').addEventListener('click', () => pickFile());
$('#btn-open-file').addEventListener('click', () => pickFile());

/* ───────────── 파일 받아쓰기 ───────────── */
const exts = () => S.app?.audioExts || ['wav', 'mp3', 'm4a', 'mp4', 'flac', 'ogg', 'opus'];
async function pickFile() {
  if (S.rec !== 'idle') return toast('녹음을 마친 뒤에 파일을 불러올 수 있어요', 'error');
  const path = await api.open({ multiple: false, directory: false, filters: [{ name: '오디오·영상', extensions: exts() }] });
  if (path) startFile(Array.isArray(path) ? path[0] : path);
}
async function startFile(path) {
  if (S.rec !== 'idle') return toast('녹음을 마친 뒤에 파일을 불러올 수 있어요', 'error');
  const ext = String(path).split('.').pop().toLowerCase();
  if (!exts().includes(ext)) return toast(`지원하지 않는 형식이에요 (${exts().join(' · ')})`, 'error');
  closePanels();
  S.live = true;
  S.viewDoc = null;
  S.fileMode = true;
  try {
    await api.invoke('transcribe_file', { path });
    toast('파일을 받아 적고 있어요. 끝나면 자동으로 저장돼요.');
  } catch (e) {
    S.fileMode = false;
    toast(String(e), 'error');
    if (/모델|API 키/.test(String(e))) openPanel('settings', 'record');
  }
  renderDock();
}
if (api.onDrop) {
  api.onDrop((e) => {
    const t = e.payload.type;
    if (t === 'enter' || t === 'over') $('#drop').hidden = S.rec !== 'idle';
    else if (t === 'drop') { $('#drop').hidden = true; if (e.payload.paths?.length) startFile(e.payload.paths[0]); }
    else $('#drop').hidden = true;
  });
}

/* ───────────── 찾기 (Ctrl+F) ───────────── */
function openFind(q) {
  const bar = $('#findbar');
  bar.hidden = false;
  const inp = $('#find-input');
  if (q != null) { inp.value = q; S.findQ = q; S.findIdx = 0; renderDoc(); }
  inp.focus();
  inp.select();
}
function closeFind() {
  $('#findbar').hidden = true;
  S.findQ = '';
  $('#find-input').value = '';
  renderDoc();
}
function updateFind(scroll) {
  const marks = $$('#transcript mark');
  const n = marks.length;
  if (S.findIdx >= n) S.findIdx = 0;
  $('#find-count').textContent = S.findQ ? (n ? `${S.findIdx + 1}/${n}` : '없음') : '';
  marks.forEach((m, i) => m.classList.toggle('cur', i === S.findIdx));
  // scrollIntoView 는 창 전체까지 밀어 올리므로 본문만 스크롤한다
  if (scroll && n) {
    const mr = marks[S.findIdx].getBoundingClientRect();
    const br = body.getBoundingClientRect();
    body.scrollTop += mr.top - br.top - body.clientHeight / 2;
  }
}
function stepFind(dir) {
  const n = $$('#transcript mark').length;
  if (!n) return;
  S.findIdx = (S.findIdx + dir + n) % n;
  updateFind(true);
}
$('#btn-find').addEventListener('click', () => ($('#findbar').hidden ? openFind() : closeFind()));
$('#find-input').addEventListener('input', debounce((e) => { S.findQ = e.target.value.trim(); S.findIdx = 0; renderDoc(); updateFind(true); }, 150));
$('#find-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') { e.preventDefault(); stepFind(e.shiftKey ? -1 : 1); }
  if (e.key === 'Escape') { e.stopPropagation(); closeFind(); }
});
$('#find-next').addEventListener('click', () => stepFind(1));
$('#find-prev').addEventListener('click', () => stepFind(-1));
$('#find-close').addEventListener('click', closeFind);

/* ───────────── 글자 크기 (Ctrl +/-/0, Ctrl+휠) ───────────── */
let fontPx = 15;
try { fontPx = +localStorage.getItem('fontPx') || 15; } catch {}
function zoom(dir) {
  fontPx = dir === 0 ? 15 : Math.max(12, Math.min(24, fontPx + dir));
  $('#transcript').style.setProperty('--fs', `${fontPx}px`);
  try { localStorage.setItem('fontPx', String(fontPx)); } catch {}
  if (dir !== 0 || fontPx !== 15) toast(`글자 크기 ${fontPx}px`);
}
$('#transcript').style.setProperty('--fs', `${fontPx}px`);

/* ───────────── 다시 듣기 · 문장 눌러 듣기 ───────────── */
const audio = new Audio();
audio.preload = 'metadata';
// key: 지금 재생할 수 있는 기록 id · until: 문장 하나만 들을 때 멈출 위치(초)
// live: 녹음 중인 기록 (파일이 계속 자라므로 필요할 때 다시 읽는다) · wantPlay: 이어 듣는 중
const P = { key: null, src: null, until: null, curId: null, started: false, raf: 0, seeking: false, followId: null, live: false, wantPlay: false, reloading: null, gen: 0 };
let playClick;

const isLiveRec = () => S.live && S.rec !== 'idle' && !S.fileMode;
function playableKey() {
  const d = doc();
  return d ? d.id : null;
}
// 자라는 녹음 파일을 다시 읽는다 (듣던 위치·재생 상태 유지)
function reloadSrc() {
  if (!P.src) return Promise.resolve();
  if (P.reloading) return P.reloading;
  const t = audio.currentTime;
  const wasPlaying = !audio.paused;
  const rate = audio.playbackRate;
  const src = P.src;
  P.reloading = new Promise((resolve) => {
    let to;
    const done = () => {
      audio.removeEventListener('loadedmetadata', done);
      clearTimeout(to);
      // 그 사이 다른 기록으로 바뀌었으면 옛 위치로 옮기지 않는다
      if (P.src !== src) { resolve(); return; }
      const dur = Number.isFinite(audio.duration) ? audio.duration : t;
      try { audio.currentTime = Math.min(t, dur); } catch { /* 아직 못 옮김 */ }
      audio.playbackRate = rate;
      if (wasPlaying) audio.play().catch(() => {});
      resolve();
    };
    to = setTimeout(done, 2000);
    audio.addEventListener('loadedmetadata', done);
    audio.src = `${P.src}?v=${Date.now()}`;
  }).finally(() => { P.reloading = null; renderPlayer(); });
  return P.reloading;
}
// 녹음 중이면, 들으려는 곳까지 파일에 담겨 있도록 다시 읽는다
async function ensureFresh(needSec) {
  if (!P.live) return;
  const dur = Number.isFinite(audio.duration) ? audio.duration : 0;
  if (needSec > dur - 0.2) await reloadSrc();
}
async function syncAudio() {
  const key = playableKey();
  const live = !!key && isLiveRec();
  if (key === P.key) {
    // 녹음이 막 끝났으면 완성된 파일로 다시 읽는다
    if (P.live !== live) { P.live = live; if (!live) reloadSrc(); }
    return;
  }
  P.key = key;
  P.live = live;
  P.wantPlay = false;
  P.gen++;
  P.src = null;
  P.until = null;
  P.started = false;
  audio.pause();
  audio.removeAttribute('src');
  renderPlayer();
  if (!key) return;
  let path = null;
  try { path = await api.invoke('session_audio', { id: key }); } catch { /* 없으면 숨김 */ }
  if (P.key !== key || !path) return;
  P.src = T ? T.core.convertFileSrc(path) : path;
  audio.src = P.src;
  audio.playbackRate = +(localStorage.getItem('playRate') || 1) || 1;
  renderPlayer();
  try {
    if (!localStorage.getItem('hintPlay')) { localStorage.setItem('hintPlay', '1'); toast('문장을 누르면 그 부분을 들을 수 있어요'); }
  } catch { /* 저장소를 못 쓰면 안내만 생략 */ }
}
function renderPlayer() {
  const on = !!P.src;
  $('#player').hidden = !on;
  $('#transcript').classList.toggle('can-play', on);
  if (!on) return;
  $('#pl-play use').setAttribute('href', audio.paused ? '#i-play' : '#i-pause');
  const dur = Number.isFinite(audio.duration) ? audio.duration : 0;
  $('#pl-time').textContent = fmt(audio.currentTime * 1000);
  $('#pl-dur').textContent = fmt(dur * 1000);
  if (!P.seeking) $('#pl-seek').value = dur ? Math.round((audio.currentTime / dur) * 1000) : 0;
  $('#pl-rate').textContent = `${audio.playbackRate}×`;
}
// 녹음 중에 다시 들으면, 재생하는 문장을 백엔드에 알려 그 소리가 다시 받아 적히면 버리게 한다
function notePlayback(playing, u) {
  if (!P.live) return;
  if (playing && u && P.notedId === u.id) return;
  if (playing && u) P.notedId = u.id;
  if (!playing) P.notedId = null;
  api.invoke('set_playback', { playing, texts: playing && u ? [u.text] : [] }).catch(() => {});
}
function markPlaying(follow) {
  $$('#transcript .u.playing').forEach((el) => el.classList.remove('playing'));
  const d = doc();
  if (!P.src || !P.started || !d) return;
  const t = audio.currentTime * 1000;
  const u = d.utterances.find((x) => t >= x.startMs - 200 && t <= x.endMs + 300);
  const el = u && $(`#transcript .u[data-id="${u.id}"]`);
  if (!el) return;
  el.classList.add('playing');
  if (!audio.paused) notePlayback(true, u);
  // 이어 듣는 중이면 지금 문장이 보이게 따라간다
  if (follow && P.followId !== u.id) {
    P.followId = u.id;
    const r = el.getBoundingClientRect();
    const b = body.getBoundingClientRect();
    if (r.top < b.top + 30 || r.bottom > b.bottom - 30) body.scrollTop += r.top - b.top - body.clientHeight / 3;
  }
}
function checkUntil() {
  if (P.until != null && audio.currentTime >= P.until) { audio.pause(); P.until = null; }
}
function tick() {
  checkUntil();
  renderPlayer();
  markPlaying(P.until == null);
  if (!audio.paused) P.raf = requestAnimationFrame(tick);
}
async function togglePlay() {
  if (!P.src) return;
  P.gen++;
  if (audio.paused) {
    P.until = null;
    P.wantPlay = true;
    await ensureFresh(audio.currentTime + 2);
    audio.play().catch((e) => toast(`재생할 수 없어요: ${e.message || e}`, 'error'));
  } else {
    P.wantPlay = false;
    audio.pause();
  }
}
async function playUtterance(id) {
  const u = doc()?.utterances.find((x) => x.id === id);
  if (!u || !P.src) return;
  // 듣고 있는 문장을 다시 누르면 멈춤
  if (!audio.paused && P.until != null && P.curId === id) { audio.pause(); return; }
  P.curId = id;
  P.followId = id;
  P.wantPlay = false;
  P.gen++;
  await ensureFresh(u.endMs / 1000 + 0.3);
  notePlayback(true, u);
  audio.currentTime = Math.max(0, u.startMs / 1000 - 0.15);
  P.until = u.endMs / 1000 + 0.2;
  audio.play().catch((e) => toast(`재생할 수 없어요: ${e.message || e}`, 'error'));
}
audio.addEventListener('play', () => { P.started = true; cancelAnimationFrame(P.raf); P.raf = requestAnimationFrame(tick); });
audio.addEventListener('pause', () => { renderPlayer(); markPlaying(false); notePlayback(false); });
audio.addEventListener('timeupdate', checkUntil); // 창이 숨겨져 rAF 가 멈춰도 문장 끝에서 멈추게
audio.addEventListener('loadedmetadata', renderPlayer);
audio.addEventListener('ended', async () => {
  P.until = null;
  renderPlayer();
  // 녹음 중에 이어 듣다 끝(지금 시각)에 닿으면, 새로 녹음된 만큼 다시 읽어 계속 따라간다
  const gen = ++P.gen; // 사용자가 재생·멈춤·문장 듣기를 하면 이 따라가기는 그만둔다
  while (P.live && P.wantPlay && P.src && gen === P.gen) {
    const t = audio.currentTime;
    await new Promise((r) => setTimeout(r, 700));
    if (!P.live || !P.wantPlay || gen !== P.gen) break;
    await reloadSrc();
    if ((Number.isFinite(audio.duration) ? audio.duration : 0) > t + 0.3) {
      audio.currentTime = t;
      audio.play().catch(() => {});
      break;
    }
  }
});
// 녹음 중 멈춰 있을 때도 전체 길이·재생 막대가 늘어나게
setInterval(() => {
  if (P.live && P.src && audio.paused && !P.reloading && !P.seeking && !document.hidden) reloadSrc();
}, 4000);
audio.addEventListener('error', () => {
  if (!P.src) return;
  P.src = null;
  renderPlayer();
  toast('녹음 파일을 열 수 없어요 (옮겼거나 지웠을 수 있어요)', 'error');
});
$('#pl-play').addEventListener('click', togglePlay);
$('#pl-seek').addEventListener('input', (e) => {
  P.seeking = true;
  P.until = null;
  P.wantPlay = !audio.paused;
  P.started = true;
  const dur = Number.isFinite(audio.duration) ? audio.duration : 0;
  audio.currentTime = (+e.target.value / 1000) * dur;
  renderPlayer();
  markPlaying(true);
});
$('#pl-seek').addEventListener('change', () => { P.seeking = false; });
const RATES = [1, 1.25, 1.5, 2, 0.75];
$('#pl-rate').addEventListener('click', () => {
  const i = RATES.indexOf(audio.playbackRate);
  audio.playbackRate = RATES[(i + 1) % RATES.length];
  try { localStorage.setItem('playRate', String(audio.playbackRate)); } catch { /* 무시 */ }
  renderPlayer();
});
// 문장 한 번 누르기 → 그 부분 듣기 (더블클릭 편집·글자 선택과 겹치지 않게 잠깐 기다린다)
$('#transcript').addEventListener('click', (e) => {
  const el = e.target.closest('.u[data-id]');
  if (!el || !P.src || editing || e.target.closest('.g-name') || ghostHit(el, e)) return;
  if (!window.getSelection().isCollapsed) return;
  clearTimeout(playClick);
  const id = +el.dataset.id;
  playClick = setTimeout(() => playUtterance(id), 230);
});

/* ───────────── 미니 창 ───────────── */
// 미니 창에서는 도크 빈 곳을 잡고 끌어 옮긴다 (큰 창에서는 더블클릭 최대화를 막으려고 끈다)
const DRAG_TARGETS = ['.dock', '.dock-mid', '.dock-row', '#timer', '#meter', '#mini-cap', '#paused-tag', '#fprog'];
function setMini(on) {
  S.mini = on;
  $('#app').classList.toggle('mini', on);
  for (const sel of DRAG_TARGETS) {
    const el = $(sel);
    if (el) on ? el.setAttribute('data-tauri-drag-region', '') : el.removeAttribute('data-tauri-drag-region');
  }
  if (on) { closePanels(); hidePopovers(); $('#findbar').hidden = true; S.findQ = ''; }
  renderDock();
  scheduleRender();
}
function renderMiniCap() {
  const cap = $('#mini-cap');
  if (!S.mini || !cap) return;
  const d = S.live ? S.liveDoc : null;
  const last = d?.utterances?.[d.utterances.length - 1];
  if (S.rec === 'recording' && S.paused) cap.textContent = S.sleepPaused ? 'PC가 잠들어서 일시정지했어요 — ▶ 로 이어서' : '일시정지 중 — 이 동안은 받아 적지 않아요';
  else if (last && S.rec !== 'idle') cap.innerHTML = `<b>${esc(speakerName(d, resolveSpk(d, last.speaker)))}</b>${esc(last.text)}`;
  else if (S.rec === 'starting') cap.textContent = '준비하는 중…';
  else if (S.rec === 'stopping') cap.textContent = '남은 발화를 정리하는 중…';
  else if (S.rec === 'recording') cap.textContent = S.speaking ? '듣는 중…' : '말씀하시면 받아 적어요';
  else cap.textContent = `${S.settings?.hotkey || ''} 로 녹음 · 더블클릭하면 창 위치 복구`;
}
$('#btn-mini').addEventListener('click', () => call('set_mini', { mini: true }));
$('#btn-expand').addEventListener('click', () => call('set_mini', { mini: false }));
$('#btn-mini-hide').addEventListener('click', () => api.invoke('hide_window'));
$('#btn-mini-min').addEventListener('click', () => api.win.minimize());
// 더블클릭으로 창 모드 바꾸기 (끌기와 겹치지 않게 mousedown 의 두 번째 클릭으로 판단)
// - 미니 창: 도크 빈 곳 → 원래 창 위치·크기로 복구
// - 큰 창: 제목 줄 · 도크 빈 곳 → 미니 창
// Tauri 끌기 영역의 기본 동작(더블클릭 = 최대화)이 이어서 돌지 않도록 여기서 멈춘다.
const blankArea = (t) => !t.closest('button, input, select, textarea, a, .popover');
$('.dock').addEventListener('mousedown', (e) => {
  if (e.button !== 0 || e.detail !== 2 || !blankArea(e.target)) return;
  e.preventDefault();
  e.stopPropagation();
  call('set_mini', { mini: !S.mini });
});
$('.titlebar').addEventListener('mousedown', (e) => {
  if (S.mini || e.button !== 0 || e.detail !== 2 || !blankArea(e.target)) return;
  e.preventDefault();
  e.stopPropagation();
  call('set_mini', { mini: true });
});

/* ───────────── 일시정지 ───────────── */
$('#btn-pause').addEventListener('click', () => call('set_paused', { paused: !S.paused }));
function onPaused(p) {
  S.paused = p;
  if (p) { S.speaking = false; resetLevel(); } else S.sleepPaused = false;
  renderDock();
  scheduleRender();
  toast(p ? '일시정지했어요. 이 동안의 소리는 받아 적지 않아요.' : '이어서 받아 적어요');
}

// 녹음 중 PC 가 잠들었다 깨어남 → 앱이 일시정지해 둠 (덮개 닫고 가방에 넣었다 열어도 저절로 이어 녹음되지 않게)
function onSleep(secs) {
  S.sleepPaused = true;
  const long = secs ? ` (${secs >= 90 ? `${Math.round(secs / 60)}분` : `${secs}초`} 동안 잠들어 있었어요)` : '';
  toast(`PC가 절전에 들어가서 녹음을 일시정지했어요${long}.\n이어서 받아 적으려면 ▶ 를 누르세요.`, 'warn');
  renderDock();
  renderMiniCap();
}

/* ───────────── 업데이트 ───────────── */
function renderUpdate() {
  const u = S.update;
  const bar = $('#update-bar');
  if (!u || S.updateDismissed && u.state === 'ready' || u.state === 'error') { bar.hidden = true; return; }
  bar.hidden = false;
  const btn = $('#btn-update-now');
  if (u.state === 'downloading') {
    const pct = u.total ? Math.round((u.done / u.total) * 100) : 0;
    $('#update-label').textContent = `새 버전 v${u.version} 내려받는 중… ${pct ? `${pct}%` : ''}`;
    btn.hidden = true;
  } else if (u.state === 'ready') {
    // 릴리스 노트에서 제목 줄은 건너뛰고 첫 항목의 이름만 ("- 🎧 **다시 듣기** — …" → "🎧 다시 듣기")
    const first = (u.notes || '').split('\n').map((l) => l.trim()).find((l) => /^[-*]\s/.test(l)) || '';
    const what = first.replace(/^[-*]\s+/, '').replace(/\*\*/g, '').split(' — ')[0].trim();
    $('#update-label').textContent = `새 버전 v${u.version} 준비됨${what ? ` · ${what}` : ''}`;
    btn.hidden = false;
    btn.textContent = '자세히 보기';
  } else if (u.state === 'installing') {
    $('#update-label').textContent = `v${u.version} 설치 중… 잠시 뒤 다시 열려요`;
    btn.hidden = true;
  } else {
    bar.hidden = true;
  }
}
function onUpdate(u) {
  S.update = u;
  if (!$('#update-modal').hidden) {
    // 팝업이 열린 채 상태가 바뀌면: 준비 끝 → 적용 버튼 살림, 실패 · 사라짐 → 닫기 (오류는 아래 토스트로)
    if (u?.state === 'ready') { $('#um-apply').disabled = false; $('#um-apply').textContent = '지금 적용'; }
    else if (!u || u.state === 'error') closeUpdateModal();
  }
  if (u?.state === 'error') toast(u.message || '업데이트 확인 실패', 'error');
  renderUpdate();
  renderAbout();
}
// 업데이트 팝업: 지금 버전 이후 바뀐 내용을 버전별로 보여 주고, 확인한 뒤 적용
async function openUpdateModal() {
  const u = S.update;
  if (!u || !['ready', 'downloading', 'available'].includes(u.state)) return;
  const m = $('#update-modal');
  $('#um-title').textContent = `새 버전 v${u.version}`;
  $('#um-sub').textContent = `지금 v${S.app?.version || ''} → v${u.version}`;
  $('#um-body').innerHTML = '<div class="md"><div class="loading"><span class="dot"></span>바뀐 내용을 불러오는 중…</div></div>';
  const apply = $('#um-apply');
  apply.disabled = u.state !== 'ready';
  apply.textContent = u.state === 'ready' ? '지금 적용' : '내려받는 중…';
  m.hidden = false;
  let list = [];
  try { list = await api.invoke('update_notes'); } catch { /* 아래에서 대신 */ }
  if (!list?.length && u.notes) list = [{ version: u.version, date: '', notes: u.notes }];
  if (m.hidden) return;
  // 각 노트 첫 줄의 "## 받아쓰기 vX" 제목은 버전 머리글과 겹치므로 뺀다
  const body = (n) => md(String(n || '').replace(/^\s*#{1,3}\s*받아쓰기[^\n]*\n/, ''));
  $('#um-body').innerHTML = list.length
    ? list.map((v) => `<section class="um-ver"><div class="um-vh"><b>v${esc(v.version)}</b>${v.date ? `<small>${esc(v.date)}</small>` : ''}</div><div class="md">${body(v.notes)}</div></section>`).join('')
    : '<p class="um-empty">바뀐 내용 정보를 불러오지 못했어요. 적용하면 최신 버전으로 바뀌어요.</p>';
}
function closeUpdateModal() { $('#update-modal').hidden = true; }
$('#btn-update-now').addEventListener('click', openUpdateModal);
$('#update-label').addEventListener('click', openUpdateModal);
$$('[data-um-close]').forEach((b) => b.addEventListener('click', closeUpdateModal));
$('#update-modal').addEventListener('mousedown', (e) => { if (e.target.id === 'update-modal') closeUpdateModal(); });
$('#um-apply').addEventListener('click', async () => {
  if (S.rec !== 'idle') return toast('녹음을 마친 뒤에 적용할 수 있어요', 'error');
  const b = $('#um-apply');
  b.disabled = true;
  b.textContent = '적용하는 중…';
  try { await api.invoke('install_update'); } catch (e) { toast(String(e), 'error'); b.disabled = false; b.textContent = '지금 적용'; }
});
$('#btn-update-later').addEventListener('click', () => { S.updateDismissed = true; renderUpdate(); });
$('#btn-check-update').addEventListener('click', async (e) => {
  const b = e.currentTarget;
  b.disabled = true;
  b.textContent = '확인 중…';
  try {
    const v = await api.invoke('check_update');
    if (v) { S.updateDismissed = false; toast(`새 버전 v${v}를 받았어요. 위의 '지금 적용'을 누르세요.`); closePanels(); }
    else toast(`최신 버전이에요 (v${S.app.version})`);
  } catch (err) { toast(String(err), 'error'); }
  b.disabled = false;
  b.textContent = '업데이트 확인';
});

/* 설정 */
// [입력 칸 id, 저장 이름, get_state keys 필드]
const KEY_INPUTS = [
  ['key-openai', 'openai', 'openai'], ['key-openai-llm', 'openai', 'openai'],
  ['key-groq', 'groq', 'groq'], ['key-groq-llm', 'groq', 'groq'],
  ['key-custom', 'custom', 'custom'], ['key-anthropic', 'anthropic', 'anthropic'],
  ['key-gemini', 'gemini', 'gemini'], ['key-llm-custom', 'llm-custom', 'llmCustom'],
];
const saveSettings = debounce(async () => {
  try {
    await api.invoke('save_settings', { settings: S.settings });
    applyTheme();
    renderDock();
    renderSetup();
  } catch (e) {
    toast(String(e), 'error');
  }
}, 250);

function setSeg(id, v) { $$(`#${id} button`).forEach((b) => b.classList.toggle('on', b.dataset.v === v)); }

function renderSettings() {
  const s = S.settings;
  if (!s) return;
  // 지금 입력하고 있는 칸은 건드리지 않는다 (아직 확정 안 된 글자가 지워지지 않게)
  const act = document.activeElement;
  const typing = act && /^(INPUT|TEXTAREA)$/.test(act.tagName) && act.type !== 'checkbox' ? act.value : null;
  queueMicrotask(() => { if (typing != null && document.activeElement === act) act.value = typing; });
  setSeg('seg-engine', s.engine);
  $$('[data-engine]').forEach((el) => (el.hidden = el.dataset.engine !== s.engine));
  $('#engine-desc').innerHTML = ENGINE_DESC[s.engine];
  $('#opt-beam').checked = s.beamSize >= 2;
  $('#opt-usd-krw').value = usdKrw();
  $('#opt-openai-model').value = s.openaiModel;
  $('#opt-groq-model').value = s.groqModel;
  $('#opt-custom-url').value = s.customUrl;
  $('#opt-custom-model').value = s.customModel;
  for (const [k, id] of KEY_INPUTS.map(([id, , k]) => [k, id])) {
    const el = $(`#${id}`);
    el.value = '';
    // 원래 안내 문구는 처음 한 번만 기억한다 (키가 저장된 채로 열면 "저장됨" 을 기억해 버려 지운 뒤에도 남았다)
    if (!('ph' in el.dataset)) el.dataset.ph = el.placeholder;
    el.placeholder = S.app.keys[k] ? '●●●●●●●● (저장됨 · 바꾸려면 입력)' : el.dataset.ph;
    // 저장된 키 지우기 — 칸을 비워 두는 것만으로는 지워지지 않는다 (Claude 는 키를 지워야 ant 로그인으로 넘어간다)
    const label = el.closest('.field').querySelector(':scope > span');
    const clr = label.querySelector('[data-clear-key]');
    if (S.app.keys[k] && !clr) label.insertAdjacentHTML('beforeend', `<button class="link" data-clear-key="${id}" title="저장된 키 지우기">지우기</button>`);
    if (!S.app.keys[k] && clr) clr.remove();
  }
  renderModels();

  $('#opt-diar').checked = s.diarization.enabled;
  // 화자 구분을 끄면 민감도 · 최대 화자 수는 쓰이지 않는다
  $('#diar-tune').classList.toggle('off', !s.diarization.enabled);
  $('#opt-thr').value = s.diarization.threshold;
  $('#thr-label').textContent = s.diarization.threshold.toFixed(2);
  $('#opt-maxspk').value = String(s.diarization.maxSpeakers);
  $('#opt-voices').checked = s.rememberVoices !== false;
  renderVoices();

  $('#opt-lang').value = s.language;
  $('#opt-glossary').value = s.glossary;
  api.invoke('list_devices').then((devs) => {
    const sel = $('#opt-mic');
    sel.innerHTML = '<option value="">기본 마이크</option>' + devs.map((d) => `<option value="${esc(d.id)}">${esc(d.name)}${d.isDefault ? ' (기본)' : ''}</option>`).join('');
    sel.value = s.micDevice || '';
  }).catch(() => {});

  setSeg('seg-llm', s.summary.provider);
  $$('[data-llm]').forEach((el) => (el.hidden = el.dataset.llm !== s.summary.provider));
  $('#llm-desc').innerHTML = (LLM_DESC[s.summary.provider] || '') + (['ollama', 'custom'].includes(s.summary.provider) ? '' : LLM_PRIVACY);
  $('#opt-ollama-url').value = s.summary.ollamaUrl;
  $('#opt-ollama-model').value = s.summary.ollamaModel;
  $('#opt-llm-groq-model').value = s.summary.groqModel;
  $('#opt-llm-gemini-model').value = s.summary.geminiModel;
  $('#opt-llm-openai-model').value = s.summary.openaiModel;
  $('#opt-anthropic-model').value = s.summary.anthropicModel;
  $('#opt-llm-custom-url').value = s.summary.customUrl;
  $('#opt-llm-custom-model').value = s.summary.customModel;

  renderHotkey();
  $('#opt-top').checked = s.alwaysOnTop;
  $('#opt-autostart').checked = S.app.autostart;
  $('#opt-audio').checked = s.saveAudio;
  $('#opt-unload').checked = s.unloadWhenIdle;
  $('#opt-autoupdate').checked = s.autoUpdate;
  setSeg('seg-theme', s.theme || 'system');
  renderAbout();
}
// 앱 정보 카드: 버전 + 업데이트 상태
function renderAbout() {
  if (!S.app) return;
  $('#app-version').textContent = `v${S.app.version}`;
  const u = S.update;
  const el = $('#about-update');
  el.classList.toggle('ready', u?.state === 'ready');
  el.textContent = u?.state === 'ready' ? `새 버전 v${u.version} 준비됨 · 쓰지 않을 때 적용돼요`
    : u?.state === 'downloading' ? `새 버전 v${u.version} 내려받는 중…`
      : S.settings?.autoUpdate ? '자동 업데이트 켜짐' : '자동 업데이트 꺼짐';
}

/* 기억한 목소리 */
async function renderVoices() {
  // 기억하기를 꺼도 이미 기억한 목소리는 보여 준다 (지울 수 있게)
  const box = $('#voice-list');
  let list = [];
  try { list = await api.invoke('list_voices'); } catch { /* 비움 */ }
  box.innerHTML = list.length
    ? list.map((v) => `<span class="voice-chip" title="회의 ${v.meetings}번 · ${minutes(v.secs)}">${esc(v.name)} <small>${v.meetings}회</small><button data-forget="${esc(v.name)}" title="이 목소리 잊기" aria-label="${esc(v.name)} 잊기">×</button></span>`).join('')
      + (list.length > 1 ? '<button class="link danger" data-forget-all>모두 잊기</button>' : '')
    : '<span class="voice-empty">아직 기억한 목소리가 없어요. 회의에서 화자 이름을 바꾸면 여기에 생겨요.</span>';
}
$('#voice-list').addEventListener('click', async (e) => {
  const one = e.target.closest('[data-forget]');
  if (one) {
    await call('forget_voice', { name: one.dataset.forget });
    toast(`'${one.dataset.forget}' 목소리를 잊었어요`);
    return renderVoices();
  }
  if (e.target.closest('[data-forget-all]')) {
    const ok = await api.ask('기억한 목소리를 모두 지울까요?\n(회의 기록은 그대로예요)', { title: '받아쓰기', kind: 'warning' });
    if (ok) { await call('forget_all_voices'); renderVoices(); }
  }
});

/* API 사용량 */
const PROVIDER = {
  groq: { name: 'Groq', url: 'https://console.groq.com/docs/rate-limits', link: '한도 안내 ↗' },
  openai: { name: 'OpenAI', url: 'https://platform.openai.com/usage', link: '사용량 · 잔액 ↗' },
  anthropic: { name: 'Claude', url: 'https://console.anthropic.com/settings/usage', link: '사용량 · 잔액 ↗' },
  gemini: { name: 'Gemini', url: 'https://aistudio.google.com/apikey', link: '키 · 사용량 ↗' },
  custom: { name: '사용자 지정 서버', url: '', link: '' },
};
const LIMIT_LABEL = { requests: '요청', tokens: '토큰', 'input-tokens': '입력 토큰', 'output-tokens': '출력 토큰', 'audio-seconds': '오디오(초)' };
const n0 = (v) => Number(v || 0).toLocaleString('ko-KR');
function minutes(secs) { return secs >= 60 ? `${(secs / 60).toFixed(1)}분` : `${Math.round(secs)}초`; }
// 금액 (USD → 원). 원화는 설정 › 사용량의 환율로
const usdKrw = () => (S.settings?.usdKrw > 0 ? S.settings.usdKrw : 1360);
function krwText(usd) {
  const w = usd * usdKrw();
  if (w <= 0) return '0원';
  if (w < 1) return '1원 미만';
  return `약 ${Math.round(w).toLocaleString('ko-KR')}원`;
}
const usdText = (usd) => (usd <= 0 ? '$0' : usd < 0.01 ? '$0.01 미만' : `$${usd.toFixed(2)}`);
const costUsd = (c) => (c ? (c.sttUsd || 0) + (c.sumUsd || 0) : 0);
// 한 줄씩: 금액 · 음성 인식 · 요약. 음성 인식과 요약을 다 썼으면 줄마다 금액도 붙인다
function tallyLines(t, c) {
  const out = [];
  const both = t?.sttRequests && t?.sumRequests;
  const part = (usd) => (both && c ? ` <small class="nowrap">· ${esc(krwText(usd))}</small>` : '');
  if (t?.sttRequests || t?.sumRequests) {
    const usd = costUsd(c);
    if (usd > 0) out.push(`<span class="usage-amt"><b>${esc(krwText(usd))}</b><small>${esc(usdText(usd))}</small></span>`);
    else if (c?.unpriced?.length) out.push('<span class="usage-amt"><b>금액 모름</b></span>');
  }
  if (t?.sttRequests) out.push(esc(`음성 인식 ${minutes(t.sttSecs)} · ${n0(t.sttRequests)}회${t.sttFailures ? ` (실패 ${n0(t.sttFailures)})` : ''}`) + part(c?.sttUsd || 0));
  if (t?.sumRequests) out.push(esc(`요약 · 묻기 ${n0(t.sumRequests)}회 · 토큰 입력 ${n0(t.sumInTokens)} · 출력 ${n0(t.sumOutTokens)}`) + part(c?.sumUsd || 0));
  return out.length ? out : ['없음'];
}
// 카드 아래 덧붙임 — 이번 달 기준 (오늘은 이번 달에 들어 있다)
function costNotes(id, t, c) {
  const out = [];
  // Groq 는 짧은 요청도 10초로 매겨, 요금이 매겨진 길이가 실제보다 꽤 길 수 있다
  const billed = c?.sttBilledSecs || 0;
  if (id === 'groq' && billed > (t?.sttSecs || 0) * 1.05 + 1) {
    out.push(`Groq 는 10초보다 짧은 음성 인식 요청도 10초로 매겨, 이번 달은 ${minutes(billed)}어치로 셈했어요.`);
  }
  if (c?.estimated) out.push('모델을 기록하기 전(v0.4.6까지) 사용분은 지금 설정한 모델 단가로 셈했어요.');
  if (c?.unpriced?.length) out.push(`단가를 몰라 금액에서 뺐어요: ${c.unpriced.join(', ')}`);
  return out;
}
// 재설정 시각(ms). Groq · OpenAI 는 받은 때부터 "1h9m7.2s" · "6ms" 같은 길이, Claude 는 "2026-11-16T08:30:00Z",
// retry-after 는 초 또는 HTTP 날짜. 알아볼 수 없으면 null
function resetAt(v, at) {
  const s = String(v).trim();
  if (/^\d+(\.\d+)?$/.test(s)) return (at + Number(s)) * 1000;
  let secs = 0, n = 0;
  for (const m of s.matchAll(/(\d+(?:\.\d+)?)(ms|h|m|s)/g)) {
    secs += Number(m[1]) * { ms: 0.001, h: 3600, m: 60, s: 1 }[m[2]];
    n++;
  }
  if (n) return (at + secs) * 1000;
  const t = Date.parse(s);
  return Number.isNaN(t) ? null : t;
}
function untilText(ms) {
  const s = (ms - Date.now()) / 1000;
  if (s < 60) return `${Math.max(1, Math.ceil(s))}초`;
  if (s < 3600) return `${Math.ceil(s / 60)}분`;
  const h = Math.floor(s / 3600), m = Math.round((s % 3600) / 60);
  return m ? `${h}시간 ${m}분` : `${h}시간`;
}
// 응답 헤더 → { lines: ["요청 1,999 / 2,000 남음 · 3분 뒤 다시 채워짐", "토큰 …"], when: "3분 전" }
// 받은 지 오래돼 재설정 시각이 지났으면, 남은 양은 이미 다시 채워졌다고 알려 준다. 한도 정보가 없으면 null
function limitsInfo(rate) {
  if (!rate) return null;
  const g = {};
  let retry = '';
  for (const [k, v] of rate.items) {
    let m = k.match(/^x-ratelimit-(limit|remaining|reset)-(.+)$/);
    if (!m) { const a = k.match(/^anthropic-ratelimit-(.+)-(limit|remaining|reset)$/); if (a) m = [k, a[2], a[1]]; }
    if (m) (g[m[2]] = g[m[2]] || {})[m[1]] = v;
    else if (k === 'retry-after') retry = v;
  }
  const resetNote = (v) => {
    const t = resetAt(v, rate.at);
    if (t == null) return ` · 재설정 ${esc(v)}`;
    return t <= Date.now() ? ' · 지금은 다시 채워졌어요' : ` · ${untilText(t)} 뒤 다시 채워짐`;
  };
  const parts = Object.entries(g).filter(([, x]) => x.remaining != null).map(([name, x]) =>
    `${esc(LIMIT_LABEL[name] || name)} ${n0(x.remaining)}${x.limit ? ` / ${n0(x.limit)}` : ''} 남음${x.reset ? resetNote(x.reset) : ''}`);
  const retryAt = retry ? resetAt(retry, rate.at) : null;
  if (retryAt != null && retryAt > Date.now()) parts.push(`한도 초과 — ${untilText(retryAt)} 뒤 다시`);
  if (!parts.length) return null;
  const ago = Math.max(0, Math.round(Date.now() / 1000 - rate.at));
  const when = ago < 60 ? '방금' : ago < 3600 ? `${Math.round(ago / 60)}분 전` : `${Math.round(ago / 3600)}시간 전`;
  return { lines: parts, when };
}
async function renderUsage() {
  const box = $('#usage-box');
  if (!box) return;
  let r;
  try { r = await api.invoke('get_usage'); } catch { r = null; }
  if (!r) { box.innerHTML = '<div class="usage-empty">사용량을 불러오지 못했어요</div>'; return; }
  const ids = [...new Set([...Object.keys(r.month || {}), ...Object.keys(r.today || {}), ...(r.rates || []).map((x) => x.provider)])];
  if (!ids.length) {
    box.innerHTML = '<div class="usage-empty">아직 API 를 쓴 기록이 없어요. 로컬 엔진과 Ollama 는 사용량이 없어요.</div>';
    return;
  }
  // 항목마다 한 줄씩 (금액 · 음성 인식 · 요약 · 한도 종류)
  const lines = (arr) => arr.map((x) => `<div>${x}</div>`).join('');
  const sum = (costs) => Object.values(costs || {}).reduce((a, c) => a + costUsd(c), 0);
  const [monthUsd, todayUsd] = [sum(r.monthCost), sum(r.todayCost)];
  const total = monthUsd > 0
    ? `<div class="usage-total">
        <div><span>이번 달 합계</span><b>${esc(krwText(monthUsd))}</b><small>${esc(usdText(monthUsd))}</small></div>
        <div><span>오늘</span><b>${esc(krwText(todayUsd))}</b><small>${esc(usdText(todayUsd))}</small></div>
      </div>`
    : '';
  box.innerHTML = total + ids.map((id) => {
    const p = PROVIDER[id] || { name: id, url: '', link: '' };
    const rate = (r.rates || []).find((x) => x.provider === id);
    const lim = limitsInfo(rate);
    const notes = costNotes(id, r.month?.[id], r.monthCost?.[id]);
    return `<div class="usage-card">
      <div class="usage-head"><b>${esc(p.name)}</b>${p.url ? `<button class="link" data-url="${p.url}">${esc(p.link)}</button>` : ''}</div>
      <div class="usage-row"><span>오늘</span><span>${lines(tallyLines(r.today?.[id], r.todayCost?.[id]))}</span></div>
      <div class="usage-row"><span>이번 달</span><span>${lines(tallyLines(r.month?.[id], r.monthCost?.[id]))}</span></div>
      ${lim ? `<div class="usage-row"><span>남은 한도<small>${lim.when} 기준</small></span><span>${lines(lim.lines)}</span></div>` : ''}
      ${notes.length ? `<div class="usage-note">${notes.map(esc).join('<br>')}</div>` : ''}
    </div>`;
  }).join('');
}

function renderModels() {
  const box = $('#model-list');
  if (!box || !S.app) return;
  const s = S.settings;
  const rows = S.app.models.map((m) => {
    const dl = S.downloads[m.id];
    const act = dl
      ? `<button class="link" data-cancel>취소</button>`
      : m.ready
        ? `<span class="badge ok">설치됨</span><button class="icon-btn sm" data-delmodel="${m.id}" title="삭제"><svg><use href="#i-trash"/></svg></button>`
        : `<button class="btn sm" data-dl="${m.id}">받기 · ${mb(m.size)}</button>`;
    return `<div class="model ${s.localModel === m.id ? 'on' : ''}" data-model="${m.id}">
      <span class="radio"></span>
      <div><b>${esc(m.label)}</b><small>${esc(m.note)}</small></div>
      <div class="m-act">${act}</div>
      ${dl ? `<div class="progress"><i style="width:${(dl.done / Math.max(dl.total, 1)) * 100}%"></i></div>` : ''}
    </div>`;
  });
  for (const f of S.app.customModels) {
    rows.push(`<div class="model ${s.localModel === f ? 'on' : ''}" data-model="${esc(f)}"><span class="radio"></span><div><b>${esc(f)}</b><small>모델 폴더에 직접 넣은 모델</small></div><div class="m-act"><span class="badge ok">설치됨</span></div></div>`);
  }
  box.innerHTML = rows.join('');

  const sm = $('#speaker-model');
  const sdl = S.downloads['campplus-zh-en'];
  sm.innerHTML = `<span>화자 인식 모델 <small style="color:var(--muted)">3D-Speaker CAM++ · 28MB</small></span>${
    S.app.speakerModelReady ? '<span class="badge ok">설치됨</span>'
      : sdl ? `<span class="badge">${Math.round((sdl.done / Math.max(sdl.total, 1)) * 100)}%</span>`
        : '<button class="btn sm" data-dl="campplus-zh-en">받기</button>'}`;
}

$('#model-list').addEventListener('click', async (e) => {
  if (e.target.closest('[data-dl]')) return;
  if (e.target.closest('[data-cancel]')) { S.dlQueue = []; return api.invoke('cancel_download'); }
  const del = e.target.closest('[data-delmodel]');
  if (del) {
    const ok = await api.ask('이 모델 파일을 삭제할까요? 다시 쓰려면 새로 내려받아야 해요.', { title: '받아쓰기', kind: 'warning' });
    if (ok) { await call('delete_model', { id: del.dataset.delmodel }); await refreshApp(); renderModels(); renderSetup(); }
    return;
  }
  const row = e.target.closest('[data-model]');
  if (row) { S.settings.localModel = row.dataset.model; saveSettings(); renderModels(); }
});

function bindSeg(id, apply) {
  $(`#${id}`).addEventListener('click', (e) => {
    const b = e.target.closest('button[data-v]');
    if (!b) return;
    apply(b.dataset.v);
    saveSettings();
    renderSettings();
  });
}
bindSeg('seg-engine', (v) => (S.settings.engine = v));
bindSeg('seg-llm', (v) => { S.settings.summary.provider = v; $('#llm-models').innerHTML = ''; });

function bind(id, fn, ev = 'change') { $(`#${id}`).addEventListener(ev, (e) => { fn(e.target); saveSettings(); }); }
bind('opt-beam', (t) => (S.settings.beamSize = t.checked ? 3 : 1));
bind('opt-openai-model', (t) => (S.settings.openaiModel = t.value));
bind('opt-groq-model', (t) => (S.settings.groqModel = t.value));
bind('opt-custom-url', (t) => (S.settings.customUrl = t.value.trim()));
bind('opt-custom-model', (t) => (S.settings.customModel = t.value.trim()));
bind('opt-diar', (t) => { S.settings.diarization.enabled = t.checked; $('#diar-tune').classList.toggle('off', !t.checked); });
bind('opt-thr', (t) => { S.settings.diarization.threshold = +t.value; $('#thr-label').textContent = (+t.value).toFixed(2); }, 'input');
bind('opt-maxspk', (t) => (S.settings.diarization.maxSpeakers = +t.value));
bind('opt-voices', (t) => { S.settings.rememberVoices = t.checked; renderVoices(); });
bind('opt-mic', (t) => (S.settings.micDevice = t.value || null));
bind('opt-lang', (t) => (S.settings.language = t.value));
bind('opt-glossary', (t) => (S.settings.glossary = t.value));
bind('opt-ollama-url', (t) => (S.settings.summary.ollamaUrl = t.value.trim()));
bind('opt-ollama-model', (t) => (S.settings.summary.ollamaModel = t.value.trim()));
bind('opt-llm-groq-model', (t) => (S.settings.summary.groqModel = t.value.trim()));
bind('opt-llm-gemini-model', (t) => (S.settings.summary.geminiModel = t.value.trim()));
bind('opt-llm-openai-model', (t) => (S.settings.summary.openaiModel = t.value.trim()));
bind('opt-anthropic-model', (t) => (S.settings.summary.anthropicModel = t.value.trim()));
bind('opt-llm-custom-url', (t) => (S.settings.summary.customUrl = t.value.trim()));
bind('opt-llm-custom-model', (t) => (S.settings.summary.customModel = t.value.trim()));
bind('opt-top', (t) => (S.settings.alwaysOnTop = t.checked));
bind('opt-audio', (t) => (S.settings.saveAudio = t.checked));
bind('opt-unload', (t) => (S.settings.unloadWhenIdle = t.checked));
bind('opt-autoupdate', (t) => { S.settings.autoUpdate = t.checked; renderAbout(); });
bind('opt-usd-krw', (t) => {
  const v = Math.round(Number(String(t.value).replace(/,/g, '')));
  if (v >= 100 && v <= 10000) S.settings.usdKrw = v;
  t.value = usdKrw();
  renderUsage();
});
bindSeg('seg-theme', (v) => { S.settings.theme = v; applyTheme(); });
$('#opt-autostart').addEventListener('change', (e) => call('set_autostart', { enabled: e.target.checked }).then(() => (S.app.autostart = e.target.checked)));

for (const [id, name] of KEY_INPUTS) {
  $(`#${id}`).addEventListener('change', async (e) => {
    const v = e.target.value.trim();
    if (!v) return;
    await call('set_api_key', { name, value: v });
    e.target.value = '';
    await refreshApp();
    renderSettings();
    toast('API 키를 안전하게 저장했어요');
  });
}

document.addEventListener('click', async (e) => {
  const b = e.target.closest('[data-clear-key]');
  if (!b) return;
  e.preventDefault();
  const [, name] = KEY_INPUTS.find(([id]) => id === b.dataset.clearKey) || [];
  if (!name) return;
  const shared = ['openai', 'groq'].includes(name) ? '\n음성 인식과 요약에 같이 쓰는 키라 둘 다에서 지워져요.' : '';
  const ok = await api.ask(`저장된 API 키를 지울까요?${shared}`, { title: '받아쓰기', kind: 'warning' });
  if (!ok) return;
  await call('set_api_key', { name, value: '' });
  await refreshApp();
  renderSettings();
  toast(name === 'anthropic' ? '키를 지웠어요. 이제 ant 로그인이 있으면 그걸로 요약해요' : '저장된 키를 지웠어요');
});

$('#btn-test-llm').addEventListener('click', async (e) => {
  const b = e.currentTarget;
  b.disabled = true;
  b.textContent = '확인 중…';
  try {
    await new Promise((r) => setTimeout(r, 300)); // 방금 바꾼 설정이 저장되도록
    const models = await api.invoke('list_llm_models');
    $('#llm-models').innerHTML = models.map((m) => `<option value="${esc(m)}"></option>`).join('');
    toast(models.length ? `연결 성공 · 모델 ${models.length}개 (모델 칸을 눌러 고를 수 있어요)` : '연결 성공');
  } catch (err) { toast(String(err), 'error'); }
  b.disabled = false;
  b.textContent = '연결 확인 · 모델 목록 불러오기';
});

$('#btn-test').addEventListener('click', async (e) => {
  const b = e.currentTarget;
  b.disabled = true;
  b.textContent = '확인 중…';
  try { toast(await api.invoke('test_engine')); } catch (err) { toast(String(err), 'error'); }
  b.disabled = false;
  b.textContent = '연결 테스트';
});

// 단축키 입력 — 키캡을 누르면 새 조합을 기다린다
let hotkeyCapturing = false;
const hk = $('#opt-hotkey');
const HOTKEY_DESC = '어디서든 녹음을 시작하고 멈춰요';
const keycaps = (keys) => keys.map((k) => `<kbd>${esc(k)}</kbd>`).join('<i>+</i>');
function renderHotkey(note) {
  const desc = $('#hotkey-desc');
  desc.classList.toggle('warn', !!note);
  if (hotkeyCapturing) {
    desc.textContent = note || 'Esc 취소 · Backspace 단축키 없애기';
    return;
  }
  const k = S.settings?.hotkey;
  hk.innerHTML = k ? keycaps(k.split('+')) : '<span class="kc-note">없음 · 눌러서 정하기</span>';
  desc.textContent = HOTKEY_DESC;
}
function stopHotkeyCapture() { hotkeyCapturing = false; hk.classList.remove('capturing'); if (S.settings) renderHotkey(); }
hk.addEventListener('click', () => {
  if (hotkeyCapturing) return;
  hotkeyCapturing = true;
  hk.classList.add('capturing');
  hk.innerHTML = '<span class="kc-note">조합키를 누르세요…</span>';
  renderHotkey();
});
hk.addEventListener('blur', stopHotkeyCapture);
hk.addEventListener('keydown', (e) => {
  if (!hotkeyCapturing) return;
  e.preventDefault();
  if (e.key === 'Escape') return stopHotkeyCapture();
  if (e.key === 'Backspace') { S.settings.hotkey = ''; saveSettings(); renderDock(); return stopHotkeyCapture(); }
  const mods = [];
  if (e.ctrlKey) mods.push('Ctrl');
  if (e.altKey) mods.push('Alt');
  if (e.shiftKey) mods.push('Shift');
  if (e.metaKey) mods.push('Super');
  let key = '';
  if (/^Key[A-Z]$/.test(e.code)) key = e.code.slice(3);
  else if (/^Digit\d$/.test(e.code)) key = e.code.slice(5);
  else if (/^F\d{1,2}$/.test(e.code)) key = e.code;
  else if (['Space', 'Insert', 'Home', 'End', 'PageUp', 'PageDown', 'Pause'].includes(e.code)) key = e.code;
  // 조합키만 누른 동안은 누른 것을 보여 준다
  if (!key) { if (mods.length) hk.innerHTML = keycaps([...mods, '…']); return; }
  if (!mods.length && !/^F\d/.test(key)) { hk.innerHTML = '<span class="kc-note">조합키를 누르세요…</span>'; renderHotkey('Ctrl · Alt · Shift 와 함께 눌러 주세요'); return; }
  const prev = S.settings.hotkey;
  S.settings.hotkey = [...mods, key].join('+');
  stopHotkeyCapture();
  hk.blur();
  // 바로 저장해 결과를 본다 — 다른 프로그램이 쓰는 조합이면 앱이 원래 단축키로 되돌린다
  api.invoke('save_settings', { settings: S.settings })
    .then(() => toast(`단축키: ${S.settings.hotkey}`))
    .catch((err) => {
      S.settings.hotkey = prev;
      stopHotkeyCapture();
      renderDock();
      toast(String(err), 'error');
    });
  renderDock();
});

$('#btn-github').addEventListener('click', () => call('open_url', { url: 'https://github.com/jeiel85/badasugi' }));
$('#btn-folder-sessions').addEventListener('click', () => call('open_folder', { kind: 'sessions' }));
$('#btn-folder-models').addEventListener('click', () => call('open_folder', { kind: 'models' }));
$('#btn-folder-logs').addEventListener('click', () => call('open_folder', { kind: 'logs' }));
$('#btn-usage-refresh').addEventListener('click', () => renderUsage());
$('#btn-quit').addEventListener('click', () => call('quit_app'));

/* ───────────── 시작 ───────────── */
(async function init() {
  await api.listen('bd://event', (e) => onEvent(e.payload));
  await api.listen('bd://rec', (e) => onRec(e.payload));
  await api.listen('bd://download', (e) => onDownload(e.payload));
  await api.listen('bd://toast', (e) => toast(String(e.payload), 'error'));
  await api.listen('bd://paused', (e) => onPaused(!!e.payload));
  await api.listen('bd://sleep', (e) => onSleep(e.payload));
  await api.listen('bd://summary-progress', (e) => {
    // 멈춘 앞 요약이 끝까지 받는 답의 진행은 버린다 (같은 회의를 바로 다시 요약해도)
    if (!S.summarizing || e.payload?.id !== S.sumId || e.payload?.token !== S.sumToken) return;
    S.sumProgress = e.payload;
    renderSummary();
  });
  await api.listen('bd://ask-progress', (e) => {
    // 멈춘 앞 질문이 끝까지 받는 답은 버린다
    const p = e.payload, a = S.ask;
    if (!a || p?.token !== a.token || a.cancelling) return;
    if (p.restart) { a.text = ''; a.restarted = true; }
    if (p.delta) { a.text += p.delta; a.waitSecs = 0; }
    if (p.waitSecs != null) a.waitSecs = p.waitSecs;
    if (!askFrame) askFrame = requestAnimationFrame(renderAskPending);
  });
  await api.listen('bd://mini', (e) => setMini(!!e.payload));
  await api.listen('bd://update', (e) => onUpdate(e.payload));
  await refreshState();
  syncMaxed();
  $('#drop-exts').textContent = exts().join(' · ');
  renderUpdate();
  if (S.app.hotkeyError) setTimeout(() => toast(S.app.hotkeyError, 'warn'), 600);
  // 화면이 끝까지 준비됐다는 신호 (자동 점검이 확인한다)
  api.invoke('ui_ready').catch(() => {});
  if (S.app.updatedFrom) toast(`v${S.app.version}(으)로 업데이트했어요`);
  if (S.app.demo) loadDemo();
  if (S.app.downloading) S.downloads[S.app.downloading] = { done: 0, total: 1 };
  renderDock();
  renderDoc();
  if (!$('#empty').hidden) renderRecent();
})();

/* ───────────── 데모 · 브라우저 미리보기 ───────────── */
function demoDoc() {
  const lines = [
    [0, 2100, 0, '자, 그럼 시작할게요. 오늘은 다음 달 앱 출시 일정부터 확정하죠.'],
    [7200, 16800, 1, '네, 결제 모듈은 개발이 끝났고 지금 테스트 서버에서 검증 중입니다. 금요일까지는 배포 가능할 것 같아요.'],
    [17900, 24300, 2, '디자인은 온보딩 화면 두 개만 남았어요. 수요일까지 시안 드릴게요.'],
    [25600, 33400, 0, '좋아요. 그럼 출시일은 15일로 잡고, 목요일에 한 번 더 점검합시다.'],
    [34800, 45100, 1, '그 전에 키오스크 응답 속도 이슈는 꼭 짚고 넘어가야 할 것 같습니다. 인덱스를 추가해서 평균 2초에서 0.4초로 줄였어요.'],
    [46300, 51200, 2, '오, 체감이 확 다르겠네요. 매장 피드백도 다시 받아볼게요.'],
    [52800, 60500, 0, '네, 매장 피드백은 정리해서 금요일까지 공유 부탁드려요.'],
  ];
  return {
    version: 1,
    id: 'demo',
    title: '앱 출시 일정 회의',
    startedAt: '2026-11-12 14:30',
    durationMs: 61000,
    engine: '로컬 · large-v3-turbo-q5_0',
    speakers: { 0: '김 팀장', 1: '이 개발', 2: '박 디자인' },
    utterances: lines.map(([s, e, spk, text], i) => ({ id: i, startMs: s, endMs: e, speaker: spk, text, latencyMs: 1400 + ((i * 523) % 1700) })),
    summary: '### 한 줄 요약\n다음 달 15일 앱 출시를 확정하고 목요일에 최종 점검한다.\n\n### 결정 사항\n- 출시일 **15일** 확정, 목요일 최종 점검\n\n### 할 일\n- [ ] 결제 모듈 배포 — 담당: 이 개발 / 기한: 금요일\n- [ ] 온보딩 시안 2종 — 담당: 박 디자인 / 기한: 수요일\n- [ ] 매장 피드백 정리·공유 — 담당: 박 디자인 / 기한: 금요일',
    audioFile: null,
    // 시각 줄 = 그 시각 문장의 코멘트 (00:34 · 00:35 는 같은 문장에 둘)
    notes: '[00:07] 결제 모듈 — 테스트 서버 검증 결과 금요일에 공유받기\n[00:34] 0.4초는 피크 시간대 기준인지 확인\n[00:35] 키오스크 인덱스: 2초 → 0.4초\n[00:52] 금요일 공유 — 담당 박 디자인\n\n- 매장 피드백 양식 미리 보내 두기',
    // 회의 후 다시 정리 전 (화면 미리보기에서 '화자 정리 되돌리기' 버튼을 보이게)
    beforeRefine: { speakers: { 0: '김 팀장', 1: '이 개발', 2: '박 디자인', 3: '최 기획' }, assign: lines.map((l, i) => [i, i === 3 ? 3 : l[2]]) },
  };
}

function loadDemo() {
  S.liveDoc = demoDoc();
  S.rec = 'recording';
  S.timerStart = Date.now() - 61000;
  S.pending.set(99, { startMs: 62000, endMs: 64000, speaker: 1 });
  S.speaking = true;
  renderDock();
  setInterval(() => setLevel(0.02 + Math.random() * 0.12), 90);
}

// 브라우저 미리보기용: 발화 구간마다 다른 높이의 소리가 나는 WAV
let demoWav;
function demoWavUrl() {
  if (demoWav) return demoWav;
  const rate = 8000, secs = 66, n = rate * secs;
  const buf = new ArrayBuffer(44 + n * 2), v = new DataView(buf);
  const str = (o, s) => [...s].forEach((c, i) => v.setUint8(o + i, c.charCodeAt(0)));
  str(0, 'RIFF'); v.setUint32(4, 36 + n * 2, true); str(8, 'WAVEfmt '); v.setUint32(16, 16, true); v.setUint16(20, 1, true); v.setUint16(22, 1, true);
  v.setUint32(24, rate, true); v.setUint32(28, rate * 2, true); v.setUint16(32, 2, true); v.setUint16(34, 16, true); str(36, 'data'); v.setUint32(40, n * 2, true);
  const segs = demoDoc().utterances;
  for (let i = 0; i < n; i++) {
    const t = i / rate, ms = t * 1000;
    const u = segs.find((x) => ms >= x.startMs && ms <= x.endMs);
    const f = u ? 180 + u.speaker * 90 : 0;
    v.setInt16(44 + i * 2, f ? Math.sin(2 * Math.PI * f * t) * 2500 : 0, true);
  }
  demoWav = URL.createObjectURL(new Blob([buf], { type: 'audio/wav' }));
  return demoWav;
}

// 브라우저 미리보기용 회의 기록 (오늘 · 어제 · 지난주)
function mockSessions() {
  const p = (n) => String(n).padStart(2, '0');
  const day = (ago) => { const d = new Date(); d.setDate(d.getDate() - ago); return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`; };
  const prev = '자, 그럼 시작할게요. 오늘은 다음 달 앱 출시 일정부터 확정하죠.';
  return [
    { id: 's5', title: '업무 인수 인계 회의', startedAt: `${day(0)} 13:59`, durationMs: 4038000, count: 412, speakers: 2, preview: prev },
    { id: 's4', title: '주간 개발 회의', startedAt: `${day(0)} 10:00`, durationMs: 1820000, count: 160, speakers: 4, preview: prev },
    { id: 'demo', title: '앱 출시 일정 회의', startedAt: `${day(1)} 14:30`, durationMs: 61000, count: 7, speakers: 3, preview: prev },
    { id: 's2', title: '키오스크 응답 속도 점검', startedAt: `${day(4)} 16:10`, durationMs: 960000, count: 88, speakers: 3, preview: prev },
    { id: 's1', title: '매장 피드백 정리', startedAt: `${day(8)} 11:20`, durationMs: 1500000, count: 120, speakers: 2, preview: prev },
    { id: 's0', title: '분기 계획 회의', startedAt: `${day(12)} 15:00`, durationMs: 3300000, count: 300, speakers: 5, preview: prev },
  ];
}

let mockVoices = [{ name: '김 팀장', meetings: 4, secs: 812 }, { name: '이 개발', meetings: 2, secs: 240 }, { name: '박 디자인', meetings: 1, secs: 95 }];
function createMock() {
  const handlers = {};
  let mockSumCancel = null;
  // 브라우저 미리보기: 앱처럼 녹취록을 읽은 뒤 섹션을 차례로 쓰며 진행을 보낸다 (?sumslow 면 느리게)
  async function mockSummarize({ id, token }) {
    const slow = new URLSearchParams(location.search).has('sumslow') ? 4 : 1;
    const wait = (ms) => new Promise((r) => setTimeout(r, ms * slow));
    const send = (p) => handlers['bd://summary-progress'] && handlers['bd://summary-progress']({ payload: { id, token, done: 0, total: 1, waitSecs: 0, ...p } });
    const check = () => { if (mockSumCancel === token) throw '요약을 멈췄어요'; };
    await wait(1500); check();
    const steps = [['제목', 0.0], ['제목', 0.01], ['한 줄 요약', 0.02], ['주요 논의', 0.08], ['주요 논의', 0.3], ['주요 논의', 0.45], ['결정 사항', 0.53], ['결정 사항', 0.62], ['할 일', 0.68], ['할 일', 0.85], ['남은 질문', 0.9], ['남은 질문', 0.97]];
    for (const [section, frac] of steps) { send({ writing: true, section, frac }); await wait(450); check(); }
    return { text: '### 한 줄 요약\n15일 출시를 확정했다.\n\n### 주요 논의\n- 김 팀장: 출시 일정 확인\n\n### 결정 사항\n- 15일 출시\n\n### 할 일\n- [ ] 공지 — 담당: 박 디자인 / 기한: 14일\n\n### 남은 질문\n- 없음', title: null };
  }
  // 브라우저 미리보기: 읽는 척 잠깐 쉬었다가 답을 조금씩 보낸다 (?askslow 면 느리게 · ?askfail 이면 오류)
  const mockQa = {};
  let mockAskCancel = null;
  async function mockAsk({ id, question, token }) {
    const q = new URLSearchParams(location.search);
    const wait = (ms) => new Promise((r) => setTimeout(r, ms * (q.has('askslow') ? 4 : 1)));
    const send = (p) => handlers['bd://ask-progress'] && handlers['bd://ask-progress']({ payload: { id, token, ...p } });
    const check = () => { if (mockAskCancel === token) throw '답을 멈췄어요'; };
    await wait(1200); check();
    if (q.has('askfail')) throw 'AI API 오류 429: Rate limit reached (요청 한도를 넘었어요. 잠시 뒤 다시 시도해 주세요)';
    const answer = /할 일|담당/.test(question)
      ? '- **결제 모듈 배포** — 이 개발, 금요일까지 [00:07]\n- **온보딩 시안 2종** — 박 디자인, 수요일까지 [00:17]\n- **매장 피드백 정리 · 공유** — 박 디자인, 금요일까지 [00:46] [00:52]'
      : '출시일은 **15일**로 정했고, 목요일에 한 번 더 점검하기로 했어요 [00:25]. 그 전에 키오스크 응답 속도는 인덱스를 추가해 평균 2초에서 0.4초로 줄였다고 이 개발이 말했어요 [00:34].';
    for (const piece of answer.match(/[\s\S]{1,9}/g)) { send({ delta: piece }); await wait(60); check(); }
    const p = (n) => String(n).padStart(2, '0');
    const now = new Date();
    const item = { q: question.trim(), a: answer, at: `${now.getFullYear()}-${p(now.getMonth() + 1)}-${p(now.getDate())} ${p(now.getHours())}:${p(now.getMinutes())}`, model: 'claude-opus-5-5' };
    (mockQa[id] ||= []).push(item);
    return item;
  }
  const st = {
    settings: {
      engine: 'local', localModel: 'large-v3-turbo-q5_0', beamSize: 1, threads: 0,
      openaiModel: 'gpt-4o-transcribe', groqModel: 'whisper-large-v3-turbo', customUrl: '', customModel: '',
      source: 'mic', micDevice: null, language: 'ko', glossary: '',
      diarization: { enabled: true, threshold: 0.45, maxSpeakers: 0, splitTurns: true }, rememberVoices: true,
      saveAudio: false, unloadWhenIdle: true, hotkey: 'Ctrl+Alt+R', alwaysOnTop: false, theme: 'system',
      autoUpdate: true, lastVersion: '', miniMode: false, miniPos: null, fullBounds: null,
      summary: { provider: 'ollama', ollamaUrl: 'http://localhost:11434', ollamaModel: 'exaone3.5:7.8b', groqModel: 'openai/gpt-oss-120b', geminiModel: 'gemini-flash-latest', openaiModel: 'gpt-5-mini', anthropicModel: 'claude-opus-5-5', customUrl: '', customModel: '' },
      usdKrw: 1360,
    },
    rec: 'idle',
    models: [
      { id: 'large-v3-turbo-q5_0', label: 'Large v3 Turbo (q5)', note: '추천 · 빠르고 한국어 정확도가 높음', size: 574041195, ready: true },
      { id: 'large-v3-turbo-q8_0', label: 'Large v3 Turbo (q8)', note: '조금 더 정확 · 메모리 더 사용', size: 874188075, ready: false },
      { id: 'medium-q5_0', label: 'Medium (q5)', note: '중간 사양 PC', size: 539212467, ready: false },
    ],
    customModels: [], speakerModelReady: true,
    keys: { openai: false, groq: false, custom: false, anthropic: false, gemini: false, llmCustom: false },
    autostart: false, version: '0.2.0', current: null, elapsedMs: 0,
    paused: false, fileMode: false, update: null, updatedFrom: null,
    audioExts: ['wav', 'mp3', 'm4a', 'mp4', 'mov', 'aac', 'flac', 'ogg', 'oga', 'opus'],
    demo: new URLSearchParams(location.search).has('demo'),
  };
  return {
    invoke: async (cmd, args) => {
      if (cmd === 'get_state') return st;
      if (cmd === 'save_settings') { st.settings = args.settings; return; }
      if (cmd === 'list_devices') return [{ id: 'mic1', name: '마이크(Realtek Audio)', isDefault: true }];
      if (cmd === 'search_sessions') return [];
      if (cmd === 'update_notes') return [
        { version: '0.3.1', date: '2026-11-13', notes: '## 받아쓰기 v0.3.1\n\n### 새 기능\n- 📊 **API 사용량** — 설정에 공급자별 사용량이 생겼어요.\n  - **오늘 · 이번 달**: 음성 인식 분량 · 요약 토큰\n  - **남은 한도**: 공급자가 알려 주는 남은 요청 · 토큰' },
        { version: '0.3.0', date: '2026-11-13', notes: '## 받아쓰기 v0.3.0\n\n### 새 기능\n- 🩺 **진단 로그** — 문제가 생겼을 때 원인을 찾을 수 있게 기록해요.\n- 🔑 **API 키 받는 방법 안내**\n\n### 개선\n- 🎤 마이크 소리가 거의 안 들어올 때 알려 줍니다' },
      ];
      if (cmd === 'get_usage') return {
        today: {
          groq: { sttSecs: 3426, sttRequests: 848, sttFailures: 0, sumRequests: 15, sumInTokens: 33953, sumOutTokens: 29412 },
          anthropic: { sttSecs: 0, sttRequests: 0, sttFailures: 0, sumRequests: 2, sumInTokens: 51788, sumOutTokens: 11674 },
        },
        month: {
          groq: { sttSecs: 9120, sttRequests: 1180, sttFailures: 3, sumRequests: 24, sumInTokens: 72073, sumOutTokens: 34822 },
          anthropic: { sttSecs: 0, sttRequests: 0, sttFailures: 0, sumRequests: 2, sumInTokens: 51788, sumOutTokens: 11674 },
          gemini: { sttSecs: 0, sttRequests: 0, sttFailures: 0, sumRequests: 2, sumInTokens: 15200, sumOutTokens: 1900 },
        },
        todayCost: {
          groq: { sttUsd: 0.0953, sumUsd: 0.0227, sttBilledSecs: 8580, estimated: false, unpriced: [] },
          anthropic: { sttUsd: 0, sumUsd: 0.4406, sttBilledSecs: 0, estimated: false, unpriced: [] },
        },
        monthCost: {
          groq: { sttUsd: 0.1532, sumUsd: 0.0317, sttBilledSecs: 13790, estimated: true, unpriced: [] },
          anthropic: { sttUsd: 0, sumUsd: 0.4406, sttBilledSecs: 0, estimated: false, unpriced: [] },
          gemini: { sttUsd: 0, sumUsd: 0, sttBilledSecs: 0, estimated: false, unpriced: ['gemini-flash-latest'] },
        },
        rates: [{ provider: 'groq', at: Math.floor(Date.now() / 1000) - 180, items: [['x-ratelimit-limit-requests', '2000'], ['x-ratelimit-remaining-requests', '1904'], ['x-ratelimit-reset-requests', '1h9m7.2s'], ['x-ratelimit-limit-tokens', '6000'], ['x-ratelimit-remaining-tokens', '5560'], ['x-ratelimit-reset-tokens', '4.4s']] }],
      };
      if (cmd === 'session_audio') return demoWavUrl();
      if (cmd === 'list_voices') return mockVoices;
      if (cmd === 'forget_voice') { mockVoices = mockVoices.filter((v) => v.name !== args.name); return; }
      if (cmd === 'forget_all_voices') { mockVoices = []; return; }
      if (cmd === 'set_mini') { st.settings.miniMode = args.mini; handlers['bd://mini'] && handlers['bd://mini']({ payload: args.mini }); return; }
      if (cmd === 'list_sessions') return mockSessions();
      if (cmd === 'load_session' || cmd === 'undo_refinement') {
        const m = mockSessions().find((x) => x.id === args.id);
        const d = { ...demoDoc(), ...(m && { id: m.id, title: m.title, startedAt: m.startedAt }), qa: [...(mockQa[args.id] || [])] };
        if (cmd === 'undo_refinement') { delete d.beforeRefine; d.speakers = { 0: '김 팀장', 1: '이 개발', 2: '박 디자인', 3: '최 기획' }; d.utterances[3].speaker = 3; }
        return d;
      }
      if (cmd === 'session_text') return '데모';
      if (cmd === 'summarize') return mockSummarize(args);
      if (cmd === 'cancel_summary') { mockSumCancel = args.token; return; }
      if (cmd === 'ask_meeting') return mockAsk(args);
      if (cmd === 'cancel_ask') { mockAskCancel = args.token; return; }
      if (cmd === 'delete_qa') {
        await new Promise((r) => setTimeout(r, 300)); // 앱처럼 잠깐 걸린다 (두 번 눌러 보기)
        const l = mockQa[args.id] || [];
        if (!args.item) l.length = 0;
        else { const k = l.findIndex((y) => y.q === args.item.q && y.at === args.item.at && y.a === args.item.a); if (k >= 0) l.splice(k, 1); }
        return;
      }
      if (cmd === 'toggle_recording') { const h = handlers['bd://rec']; st.rec = st.rec === 'idle' ? 'recording' : 'idle'; h && h({ payload: st.rec }); return; }
      return null;
    },
    listen: async (ev, fn) => { handlers[ev] = fn; },
    win: {
      minimize() {},
      hide() {},
      _max: false,
      _full: false,
      async toggleMaximize() { this._max = !this._max; },
      async setFullscreen(on) { this._full = !!on; },
      async isMaximized() { return this._max; },
      async isFullscreen() { return this._full; },
      onResized() {},
    },
    save: async () => null,
    open: async () => null,
    ask: async (m) => confirm(m),
  };
}
