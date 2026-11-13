'use strict';

/* ───────────── 기반 ───────────── */
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const T = window.__TAURI__;
const api = T ? {
  invoke: T.core.invoke,
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
const LLM_DESC = {
  ollama: 'PC에 따로 설치한 <b>Ollama</b>로 요약해요 (앱에 들어 있지 않아요). 인터넷 없이 동작하지만 Ollama 설치와 모델(약 5GB)이 필요하고, PC 사양에 따라 몇 분 걸릴 수 있어요. <button class="link" data-url="https://ollama.com/download">Ollama 받기 ↗</button>',
  groq: '<b>무료 등급</b>으로 바로 쓸 수 있고 매우 빨라요. 음성 인식용 Groq 키를 같이 써요. <button class="link" data-url="https://console.groq.com/keys">키 무료로 받기 ↗</button>',
  gemini: 'Google Gemini — <b>무료 등급</b>이 있고 한국어 요약 품질이 좋아요. <button class="link" data-url="https://aistudio.google.com/apikey">키 무료로 받기 ↗</button>',
  openai: 'OpenAI — 음성 인식용 OpenAI 키를 같이 써요. 사용한 만큼 과금돼요. <button class="link" data-url="https://platform.openai.com/api-keys">키 만들기 ↗</button>',
  anthropic: 'Anthropic Claude — 긴 회의도 꼼꼼하게 정리해요. 사용한 만큼 과금돼요. <button class="link" data-url="https://console.anthropic.com/settings/keys">키 만들기 ↗</button>',
  custom: 'LM Studio · vLLM · OpenRouter 같은 OpenAI 호환 서버를 써요.',
};
const LLM_PRIVACY = '<br>API를 쓰면 녹취록이 해당 서비스로 전송돼요.';

const ENGINE_DESC = {
  local: '인터넷 없이 이 PC에서 인식합니다. 회의 내용이 밖으로 나가지 않아요. 첫 사용 때 모델을 한 번 내려받습니다.',
  openai: 'OpenAI gpt-4o-transcribe — 한국어 정확도가 가장 높습니다. 사용량만큼 과금됩니다.',
  groq: 'Groq Whisper — 매우 빠르고 저렴합니다. (large-v3-turbo 기준 시간당 약 $0.04)',
  custom: 'OpenAI 호환 음성 인식 서버(faster-whisper-server, LocalAI 등)를 사용합니다.',
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
function debounce(fn, ms) {
  let t;
  return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); };
}

function md(src) {
  const inline = (s) => esc(s).replace(/\*\*(.+?)\*\*/g, '<b>$1</b>').replace(/`(.+?)`/g, '<code>$1</code>');
  let html = '', list = false;
  for (const raw of String(src).split('\n')) {
    const line = raw.trimEnd();
    const li = line.match(/^\s*[-*]\s+(\[[ xX]\]\s+)?(.*)$/);
    if (li) {
      if (!list) { html += '<ul>'; list = true; }
      html += li[1] ? `<li class="task"><span>${inline(li[2])}</span></li>` : `<li>${inline(li[2])}</li>`;
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

function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => { renderQueued = false; renderDoc(); });
}

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
  $('#empty').hidden = hasContent || S.rec !== 'idle' || !S.live;
  $('#meta').hidden = !d || (!hasContent && S.rec === 'idle');
  renderSetup();

  const stick = nearBottom();
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
  tr.innerHTML = groups.map((g) => `
    <div class="group" style="--c:${spkColor(g.speaker)}">
      <div class="g-head"><span class="g-name" data-spk="${g.speaker ?? ''}" data-ids="${g.items.filter((x) => x.kind === 'u').map((x) => x.id).join(',')}" title="이름 바꾸기 · 화자 바꾸기">${esc(speakerName(d, g.speaker))}</span><span class="g-time">${fmt(g.startMs)}</span></div>
      ${g.items.map((it) => it.kind === 'u'
        ? `<div class="u" data-id="${it.id}">${hl(it.text)}${it.latencyMs != null ? `<span class="lat">${(it.latencyMs / 1000).toFixed(1)}초</span>` : ''}</div>`
        : it.kind === 'p'
          ? `<div class="u pending" style="--w:${40 + ((it.id * 37) % 50)}%"></div>`
          : `<div class="u err">인식 실패: ${esc(it.message)}</div>`).join('')}
    </div>`).join('');

  $('#listening').hidden = !(S.live && S.rec === 'recording' && S.speaking && !S.paused && !S.fileMode);
  renderMeta();
  renderSummary();
  updateActions();
  renderMiniCap();
  syncAudio();
  markPlaying(false);
  if (S.findQ) { updateFind(false); return; }
  if (stick) body.scrollTop = body.scrollHeight;
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
  box.innerHTML = [...spk].sort((a, b) => a - b)
    .map((s) => `<button class="spk-chip" data-spk="${s}" style="--c:${spkColor(s)}" title="이름 바꾸기 · 다른 화자와 합치기"><i></i>${esc(speakerName(d, s))}</button>`)
    .join('');
}

function renderSummary() {
  const d = doc();
  const card = $('#summary-card');
  if (S.summarizing && d) {
    card.hidden = false;
    $('#summary-body').innerHTML = '<div class="loading"><span class="dot"></span>회의 내용을 정리하는 중…</div>';
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
  $('#viewing').hidden = S.live;
  if (!S.live && S.viewDoc) $('#viewing-label').textContent = `기록 보기 · ${S.viewDoc.title}`;
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
      cards.push(`<div class="setup-card"><b>${s.engine === 'openai' ? 'OpenAI' : 'Groq'} API 키가 필요해요</b><p>설정에서 키를 입력하면 바로 사용할 수 있어요. 키는 Windows 자격 증명 관리자에 안전하게 보관됩니다.</p><div class="actions"><button class="btn sm" data-open-settings>설정 열기</button></div></div>`);
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
setInterval(() => { if (S.rec === 'recording') tickTimer(); }, 250);

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
    S.progress = null;
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
    if (S.fileMode && prev !== 'idle' && S.liveDoc?.utterances.length) toast('파일 받아쓰기를 마쳤어요');
    S.fileMode = false;
    S.progress = null;
    resetLevel();
  }
  renderDock();
  scheduleRender();
}

function onEvent(e) {
  const d = S.liveDoc;
  switch (e.type) {
    case 'level':
      setLevel(e.rms);
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
      scheduleRender();
      break;
    case 'transcript':
      S.pending.delete(e.id);
      if (d) {
        const u = { id: e.id, startMs: e.startMs, endMs: e.endMs, speaker: resolveSpk(d, e.speaker), text: e.text, latencyMs: e.latencyMs };
        const i = d.utterances.findIndex((x) => x.id === e.id);
        if (i >= 0) d.utterances[i] = u; else d.utterances.push(u);
        d.durationMs = Math.max(d.durationMs || 0, e.endMs);
      }
      scheduleRender();
      break;
    case 'discarded':
      S.pending.delete(e.id);
      scheduleRender();
      break;
    case 'failed': {
      const p = S.pending.get(e.id);
      S.pending.delete(e.id);
      if (p) S.failed.set(e.id, { ...p, message: e.message });
      if (S.failed.size === 1) toast(`인식 실패: ${e.message}`, 'error');
      scheduleRender();
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
    case 'stopped':
      S.pending.clear();
      api.invoke('get_state').then((st) => { S.app = st; renderDock(); scheduleRender(); });
      break;
  }
}

function onDownload(e) {
  if (e.state === 'progress') {
    S.downloads[e.id] = { done: e.done, total: e.total };
  } else {
    delete S.downloads[e.id];
    if (e.state === 'error') { toast(e.message, 'error'); S.dlQueue = []; }
    if (e.state === 'done') {
      refreshState().then(() => { renderSettings(); nextDownload(); });
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
    if (/모델|API 키/.test(String(e))) openPanel('settings');
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
$('#btn-back-live').addEventListener('click', () => { S.live = true; S.viewDoc = null; renderDock(); scheduleRender(); });
$('#jump').addEventListener('click', () => { body.scrollTop = body.scrollHeight; $('#jump').hidden = true; });
body.addEventListener('scroll', () => { if (nearBottom()) $('#jump').hidden = true; });

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
      if (v && v !== `화자 ${+spk + 1}`) d.speakers[spk] = v; else delete d.speakers[spk];
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
      while (used.has(n)) n++;
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
  const c = e.target.closest('.spk-chip');
  if (c && !c.querySelector('input')) speakerMenu(c, c.dataset.spk, null);
});
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
  if (!el || !d) return;
  const u = d.utterances.find((x) => x.id === +el.dataset.id);
  if (!u) return;
  editing = true;
  el.textContent = u.text;
  el.contentEditable = 'true';
  el.focus();
  const finish = (save) => {
    el.contentEditable = 'false';
    editing = false;
    const v = el.innerText.trim();
    if (save && !v) {
      d.utterances = d.utterances.filter((x) => x.id !== u.id);
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
  S.summarizing = true;
  $('#summary-card').classList.remove('folded');
  renderSummary();
  updateActions();
  body.scrollTop = 0;
  try {
    const r = await api.invoke('summarize', { id: d.id });
    d.summary = r.text;
    if (r.title) { d.title = r.title; d.autoTitle = false; renderMeta(); }
  } catch (e) {
    toast(String(e), 'error');
  } finally {
    S.summarizing = false;
    renderSummary();
    updateActions();
  }
});

/* ───────────── 패널 ───────────── */
function openPanel(name) {
  closePanels();
  hidePopovers();
  $('#scrim').hidden = false;
  $(`#panel-${name}`).hidden = false;
  if (name === 'settings') renderSettings();
  if (name === 'history') renderHistory();
}
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
  if (e.key === 'Escape' && !hotkeyCapturing) {
    if (!$('#findbar').hidden && $$('.panel').every((p) => p.hidden)) closeFind();
    closePanels();
    hidePopovers();
  }
  if (e.ctrlKey && !e.altKey && (e.key === 'f' || e.key === 'F')) { e.preventDefault(); openFind(); }
  const typing = e.target.closest?.('input, textarea, select, [contenteditable="true"]');
  if (e.code === 'Space' && !typing && !e.ctrlKey && !e.altKey && P.src && !S.mini) { e.preventDefault(); togglePlay(); }
  if (e.ctrlKey && (e.key === '=' || e.key === '+')) { e.preventDefault(); zoom(1); }
  if (e.ctrlKey && e.key === '-') { e.preventDefault(); zoom(-1); }
  if (e.ctrlKey && e.key === '0') { e.preventDefault(); zoom(0); }
});
document.addEventListener('wheel', (e) => { if (e.ctrlKey) { e.preventDefault(); zoom(e.deltaY < 0 ? 1 : -1); } }, { passive: false });
document.addEventListener('click', (e) => {
  const dl = e.target.closest('[data-dl]');
  if (dl) queueDownloads(dl.dataset.dl.split(','));
  if (e.target.closest('[data-open-settings]')) openPanel('settings');
  const url = e.target.closest('[data-url]');
  if (url) { e.preventDefault(); call('open_url', { url: url.dataset.url }); }
});

/* 기록 */
async function renderHistory() {
  const q = $('#history-q').value.trim();
  const list = q ? await call('search_sessions', { query: q }) : await call('list_sessions');
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
  box.innerHTML = list.map((m) => {
    const dd = m.startedAt.slice(0, 10);
    const head = dd !== day ? `<div class="h-day">${dd.replace(/-/g, '. ')}</div>` : '';
    day = dd;
    return `${head}<div class="h-item" role="button" data-id="${esc(m.id)}">
      <div class="h-title"><span>${mark(m.title)}${m.hits ? `<span class="h-badge">${m.hits}곳</span>` : ''}</span>${m.id === liveId ? '<span class="h-live">● 녹음 중</span>' : `<button class="icon-btn sm" data-del="${esc(m.id)}" title="삭제"><svg><use href="#i-trash"/></svg></button>`}</div>
      <div class="h-sub">${m.startedAt.slice(11)} · ${fmt(m.durationMs)} · 화자 ${m.speakers}명 · 발화 ${m.count}개</div>
      ${m.snippet ? `<div class="h-snip">${mark(m.snippet)}</div>` : `<div class="h-prev">${esc(m.preview)}</div>`}
    </div>`;
  }).join('');
}
$('#history-list').addEventListener('click', async (e) => {
  const del = e.target.closest('[data-del]');
  if (del) {
    e.stopPropagation();
    const ok = await api.ask('이 회의 기록을 삭제할까요?\n(녹취록·요약·녹음 파일이 함께 지워지며 되돌릴 수 없어요)', { title: '받아쓰기', kind: 'warning' });
    if (ok) { await call('delete_session', { id: del.dataset.del }); renderHistory(); }
    return;
  }
  const item = e.target.closest('.h-item');
  if (!item) return;
  const id = item.dataset.id;
  if (S.liveDoc && id === S.liveDoc.id) { S.live = true; S.viewDoc = null; }
  else { S.viewDoc = await call('load_session', { id }); S.live = false; }
  const q = $('#history-q').value.trim();
  closePanels();
  renderDock();
  renderDoc();
  body.scrollTop = 0;
  if (q) openFind(q);
});
$('#btn-open-folder').addEventListener('click', () => call('open_folder', { kind: 'sessions' }));
$('#history-q').addEventListener('input', debounce(renderHistory, 220));
$('#btn-import').addEventListener('click', () => pickFile());
$('#btn-open-file').addEventListener('click', () => pickFile());

/* ───────────── 파일 받아쓰기 ───────────── */
const exts = () => S.app?.audioExts || ['wav', 'mp3', 'm4a', 'mp4', 'flac', 'ogg'];
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
    if (/모델|API 키/.test(String(e))) openPanel('settings');
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
const P = { key: null, src: null, until: null, curId: null, started: false, raf: 0, seeking: false, followId: null, live: false, wantPlay: false, reloading: null };
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
  P.reloading = new Promise((resolve) => {
    let to;
    const done = () => {
      audio.removeEventListener('loadedmetadata', done);
      clearTimeout(to);
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
function markPlaying(follow) {
  $$('#transcript .u.playing').forEach((el) => el.classList.remove('playing'));
  const d = doc();
  if (!P.src || !P.started || !d) return;
  const t = audio.currentTime * 1000;
  const u = d.utterances.find((x) => t >= x.startMs - 200 && t <= x.endMs + 300);
  const el = u && $(`#transcript .u[data-id="${u.id}"]`);
  if (!el) return;
  el.classList.add('playing');
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
  await ensureFresh(u.endMs / 1000 + 0.3);
  audio.currentTime = Math.max(0, u.startMs / 1000 - 0.15);
  P.until = u.endMs / 1000 + 0.2;
  audio.play().catch((e) => toast(`재생할 수 없어요: ${e.message || e}`, 'error'));
}
audio.addEventListener('play', () => { P.started = true; cancelAnimationFrame(P.raf); P.raf = requestAnimationFrame(tick); });
audio.addEventListener('pause', () => { renderPlayer(); markPlaying(false); });
audio.addEventListener('timeupdate', checkUntil); // 창이 숨겨져 rAF 가 멈춰도 문장 끝에서 멈추게
audio.addEventListener('loadedmetadata', renderPlayer);
audio.addEventListener('ended', async () => {
  P.until = null;
  renderPlayer();
  // 녹음 중에 이어 듣다 끝(지금 시각)에 닿으면, 새로 녹음된 만큼 다시 읽어 계속 따라간다
  while (P.live && P.wantPlay && P.src) {
    const t = audio.currentTime;
    await new Promise((r) => setTimeout(r, 700));
    if (!P.live || !P.wantPlay) break;
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
  if (S.rec === 'recording' && S.paused) cap.textContent = '일시정지 중 — 이 동안은 받아 적지 않아요';
  else if (last && S.rec !== 'idle') cap.innerHTML = `<b>${esc(speakerName(d, resolveSpk(d, last.speaker)))}</b>${esc(last.text)}`;
  else if (S.rec === 'starting') cap.textContent = '준비하는 중…';
  else if (S.rec === 'stopping') cap.textContent = '남은 발화를 정리하는 중…';
  else if (S.rec === 'recording') cap.textContent = S.speaking ? '듣는 중…' : '말씀하시면 받아 적어요';
  else cap.textContent = `${S.settings?.hotkey || ''} 로 녹음 · 더블클릭하면 큰 창`;
}
$('#btn-mini').addEventListener('click', () => call('set_mini', { mini: true }));
$('#btn-expand').addEventListener('click', () => call('set_mini', { mini: false }));
$('#btn-mini-hide').addEventListener('click', () => api.invoke('hide_window'));
// 미니 창 도크 빈 곳 더블클릭 → 큰 창 (끌기와 겹치지 않게 mousedown 의 두 번째 클릭으로 판단)
$('.dock').addEventListener('mousedown', (e) => {
  if (S.mini && e.detail === 2 && e.target.closest('[data-tauri-drag-region]')) call('set_mini', { mini: false });
});

/* ───────────── 일시정지 ───────────── */
$('#btn-pause').addEventListener('click', () => call('set_paused', { paused: !S.paused }));
function onPaused(p) {
  S.paused = p;
  if (p) { S.speaking = false; resetLevel(); }
  renderDock();
  scheduleRender();
  toast(p ? '일시정지했어요. 이 동안의 소리는 받아 적지 않아요.' : '이어서 받아 적어요');
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
    $('#update-label').textContent = `새 버전 v${u.version} 준비됨${u.notes ? ` · ${u.notes.split('\n')[0].replace(/^[#\-\s]+/, '')}` : ''}`;
    btn.hidden = false;
    btn.textContent = '지금 적용';
  } else if (u.state === 'installing') {
    $('#update-label').textContent = `v${u.version} 설치 중… 잠시 뒤 다시 열려요`;
    btn.hidden = true;
  } else {
    bar.hidden = true;
  }
}
function onUpdate(u) {
  S.update = u;
  if (u?.state === 'error') toast(u.message || '업데이트 확인 실패', 'error');
  renderUpdate();
}
$('#btn-update-now').addEventListener('click', async () => {
  if (S.rec !== 'idle') return toast('녹음을 마친 뒤에 적용할 수 있어요', 'error');
  try { await api.invoke('install_update'); } catch (e) { toast(String(e), 'error'); }
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
  setSeg('seg-engine', s.engine);
  $$('[data-engine]').forEach((el) => (el.hidden = el.dataset.engine !== s.engine));
  $('#engine-desc').textContent = ENGINE_DESC[s.engine];
  $('#opt-beam').checked = s.beamSize >= 2;
  $('#opt-openai-model').value = s.openaiModel;
  $('#opt-groq-model').value = s.groqModel;
  $('#opt-custom-url').value = s.customUrl;
  $('#opt-custom-model').value = s.customModel;
  for (const [k, id] of KEY_INPUTS.map(([id, , k]) => [k, id])) {
    const el = $(`#${id}`);
    el.value = '';
    el.placeholder = S.app.keys[k] ? '●●●●●●●● (저장됨 · 바꾸려면 입력)' : el.dataset.ph || el.placeholder;
    el.dataset.ph = el.dataset.ph || el.placeholder;
  }
  renderModels();

  $('#opt-diar').checked = s.diarization.enabled;
  $('#opt-thr').value = s.diarization.threshold;
  $('#thr-label').textContent = s.diarization.threshold.toFixed(2);
  $('#opt-maxspk').value = String(s.diarization.maxSpeakers);

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

  $('#opt-hotkey').value = s.hotkey || '없음';
  $('#opt-top').checked = s.alwaysOnTop;
  $('#opt-autostart').checked = S.app.autostart;
  $('#opt-audio').checked = s.saveAudio;
  $('#opt-unload').checked = s.unloadWhenIdle;
  $('#opt-autoupdate').checked = s.autoUpdate;
  $('#opt-theme').value = s.theme;
  $('#app-version').textContent = `v${S.app.version}`;
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
    if (ok) { await call('delete_model', { id: del.dataset.delmodel }); await refreshState(); renderSettings(); }
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
bind('opt-diar', (t) => { S.settings.diarization.enabled = t.checked; });
bind('opt-thr', (t) => { S.settings.diarization.threshold = +t.value; $('#thr-label').textContent = (+t.value).toFixed(2); }, 'input');
bind('opt-maxspk', (t) => (S.settings.diarization.maxSpeakers = +t.value));
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
bind('opt-autoupdate', (t) => (S.settings.autoUpdate = t.checked));
bind('opt-theme', (t) => { S.settings.theme = t.value; applyTheme(); });
$('#opt-autostart').addEventListener('change', (e) => call('set_autostart', { enabled: e.target.checked }).then(() => (S.app.autostart = e.target.checked)));

for (const [id, name] of KEY_INPUTS) {
  $(`#${id}`).addEventListener('change', async (e) => {
    const v = e.target.value.trim();
    if (!v) return;
    await call('set_api_key', { name, value: v });
    e.target.value = '';
    await refreshState();
    renderSettings();
    toast('API 키를 안전하게 저장했어요');
  });
}

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

// 단축키 입력
let hotkeyCapturing = false;
const hk = $('#opt-hotkey');
function stopHotkeyCapture() { hotkeyCapturing = false; hk.classList.remove('capturing'); if (S.settings) hk.value = S.settings.hotkey || '없음'; }
hk.addEventListener('click', () => { hotkeyCapturing = true; hk.classList.add('capturing'); hk.value = '조합키를 누르세요 (Esc 취소 · Backspace 해제)'; });
hk.addEventListener('blur', stopHotkeyCapture);
hk.addEventListener('keydown', (e) => {
  if (!hotkeyCapturing) return;
  e.preventDefault();
  if (e.key === 'Escape') return stopHotkeyCapture();
  if (e.key === 'Backspace') { S.settings.hotkey = ''; saveSettings(); return stopHotkeyCapture(); }
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
  if (!key) return;
  if (!mods.length && !/^F\d/.test(key)) { hk.value = 'Ctrl·Alt·Shift 와 함께 눌러 주세요'; return; }
  S.settings.hotkey = [...mods, key].join('+');
  saveSettings();
  stopHotkeyCapture();
  hk.blur();
  toast(`단축키: ${S.settings.hotkey}`);
});

$('#btn-github').addEventListener('click', () => call('open_url', { url: 'https://github.com/jeiel85/badasugi' }));
$('#btn-folder-sessions').addEventListener('click', () => call('open_folder', { kind: 'sessions' }));
$('#btn-folder-models').addEventListener('click', () => call('open_folder', { kind: 'models' }));
$('#btn-quit').addEventListener('click', () => call('quit_app'));

/* ───────────── 시작 ───────────── */
(async function init() {
  await api.listen('bd://event', (e) => onEvent(e.payload));
  await api.listen('bd://rec', (e) => onRec(e.payload));
  await api.listen('bd://download', (e) => onDownload(e.payload));
  await api.listen('bd://toast', (e) => toast(String(e.payload), 'error'));
  await api.listen('bd://paused', (e) => onPaused(!!e.payload));
  await api.listen('bd://mini', (e) => setMini(!!e.payload));
  await api.listen('bd://update', (e) => onUpdate(e.payload));
  await refreshState();
  $('#drop-exts').textContent = exts().join(' · ');
  renderUpdate();
  if (S.app.hotkeyError) setTimeout(() => toast(S.app.hotkeyError, 'warn'), 600);
  if (S.app.updatedFrom) toast(`v${S.app.version}(으)로 업데이트했어요`);
  if (S.app.demo) loadDemo();
  if (S.app.downloading) S.downloads[S.app.downloading] = { done: 0, total: 1 };
  renderDock();
  renderDoc();
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

function createMock() {
  const handlers = {};
  const st = {
    settings: {
      engine: 'local', localModel: 'large-v3-turbo-q5_0', beamSize: 1, threads: 0,
      openaiModel: 'gpt-4o-transcribe', groqModel: 'whisper-large-v3-turbo', customUrl: '', customModel: '',
      source: 'mic', micDevice: null, language: 'ko', glossary: '',
      diarization: { enabled: true, threshold: 0.45, maxSpeakers: 0, splitTurns: true },
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
    audioExts: ['wav', 'mp3', 'm4a', 'mp4', 'mov', 'aac', 'flac', 'ogg', 'oga'],
    demo: new URLSearchParams(location.search).has('demo'),
  };
  return {
    invoke: async (cmd, args) => {
      if (cmd === 'get_state') return st;
      if (cmd === 'save_settings') { st.settings = args.settings; return; }
      if (cmd === 'list_devices') return [{ id: 'mic1', name: '마이크(Realtek Audio)', isDefault: true }];
      if (cmd === 'search_sessions') return [];
      if (cmd === 'session_audio') return demoWavUrl();
      if (cmd === 'set_mini') { st.settings.miniMode = args.mini; handlers['bd://mini'] && handlers['bd://mini']({ payload: args.mini }); return; }
      if (cmd === 'list_sessions') return [{ id: 'demo', title: '앱 출시 일정 회의', startedAt: '2026-11-12 14:30', durationMs: 61000, count: 7, speakers: 3, preview: '자, 그럼 시작할게요. 오늘은 다음 달 앱 출시 일정부터 확정하죠.' }];
      if (cmd === 'load_session') return demoDoc();
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
