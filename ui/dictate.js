// 말로 입력 알림 — 앱(dictate.rs)이 보낸 상태를 그린다. 누를 단추는 없다 (포커스를 가져가지 않는 창)
'use strict';

const $ = (s) => document.querySelector(s);
const darkMq = matchMedia('(prefers-color-scheme: dark)');
const api = window.__TAURI__
  ? { invoke: window.__TAURI__.core.invoke, listen: window.__TAURI__.event.listen }
  : mockApi();

function applyTheme() {
  document.documentElement.dataset.theme = darkMq.matches ? 'dark' : 'light';
}
darkMq.addEventListener('change', applyTheme);
applyTheme();

const TEXT = {
  listening: (v) => (v.latched ? '듣는 중 · 다시 누르거나 말을 멈추면 입력해요' : '듣는 중 · 놓으면 입력해요'),
  working: () => '받아 적는 중…',
  polishing: () => 'AI 로 다듬는 중…',
  done: (v) => (v.missed ? `입력했어요 · ${v.chars}자 — 한 부분(${v.missed}곳)은 받아 적지 못했어요` : `입력했어요 · ${v.chars}자`),
  error: (v) => v.message,
};

const bars = [...document.querySelectorAll('#dict-lv i')];
function level(rms) {
  const db = 20 * Math.log10((rms || 0) + 1e-6);
  const v = Math.min(1, Math.max(0, (db + 58) / 46));
  const shape = [0.5, 0.8, 1, 0.8, 0.5];
  bars.forEach((b, i) => (b.style.height = `${3 + v * 15 * shape[i] * (0.75 + Math.random() * 0.5)}px`));
}

function render(v) {
  if (!v || v.state === 'hidden') return;
  const box = $('#dict');
  box.dataset.state = v.state;
  $('#dict-txt').textContent = (TEXT[v.state] || (() => ''))(v);
  if (v.state !== 'listening') level(0);
}

(async () => {
  await api.listen('bd://dictate', (e) => render(e.payload));
  await api.listen('bd://dictate-level', (e) => level(e.payload));
  render(await api.invoke('dictate_state'));
  await api.invoke('dictate_ready').catch(() => {});
})();
document.addEventListener('contextmenu', (e) => e.preventDefault());

/* 브라우저 미리보기: dictate.html?state=listening|working|polishing|done|error&latched=1 */
function mockApi() {
  const q = new URLSearchParams(location.search);
  const state = q.get('state') || 'listening';
  const v = { state, latched: q.has('latched'), chars: 42, missed: q.has('missed') ? 1 : 0, message: '회의를 받아 적는 중이라 말로 입력은 쉬어요 — 내 말이 회의 기록에 섞이지 않게' };
  let onLevel = null;
  setInterval(() => onLevel && state === 'listening' && onLevel({ payload: 0.02 + Math.random() * 0.12 }), 90);
  return {
    listen: async (ev, h) => { if (ev === 'bd://dictate-level') onLevel = h; },
    invoke: async (cmd) => (cmd === 'dictate_state' ? v : null),
  };
}
