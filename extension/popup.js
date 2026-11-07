'use strict';
// Panneau "conso des onglets". Ne tourne que quand le panneau est ouvert :
// aucune tâche de fond, aucun service worker. Les chiffres viennent de
// battrack (hôte natif) qui lit /proc ; l'historique 10 min / 1 h vient du démon.

const HOST = 'com.glime.battrack';
const $ = (s) => document.querySelector(s);

let port = null;
let prev = null;
let snap = null;
let tabs = [];
let frames = new Map(); // tabId -> [renderer-client-id]
let mapAt = 0;
let mapping = false;
let sortKey = 'live';
const icons = new Map();

const LABEL = {
  main: 'Navigateur (interface)',
  gpu: 'GPU (affichage, vidéo)',
  network: 'Réseau',
  audio: 'Audio',
  storage: 'Stockage',
  zygote: 'Zygote',
  ext: 'Extensions',
  renderer: 'Autres pages (workers, préchargement…)',
};
const SHORT = { main: 'ui', gpu: 'gpu', network: 'net', audio: 'audio', storage: 'stock', zygote: 'zyg', ext: 'ext', renderer: 'autres' };

const fmt = (v) => (v == null ? '–' : v < 0.05 ? '0' : v < 10 ? v.toFixed(1) : v.toFixed(0));
const mb = (kb) => (kb >= 1048576 ? (kb / 1048576).toFixed(1) + ' G' : Math.round(kb / 1024) + ' M');
const tone = (v) => (v == null ? 'dim' : v >= 20 ? 'hi' : v >= 5 ? 'mid' : '');
const add = (acc, v, k) => (v == null ? acc : (acc ?? 0) + v / k);

function host(url) {
  try {
    const u = new URL(url);
    return u.protocol.startsWith('http') ? u.hostname.replace(/^www\./, '') : u.protocol + '//' + u.hostname;
  } catch {
    return (url || '').slice(0, 40);
  }
}

function msg(text, info) {
  const m = $('#msg');
  m.textContent = text || '';
  m.hidden = !text;
  m.className = 'msg' + (info ? ' info' : '');
}

/* ---------- lien avec battrack ---------- */

function connect() {
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch (e) {
    msg('Impossible de joindre battrack : ' + e.message);
    return;
  }
  port.onMessage.addListener(onSnap);
  port.onDisconnect.addListener(() => {
    const err = chrome.runtime.lastError?.message || 'déconnecté';
    msg('battrack injoignable (' + err + '). Lance ~/battrack/install.sh puis rouvre ce panneau.');
    port = null;
  });
  poll();
  setInterval(poll, 2000);
}

function poll() {
  if (!port) return;
  try {
    port.postMessage({ cmd: 'snap' });
  } catch {}
}

async function refreshTabs() {
  if (mapping) return;
  mapping = true;
  try {
    tabs = await chrome.tabs.query({});
    const m = new Map();
    await Promise.all(
      tabs.map(async (t) => {
        if (t.discarded) return m.set(t.id, []);
        try {
          const fr = await chrome.webNavigation.getAllFrames({ tabId: t.id });
          m.set(t.id, [...new Set((fr || []).map((f) => f.processId).filter((x) => x > 0))]);
        } catch {
          m.set(t.id, []);
        }
      })
    );
    frames = m;
    mapAt = Date.now();
  } finally {
    mapping = false;
  }
}

async function onSnap(s) {
  prev = snap;
  snap = s;
  const old = new Map(prev ? prev.procs.map((p) => [p.pid, p]) : []);
  const dt = prev ? (s.t - prev.t) / 1000 : 0;
  for (const p of s.procs) {
    const q = old.get(p.pid);
    p.live = q && dt > 0.3 ? Math.max(0, ((p.tk - q.tk) / s.hz / dt) * 100) : null;
  }
  // puissance RAPL (µJ cumulés) : psys = PC entier, pkg = CPU+GPU+RAM
  const joules = (a, b) => (a >= 0 && b >= 0 ? (a - b < 0 ? a - b + (s.rmax || 0) : a - b) / 1e6 : null);
  if (prev && dt > 0.3) {
    const jp = joules(s.pkg, prev.pkg);
    const js = joules(s.psys, prev.psys);
    s.pkgW = jp == null ? null : jp / dt;
    s.psysW = js == null ? null : js / dt;
  }
  if (Date.now() - mapAt > 8000) await refreshTabs();
  render();
}

/* ---------- modèle : onglet = somme de ses processus ---------- */

function model() {
  const s = snap;
  const byCid = new Map();
  for (const p of s.procs) if (p.cid) byCid.set(p.cid, p);
  const share = new Map();
  for (const cids of frames.values()) for (const c of cids) if (byCid.has(c)) share.set(c, (share.get(c) || 0) + 1);

  const used = new Set();
  const rows = tabs.map((t) => {
    const r = { t, live: null, m10: null, m60: null, rss: 0, shared: false };
    for (const c of frames.get(t.id) || []) {
      const p = byCid.get(c);
      if (!p) continue;
      used.add(c);
      const k = share.get(c) || 1;
      if (k > 1) r.shared = true;
      r.live = add(r.live, p.live, k);
      r.m10 = add(r.m10, p.m10, k);
      r.m60 = add(r.m60, p.m60, k);
      r.rss += p.rss / k;
    }
    return r;
  });

  const groups = new Map();
  const tot = { live: null, m10: null, m60: null, rss: 0 };
  for (const p of s.procs) {
    tot.live = add(tot.live, p.live, 1);
    tot.m10 = add(tot.m10, p.m10, 1);
    tot.m60 = add(tot.m60, p.m60, 1);
    tot.rss += p.rss;
    if (p.cid && used.has(p.cid)) continue;
    let g = groups.get(p.ty);
    if (!g) groups.set(p.ty, (g = { ty: p.ty, label: LABEL[p.ty] || p.ty, short: SHORT[p.ty] || p.ty, n: 0, live: null, m10: null, m60: null, rss: 0 }));
    g.n++;
    g.live = add(g.live, p.live, 1);
    g.m10 = add(g.m10, p.m10, 1);
    g.m60 = add(g.m60, p.m60, 1);
    g.rss += p.rss;
  }
  const gl = [...groups.values()].filter((g) => g.ty !== 'zygote' || (g.live ?? 0) > 0.05);
  gl.sort((a, b) => (b.live ?? b.m10 ?? 0) - (a.live ?? a.m10 ?? 0));
  return { rows, groups: gl, tot, mapped: used.size };
}

const sortVal = (r, k) => r[k] ?? (k === 'live' ? r.m10 : null);

function sortRows(rows) {
  const k = sortKey;
  rows.sort((a, b) => {
    if (k === 'title') return (a.t.title || '').localeCompare(b.t.title || '');
    const va = sortVal(a, k);
    const vb = sortVal(b, k);
    if (va == null && vb == null) return a.t.discarded - b.t.discarded;
    if (va == null) return 1;
    if (vb == null) return -1;
    return vb - va;
  });
}

/* ---------- affichage ---------- */

function favicon(t) {
  let img = icons.get(t.id);
  const src = chrome.runtime.getURL('/_favicon/?pageUrl=' + encodeURIComponent(t.url || '') + '&size=16');
  if (!img) {
    img = document.createElement('img');
    img.alt = '';
    icons.set(t.id, img);
  }
  if (img.dataset.src !== src) {
    img.src = src;
    img.dataset.src = src;
  }
  return img;
}

function flagsOf(t) {
  return (t.audible ? '🔊' : '') + (t.pinned ? '📌' : '') + (t.discarded ? '💤' : '') + (t.frozen ? '❄️' : '');
}

function num(v, live, shared) {
  const td = document.createElement('td');
  td.textContent = fmt(v) + (shared && v != null ? ' ≈' : '');
  const c = tone(v);
  if (live) {
    td.classList.add('live');
    td.style.setProperty('--w', Math.min(100, v ?? 0) + '%');
  }
  if (c) td.classList.add(c);
  return td;
}

function cell(text, cls) {
  const td = document.createElement('td');
  td.textContent = text;
  if (cls) td.className = cls;
  return td;
}

function tabRow(r) {
  const t = r.t;
  const tr = document.createElement('tr');
  if (t.discarded) tr.className = 'disc';

  const td = document.createElement('td');
  td.className = 'l';
  const row = document.createElement('div');
  row.className = 'row';
  const tt = document.createElement('span');
  tt.className = 'tt';
  tt.textContent = t.title || t.url || '(sans titre)';
  tt.title = 'Aller à l’onglet\n' + (t.url || '');
  tt.addEventListener('click', () => {
    chrome.tabs.update(t.id, { active: true });
    chrome.windows.update(t.windowId, { focused: true });
  });
  const dom = document.createElement('span');
  dom.className = 'dom';
  dom.textContent = host(t.url);
  const fl = document.createElement('span');
  fl.className = 'fl';
  fl.textContent = flagsOf(t);
  row.append(favicon(t), tt, dom, fl);
  td.append(row);

  const act = document.createElement('td');
  act.className = 'act';
  const b = document.createElement('button');
  b.className = 'mini';
  b.textContent = '💤';
  b.disabled = t.discarded || t.active;
  b.title = t.discarded ? 'Déjà en veille' : t.active ? 'Onglet affiché : pas de veille' : 'Mettre en veille (libère CPU et RAM, recharge au clic)';
  b.setAttribute('aria-label', 'Mettre en veille ' + (t.title || ''));
  b.addEventListener('click', async () => {
    b.disabled = true;
    try {
      await chrome.tabs.discard(t.id);
    } catch (e) {
      msg('Veille impossible : ' + e.message);
    }
    mapAt = 0;
  });
  act.append(b);

  tr.append(td, num(r.live, true, r.shared), num(r.m10, false, r.shared), num(r.m60, false, r.shared), cell(r.rss ? mb(r.rss) : '–'), act);
  return tr;
}

function groupRow(g) {
  const tr = document.createElement('tr');
  const name = g.label + (g.n > 1 ? ` ×${g.n}` : '');
  tr.append(cell(name, 'l'), num(g.live, true), num(g.m10), num(g.m60), cell(mb(g.rss)), cell('', 'act'));
  return tr;
}

function render() {
  if (!snap) return;
  const s = snap;
  const m = model();
  sortRows(m.rows);

  const tb = document.createDocumentFragment();
  for (const r of m.rows) tb.append(tabRow(r));
  $('#tabs tbody').replaceChildren(tb);

  const ib = document.createDocumentFragment();
  for (const g of m.groups) ib.append(groupRow(g));
  $('#intern tbody').replaceChildren(ib);

  for (const th of document.querySelectorAll('#tabs th[data-k]')) th.classList.toggle('sorted', th.dataset.k === sortKey);

  const cpu = m.tot.live ?? m.tot.m10;
  $('#k-cpu').textContent = cpu == null ? '…' : fmt(cpu) + ' %';
  $('#k-cpu').className = tone(cpu);
  if (s.bat && s.pw > 0) {
    $('#k-pw').textContent = s.pw.toFixed(1) + ' W';
    $('#k-pw-l').textContent = `PC sur batterie · ${s.pct}%`;
  } else if (s.psysW != null) {
    $('#k-pw').textContent = s.psysW.toFixed(1) + ' W';
    $('#k-pw-l').textContent = `PC sur secteur · ${s.pct}%`;
  } else {
    $('#k-pw').textContent = 'secteur';
    $('#k-pw-l').textContent = s.pct >= 0 ? `PC · ${s.pct}%` : 'PC';
  }
  $('#k-pw').title = s.pkgW != null ? `dont CPU+GPU+RAM (package) : ${s.pkgW.toFixed(1)} W` : '';
  const nd = tabs.filter((t) => t.discarded).length;
  $('#k-tabs').textContent = String(tabs.length);
  $('#k-tabs-l').textContent = nd ? `onglets · ${nd} en veille` : 'onglets';
  $('#daemon').textContent = s.daemon ? 'historique : démon ✓' : 'démon arrêté → pas de 10 min / 1 h';

  const awake = tabs.filter((t) => !t.discarded).length;
  if (awake && !m.mapped && s.procs.some((p) => p.ty === 'renderer')) {
    msg('Onglets non reliés aux processus (Helium a peut-être changé). Copie le log et envoie-le à Claude.', true);
  }
}

/* ---------- log à copier ---------- */

function buildLog() {
  const s = snap;
  const { rows, groups, tot } = model();
  const d = new Date();
  const nd = tabs.filter((t) => t.discarded).length;
  const L = [];
  const when = d.toLocaleDateString('fr-FR') + ' ' + d.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' });
  const pkg = s.pkgW != null ? ` (pkg ${s.pkgW.toFixed(1)} W)` : '';
  const pc = (s.bat && s.pw > 0 ? s.pw.toFixed(1) + ' W batterie' : s.psysW != null ? s.psysW.toFixed(1) + ' W secteur' : 'secteur') + pkg;
  L.push(`### helium-tabs 1.1 · ${when} · ${tabs.length} onglets (${nd} en veille) · ${s.procs.length} proc · démon ${s.daemon ? 'oui' : 'NON'} · PC ${pc}`);
  L.push(`total Helium CPU%(1 cœur) live/10m/1h ${fmt(tot.live)}/${fmt(tot.m10)}/${fmt(tot.m60)} · RAM ${mb(tot.rss)}`);
  L.push('# live/10m/1h   RAM  site [a=audio p=épinglé v=affiché ≈=processus partagé]');
  const key = (r) => r.m10 ?? r.live ?? 0;
  const awake = rows.filter((r) => !r.t.discarded).sort((a, b) => key(b) - key(a));
  const top = awake.slice(0, 15);
  for (const r of top) {
    const fl = (r.t.audible ? 'a' : '') + (r.t.pinned ? 'p' : '') + (r.t.active ? 'v' : '') + (r.shared ? '≈' : '');
    L.push(`${fmt(r.live).padStart(5)}/${fmt(r.m10).padStart(4)}/${fmt(r.m60).padStart(4)} ${mb(r.rss).padStart(6)}  ${host(r.t.url)}${fl ? ' [' + fl + ']' : ''}`);
  }
  if (awake.length > top.length) L.push(`… +${awake.length - top.length} onglets plus calmes`);
  L.push('interne: ' + groups.map((g) => `${g.short}${g.n > 1 ? '×' + g.n : ''} ${fmt(g.live)}/${fmt(g.m10)}/${fmt(g.m60)}`).join(' · '));
  if (s.flags) L.push('flags gpu: ' + s.flags.slice(0, 300));
  L.push('### fin');
  return L.join('\n');
}

/* ---------- boutons ---------- */

$('#copy').addEventListener('click', async () => {
  const b = $('#copy');
  if (!snap) return msg('Pas encore de données, attends 2 s.', true);
  try {
    await navigator.clipboard.writeText(buildLog());
    b.textContent = '✓ Copié';
  } catch (e) {
    msg('Copie impossible : ' + e.message);
  }
  setTimeout(() => (b.textContent = '📋 Copier log'), 1500);
});

$('#sleep-all').addEventListener('click', async () => {
  const list = tabs.filter((t) => !t.active && !t.audible && !t.pinned && !t.discarded);
  let n = 0;
  for (const t of list) {
    try {
      await chrome.tabs.discard(t.id);
      n++;
    } catch {}
  }
  msg(n ? `${n} onglet(s) mis en veille. Ils se rechargent quand tu cliques dessus.` : 'Rien à mettre en veille.', true);
  mapAt = 0;
});

for (const th of document.querySelectorAll('#tabs th[data-k]')) {
  th.addEventListener('click', () => {
    sortKey = th.dataset.k;
    render();
  });
}

refreshTabs().then(connect);
