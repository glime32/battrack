'use strict';
// Veille auto : sur batterie, endort les onglets en arrière-plan qui mangent du CPU.
// Réveillé une fois par minute par chrome.alarms, rien entre deux passages.
// Sur secteur, le passage s'arrête dès la lecture de l'état de la batterie.

const HOST = 'com.glime.battrack';
const SEUIL = 4; // % d'un cœur, en moyenne sur la dernière minute
const FOIS = 2; // passages de suite au-dessus du seuil avant la veille
const QUITTE = 2 * 60e3; // onglet quitté depuis au moins 2 min
const PERIODE = 1; // minutes entre deux passages
const GARDER = 8; // entrées gardées dans le journal du panneau
const EXCEPTIONS = ['claude.ai', 'meet.google.com', 'zoom.us', 'discord.com', 'music.youtube.com', 'soundcloud.com'];

const site = (url) => {
  try {
    return new URL(url).hostname.replace(/^www\./, '');
  } catch {
    return '';
  }
};
const excepte = (h, liste) => liste.some((e) => h === e || h.endsWith('.' + e));

async function reglages() {
  const { auto } = await chrome.storage.local.get('auto');
  if (auto) return auto;
  const neuf = { on: true, except: EXCEPTIONS, log: [] };
  await chrome.storage.local.set({ auto: neuf });
  return neuf;
}

async function armer() {
  const a = await reglages();
  if (!a.on) return chrome.alarms.clear('veille');
  const deja = await chrome.alarms.get('veille');
  if (!deja) chrome.alarms.create('veille', { periodInMinutes: PERIODE });
}

chrome.runtime.onInstalled.addListener(armer);
chrome.runtime.onStartup.addListener(armer);
chrome.storage.onChanged.addListener((ch, zone) => {
  if (zone === 'local' && ch.auto && ch.auto.newValue?.on !== ch.auto.oldValue?.on) armer();
});
chrome.alarms.onAlarm.addListener((al) => {
  if (al.name === 'veille') passage().catch(() => {});
});

// CPU d'un processus sur la dernière minute : écart de ticks avec le passage
// précédent, sinon la moyenne 1 min du démon battrack.
function cpuDe(p, avant, s) {
  const q = avant?.tk[p.pid];
  const dt = avant ? (s.t - avant.t) / 1000 : 0;
  if (q != null && dt > 20 && dt < 300) return Math.max(0, ((p.tk - q) / s.hz / dt) * 100);
  return typeof p.m1 === 'number' ? p.m1 : null;
}

async function passage() {
  const a = await reglages();
  if (!a.on) return;
  const s = await chrome.runtime.sendNativeMessage(HOST, { cmd: 'snap' });
  if (!s || !s.procs) return;
  const { avant = null, coups = {} } = await chrome.storage.session.get(['avant', 'coups']);
  const tk = {};
  for (const p of s.procs) tk[p.pid] = p.tk;
  await chrome.storage.session.set({ avant: { t: s.t, tk } });

  if (!s.bat) {
    if (Object.keys(coups).length) await chrome.storage.session.set({ coups: {} });
    return;
  }

  const parCid = new Map();
  for (const p of s.procs) if (p.cid) parCid.set(p.cid, cpuDe(p, avant, s));

  const onglets = await chrome.tabs.query({});
  const cids = new Map();
  const partage = new Map();
  for (const t of onglets) {
    if (t.discarded) continue;
    let fr = [];
    try {
      fr = (await chrome.webNavigation.getAllFrames({ tabId: t.id })) || [];
    } catch {}
    const l = [...new Set(fr.map((f) => f.processId).filter((x) => x > 0))];
    cids.set(t.id, l);
    for (const c of l) partage.set(c, (partage.get(c) || 0) + 1);
  }

  const maintenant = Date.now();
  const suite = {};
  const endormis = [];
  for (const t of onglets) {
    if (t.discarded || t.active || t.audible || t.pinned || t.autoDiscardable === false) continue;
    if (!/^https?:/.test(t.url || '')) continue;
    if (t.lastAccessed && maintenant - t.lastAccessed < QUITTE) continue;
    const h = site(t.url);
    if (excepte(h, a.except)) continue;
    let cpu = null;
    for (const c of cids.get(t.id) || []) {
      const v = parCid.get(c);
      if (v != null) cpu = (cpu ?? 0) + v / (partage.get(c) || 1);
    }
    if (cpu == null || cpu < SEUIL) continue;
    suite[t.id] = (coups[t.id] || 0) + 1;
    if (suite[t.id] < FOIS) continue;
    try {
      await chrome.tabs.discard(t.id);
      delete suite[t.id];
      endormis.push({ site: h, titre: t.title || h, cpu: Math.round(cpu * 10) / 10, quand: maintenant });
    } catch {}
  }
  await chrome.storage.session.set({ coups: suite });
  if (endormis.length) {
    const b = await reglages();
    b.log = [...endormis, ...(b.log || [])].slice(0, GARDER);
    await chrome.storage.local.set({ auto: b });
  }
}
