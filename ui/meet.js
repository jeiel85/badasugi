// 회의 시작 알림 창 — 앱(meeting.rs)이 보낸 내용을 그리고, 누른 단추를 돌려준다.
// 창은 내용을 그린 뒤 meet_ready 를 부르면 그때 (포커스를 빼앗지 않고) 보인다.
'use strict';

const $ = (s) => document.querySelector(s);
const $$ = (s) => document.querySelectorAll(s);
// 받을 소리 (큰 창의 입력 이름과 같게)
const SOURCE_BY = { mic: '마이크로', system: '시스템 소리로', both: '마이크 + 시스템 소리로' };
const darkMq = matchMedia('(prefers-color-scheme: dark)');
const api = window.__TAURI__
  ? { invoke: window.__TAURI__.core.invoke, listen: window.__TAURI__.event.listen }
  : mockApi();
let theme = 'system';
let busy = false;

function applyTheme() {
  document.documentElement.dataset.theme = theme === 'system' ? (darkMq.matches ? 'dark' : 'light') : theme;
}
darkMq.addEventListener('change', applyTheme);

/** 12:34 · 1:02:03 */
function clock(ms) {
  const s = Math.floor(ms / 1000);
  const p = (n) => String(n).padStart(2, '0');
  const h = Math.floor(s / 3600);
  return h ? `${h}:${p(Math.floor(s / 60) % 60)}:${p(s % 60)}` : `${p(Math.floor(s / 60))}:${p(s % 60)}`;
}

function render(v) {
  theme = v.theme || 'system';
  applyTheme();
  const p = v.prompt;
  if (!p) return false;
  let title, desc, acts;
  if (p.kind === 'start') {
    // Outlook 일정에서 가져오기를 켰으면 지금 일정의 제목을 보인다
    title = v.event || '회의가 시작된 것 같아요';
    desc = `${v.event ? '회의가 시작된 것 같아요 · ' : ''}${p.app}에서 마이크를 쓰고 있어요 · ${SOURCE_BY[v.source] || SOURCE_BY.mic} 받아 적어요`;
    acts = [['start', '받아쓰기 시작'], ['close', '이번엔 안 함']];
  } else if (p.kind === 'started') {
    title = '받아 적고 있어요';
    desc = v.event ? `${v.event} · 회의가 끝나면 멈출지 물어볼게요` : '회의가 끝나면 멈출지 물어볼게요';
    acts = [['open', '창 열기']];
  } else {
    title = '회의가 끝난 것 같아요';
    desc = `${p.app}에서 마이크를 놓았어요 · 받아 적은 시간 ${clock(v.elapsedMs || 0)}`;
    acts = [['stop', '멈추기'], ['close', '계속 받아쓰기']];
  }
  $('#meet-title').textContent = title;
  $('#meet-desc').textContent = desc;
  const box = $('#meet-acts');
  box.replaceChildren(...acts.map(([action, label], i) => {
    const b = document.createElement('button');
    b.className = i ? 'btn ghost sm' : 'btn sm';
    b.dataset.action = action;
    b.textContent = label;
    return b;
  }));
  $('#meet').hidden = false;
  busy = false;
  return true;
}

async function show(v) {
  if (render(v)) await api.invoke('meet_ready').catch(() => {});
}

async function answer(action) {
  if (busy) return;
  busy = true;
  $$('#meet button').forEach((b) => (b.disabled = true));
  try {
    await api.invoke('meet_answer', { action });
  } catch {
    // 시작하지 못한 까닭은 앱이 큰 창에 알린다 — 이 창은 앱이 닫는다
  }
  // 창이 남아 있으면(다음 내용이 왔으면) 다시 누를 수 있게
  $$('#meet button').forEach((b) => (b.disabled = false));
  busy = false;
}

$('#meet-acts').addEventListener('click', (e) => {
  const b = e.target.closest('button[data-action]');
  if (b) answer(b.dataset.action);
});
$('#meet-close').addEventListener('click', () => answer('close'));
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') answer('close'); });
document.addEventListener('contextmenu', (e) => e.preventDefault());

(async () => {
  await api.listen('bd://meet', (e) => show(e.payload));
  show(await api.invoke('meet_state'));
})();

/* 브라우저 미리보기: meet.html?kind=start|started|end&app=Zoom&source=both&theme=dark */
function mockApi() {
  const q = new URLSearchParams(location.search);
  let handler = null;
  const view = (kind) => ({
    prompt: kind === 'none' ? null : { kind, app: q.get('app') || 'Zoom' },
    source: q.get('source') || 'both',
    elapsedMs: 3723000,
    theme: q.get('theme') || 'system',
    event: q.get('event') || null,
  });
  return {
    listen: async (_ev, h) => { handler = h; },
    invoke: async (cmd, args) => {
      if (cmd === 'meet_state') return view(q.get('kind') || 'start');
      if (cmd === 'meet_answer') {
        window.lastAnswer = args.action;
        await new Promise((r) => setTimeout(r, 300));
        const next = args.action === 'start' ? 'started' : 'none';
        if (next === 'none') $('#meet').hidden = true;
        else handler && handler({ payload: view(next) });
      }
      return null;
    },
  };
}
