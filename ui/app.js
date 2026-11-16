'use strict';

/* ───────────── 기반 ───────────── */
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const T = window.__TAURI__;
// 기록을 읽고 쓰는 명령은 앱이 작업 스레드에서 처리한다 — 보낸 순서대로 끝나도록 하나씩 보낸다
// (편집을 저장하고 바로 다시 읽어도 저장된 내용을 받게)
const ORDERED_CMDS = new Set(['save_session', 'load_session', 'undo_refinement', 'delete_session', 'export_session', 'session_text', 'session_audio']);
// 앞서 보낸 기록 명령(편집 저장 등)이 끝난 뒤에 시작하지만, 뒤에 오는 명령은 붙잡지 않는다 (요약은 몇 분 걸릴 수 있다)
const AFTER_CMDS = new Set(['summarize']);
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
        ? `<div class="u" data-id="${it.id}">${hl(it.text)}${it.latencyMs != null ? `<span class="lat">${(it.latencyMs / 1000).toFixed(1)}초</span>` : ''}</div>`
        : it.kind === 'p'
          ? `<div class="u pending" style="--w:${40 + ((it.id * 37) % 50)}%"></div>`
          : `<div class="u err">인식 실패: ${esc(it.message)}</div>`).join('')}
    </div>`));

  $('#listening').hidden = !(S.live && S.rec === 'recording' && S.speaking && !S.paused && !S.fileMode);
  renderMeta();
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

// 긴 회의는 구간으로 나눠 정리한다 → "긴 회의라 나눠서 정리하는 중… (2/5)"
function summaryStep(p) {
  if (!p || p.total <= 1) return '회의 내용을 정리하는 중…';
  const at = `(${Math.min(p.done + 1, p.total)}/${p.total})`;
  if (p.waitSecs > 0) return `요청 한도 때문에 ${p.waitSecs}초 기다렸다가 이어서 정리해요… ${at}`;
  if (p.done >= p.total - 1) return `구간 메모를 합쳐 회의록을 만드는 중… ${at}`;
  return `긴 회의라 구간으로 나눠 정리하는 중… ${at}`;
}
function renderSummary() {
  const d = doc();
  const card = $('#summary-card');
  // 요약하는 동안 다른 기록으로 옮겨 가도, 그 기록에 '정리하는 중'을 띄우지 않는다
  if (S.summarizing && d && d.id === S.sumId) {
    card.hidden = false;
    const stop = S.sumCancelling
      ? '<button class="link" id="btn-cancel-summary" disabled>멈추는 중…</button>'
      : '<button class="link" id="btn-cancel-summary" title="보내 둔 요청은 버리고 여기서 그만둬요">멈추기</button>';
    $('#summary-body').innerHTML = `<div class="loading"><span class="dot"></span>${esc(summaryStep(S.sumProgress))}${stop}</div>`;
    return;
  }
  card.hidden = !(d && d.summary);
  if (d && d.summary) $('#summary-body').innerHTML = md(d.summary);
}

function updateActions() {
  const d = doc();
  const has = !!(d && d.utterances.length);
  $('#btn-copy').disabled = !has;
  $('#btn-save').disabled = !has;
  $('#btn-summary').disabled = !has || S.summarizing;
  renderBanners(has);
}

// 위쪽 배너: 지난 회의를 보는 중 / 방금 회의를 마침 (저장됐다고 알리고 → 새 회의 · 회의 기록)
function renderBanners(has) {
  const v = !S.live ? S.viewDoc : null;
  $('#viewing').hidden = !v;
  if (v) {
    // 제목은 바로 아래에 있으니 배너에는 언제 회의인지를 보여 준다
    $('#viewing-label').textContent = `지난 회의 · ${whenLabel(v.startedAt)}`;
    const back = $('#btn-back-live');
    const live = S.rec !== 'idle';
    back.textContent = live ? '실시간으로 돌아가기' : '닫기';
    back.title = live ? '받아 적는 중인 회의로 돌아가요' : '이 기록을 닫아요';
  }
  const ended = S.live && S.rec === 'idle' && has;
  $('#ended').hidden = !ended;
  if (ended) $('#ended-label').textContent = `${S.liveDoc?.sourceFile ? '파일 받아쓰기를' : '회의를'} 마쳤어요 · 기록에 저장됨`;
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
  persistDoc.flush(); // 방금 고친 내용이 있으면 먼저 저장
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
    S.live = true;
    S.pending.clear();
    S.failed.clear();
    S.liveDoc = null;
    S.slowHinted = false;
    S.paused = false;
    S.sleepPaused = false;
    S.progress = null;
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
        api.invoke('load_session', { id }).then((fresh) => {
          if (S.liveDoc?.id !== id) return;
          S.liveDoc = fresh;
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
      api.invoke('get_state').then(async (st) => {
        S.app = st;
        if (id) {
          try {
            const fresh = await api.invoke('load_session', { id });
            if (S.liveDoc?.id === id) S.liveDoc = fresh;
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
$('#btn-back-live').addEventListener('click', () => { S.live = true; S.viewDoc = null; follow = true; renderDock(); scheduleRender(); });
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
  persistDoc.flush();
  try {
    const fresh = await api.invoke('undo_refinement', { id: d.id });
    if (S.live && S.liveDoc?.id === d.id) S.liveDoc = fresh;
    else if (S.viewDoc?.id === d.id) S.viewDoc = fresh;
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

// 발화 고치기 (더블클릭)
$('#transcript').addEventListener('dblclick', (e) => {
  clearTimeout(playClick);
  if (!audio.paused && P.until != null) audio.pause();
  const el = e.target.closest('.u[data-id]');
  const d = doc();
  if (!el || !d || el.isContentEditable) return;
  const u = d.utterances.find((x) => x.id === +el.dataset.id);
  if (!u) return;
  editing = true;
  el.textContent = u.text;
  el.contentEditable = 'true';
  el.focus();
  const finish = (save) => {
    el.contentEditable = 'false';
    editing = false;
    invalidateTranscript();
    // innerText 는 화면 배치에 기대므로(가려진 문단은 빈 값) 글자 그대로인 textContent 로 읽는다 — 빈 값이면 발화가 지워지므로 중요
    const v = (el.textContent || '').replace(/\s+/g, ' ').trim();
    if (save && !v) {
      d.utterances = d.utterances.filter((x) => x.id !== u.id);
      d.deleted = [...(d.deleted || []), u.id];
      persistDoc(d);
      toast('발화를 지웠어요');
    } else if (save && v !== u.text) { u.text = v; persistDoc(d); }
    scheduleRender();
  };
  el.addEventListener('keydown', (k) => {
    if (k.key === 'Enter' && !k.shiftKey) { k.preventDefault(); el.blur(); }
    if (k.key === 'Escape') { el.textContent = u.text; finish(false); }
  });
  el.addEventListener('blur', () => finish(true), { once: true });
});

const persistDoc = debounce((d) => {
  api.invoke('save_session', { doc: d }).catch((e) => toast(String(e), 'error'));
}, 300);

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
function hidePopovers() { $$('.popover').forEach((p) => (p.hidden = true)); }
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
$('#btn-summary').addEventListener('click', async () => {
  const d = doc();
  if (!d || S.summarizing) return;
  if (S.rec !== 'idle' && S.live) toast('지금까지의 내용으로 요약할게요');
  // 방금 고친 문장이 요약에 들어가게, 기다리던 저장을 먼저 보낸다 (요약은 그 저장이 끝난 뒤 기록을 읽는다)
  persistDoc.flush();
  S.summarizing = true;
  S.sumCancelling = false;
  S.sumProgress = null;
  S.sumId = d.id;
  // 요약마다 번호 — 앞선 저장을 기다리는 동안 멈춰도 앱이 그 요약을 알아보고 시작하자마자 멈춘다
  S.sumToken = Date.now();
  $('#summary-card').classList.remove('folded');
  renderSummary();
  updateActions();
  // 녹음 중에 요약해도 위에 뜬 요약 카드를 보고 있게 따라가기를 멈춘다
  follow = false;
  body.scrollTop = 0;
  try {
    const r = await api.invoke('summarize', { id: d.id, token: S.sumToken });
    d.summary = r.text;
    if (r.title) { d.title = r.title; d.autoTitle = false; renderMeta(); }
  } catch (e) {
    // 사람이 멈춘 것은 오류가 아니다
    const msg = String(e);
    toast(msg, msg === '요약을 멈췄어요' ? undefined : 'error');
  } finally {
    S.summarizing = false;
    S.sumCancelling = false;
    S.sumProgress = null;
    renderSummary();
    updateActions();
  }
});
$('#summary-card').addEventListener('click', (e) => {
  const b = e.target.closest('#btn-cancel-summary');
  if (!b || !S.summarizing || S.sumCancelling) return;
  S.sumCancelling = true;
  renderSummary();
  api.invoke('cancel_summary', { token: S.sumToken }).catch(() => {});
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
    if (!$('#findbar').hidden && $$('.panel').every((p) => p.hidden)) closeFind();
    closePanels();
    hidePopovers();
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
      if (S.viewDoc?.id === gone) { S.viewDoc = null; S.live = true; }
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
  if (!el || !P.src || editing || e.target.closest('.g-name')) return;
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
function tallyLines(t) {
  const out = [];
  if (t?.sttRequests) out.push(`음성 인식 ${minutes(t.sttSecs)} · ${n0(t.sttRequests)}회${t.sttFailures ? ` (실패 ${n0(t.sttFailures)})` : ''}`);
  if (t?.sumRequests) out.push(`요약 ${n0(t.sumRequests)}회 · 토큰 입력 ${n0(t.sumInTokens)} · 출력 ${n0(t.sumOutTokens)}`);
  return out.length ? out : ['없음'];
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
  // 항목마다 한 줄씩 (음성 인식 · 요약 · 한도 종류)
  const lines = (arr) => arr.map((x) => `<div>${x}</div>`).join('');
  box.innerHTML = ids.map((id) => {
    const p = PROVIDER[id] || { name: id, url: '', link: '' };
    const rate = (r.rates || []).find((x) => x.provider === id);
    const lim = limitsInfo(rate);
    return `<div class="usage-card">
      <div class="usage-head"><b>${esc(p.name)}</b>${p.url ? `<button class="link" data-url="${p.url}">${esc(p.link)}</button>` : ''}</div>
      <div class="usage-row"><span>오늘</span><span>${lines(tallyLines(r.today?.[id]).map(esc))}</span></div>
      <div class="usage-row"><span>이번 달</span><span>${lines(tallyLines(r.month?.[id]).map(esc))}</span></div>
      ${lim ? `<div class="usage-row"><span>남은 한도<small>${lim.when} 기준</small></span><span>${lines(lim.lines)}</span></div>` : ''}
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
    if (!S.summarizing || e.payload?.id !== S.sumId) return;
    S.sumProgress = e.payload;
    renderSummary();
  });
  await api.listen('bd://mini', (e) => setMini(!!e.payload));
  await api.listen('bd://update', (e) => onUpdate(e.payload));
  await refreshState();
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
  const st = {
    settings: {
      engine: 'local', localModel: 'large-v3-turbo-q5_0', beamSize: 1, threads: 0,
      openaiModel: 'gpt-4o-transcribe', groqModel: 'whisper-large-v3-turbo', customUrl: '', customModel: '',
      source: 'mic', micDevice: null, language: 'ko', glossary: '',
      diarization: { enabled: true, threshold: 0.45, maxSpeakers: 0, splitTurns: true }, rememberVoices: true,
      saveAudio: false, unloadWhenIdle: true, hotkey: 'Ctrl+Alt+R', alwaysOnTop: false, theme: 'system',
      autoUpdate: true, lastVersion: '', miniMode: false, miniPos: null, fullBounds: null,
      summary: { provider: 'ollama', ollamaUrl: 'http://localhost:11434', ollamaModel: 'exaone3.5:7.8b', groqModel: 'openai/gpt-oss-120b', geminiModel: 'gemini-flash-latest', openaiModel: 'gpt-5-mini', anthropicModel: 'claude-opus-5-5', customUrl: '', customModel: '' },
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
        today: { groq: { sttSecs: 754, sttRequests: 96, sttFailures: 1, sumRequests: 1, sumInTokens: 4210, sumOutTokens: 612 } },
        month: { groq: { sttSecs: 9120, sttRequests: 1180, sttFailures: 3, sumRequests: 9, sumInTokens: 38120, sumOutTokens: 5410 }, gemini: { sttSecs: 0, sttRequests: 0, sttFailures: 0, sumRequests: 2, sumInTokens: 15200, sumOutTokens: 1900 } },
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
        const d = { ...demoDoc(), ...(m && { id: m.id, title: m.title, startedAt: m.startedAt }) };
        if (cmd === 'undo_refinement') { delete d.beforeRefine; d.speakers = { 0: '김 팀장', 1: '이 개발', 2: '박 디자인', 3: '최 기획' }; d.utterances[3].speaker = 3; }
        return d;
      }
      if (cmd === 'session_text') return '데모';
      if (cmd === 'toggle_recording') { const h = handlers['bd://rec']; st.rec = st.rec === 'idle' ? 'recording' : 'idle'; h && h({ payload: st.rec }); return; }
      return null;
    },
    listen: async (ev, fn) => { handlers[ev] = fn; },
    win: { minimize() {}, hide() {} },
    save: async () => null,
    open: async () => null,
    ask: async (m) => confirm(m),
  };
}
