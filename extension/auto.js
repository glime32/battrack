'use strict';
// Veille auto, sur batterie comme sur secteur : endort les onglets en arrière-plan
// qui mangent du CPU, et quand Helium n'a plus le focus depuis 10 min, ceux qui
// travaillent encore un peu. Réveillé une fois par minute par chrome.alarms.
// Se recharge seule quand sa version change sur le disque.
// À chaque passage, l'hôte reçoit aussi « processus → sites » pour le relevé des réveils.

const HOST = 'com.glime.battrack';
const SEUIL = 4; // % d'un cœur, en moyenne sur la dernière minute
const FOIS = 2; // passages de suite au-dessus du seuil avant la veille
const QUITTE = 2 * 60e3; // onglet quitté depuis au moins 2 min
const FOND = 10 * 60e3; // Helium sans le focus depuis 10 min
const SEUIL_FOND = 0.05; // vrai 0 % : laissé ; 0,1 % : endormi
const SON = 5 * 60e3; // un onglet qui a joué du son il y a moins de 5 min est laissé
const PERIODE = 1; // minutes entre deux passages
const GARDER = 8; // entrées gardées dans le journal du panneau
const EXCEPTIONS = ['claude.ai', 'meet.google.com', 'zoom.us', 'discord.com', 'music.youtube.com', 'soundcloud.com'];
const MESSAGERIES = ['web.whatsapp.com', 'messages.google.com', 'instagram.com', 'snapchat.com', 'discord.com', 'messenger.com'];
const FOND_AUSSI = ['claude.ai']; // exceptions levées quand Helium est en fond
const CLAUDE = /^(?:⌛|🔔|✅)?\s*Claude$/u;

const site = (url) => {
  try {
    const u = new URL(url);
    return u.protocol === 'file:' ? 'fichier' : u.hostname.replace(/^www\./, '');
  } catch {
    return '';
  }
};
const excepte = (h, liste) => liste.some((e) => h === e || h.endsWith('.' + e));
const endormable = (url) => /^https?:/.test(url) || (url.startsWith('file:') && !/\/claude\/choix-[^/]*$/.test(url));

async function reglages() {
  const { auto } = await chrome.storage.local.get('auto');
  if (auto) return auto;
  const neuf = { on: true, except: EXCEPTIONS, log: [] };
  await chrome.storage.local.set({ auto: neuf });
  return neuf;
}

// les onglets Discord déjà ouverts reçoivent le script sans avoir à être rechargés
async function discord() {
  for (const t of await chrome.tabs.query({ url: 'https://discord.com/*' })) {
    if (t.discarded) continue;
    const target = { tabId: t.id };
    chrome.scripting.insertCSS({ target, files: ['discord.css'] }).catch(() => {});
    chrome.scripting.executeScript({ target, files: ['discord.js'] }).catch(() => {});
  }
}

async function aJour() {
  try {
    const r = await fetch(chrome.runtime.getURL('manifest.json'), { cache: 'no-store' });
    if ((await r.json()).version !== chrome.runtime.getManifest().version) chrome.runtime.reload();
  } catch {}
}

async function armer() {
  const a = await reglages();
  if (!a.on) return chrome.alarms.clear('veille');
  const deja = await chrome.alarms.get('veille');
  if (!deja) chrome.alarms.create('veille', { periodInMinutes: PERIODE });
}

chrome.runtime.onInstalled.addListener(() => {
  armer();
  discord();
});
chrome.runtime.onStartup.addListener(armer);
chrome.storage.onChanged.addListener((ch, zone) => {
  if (zone === 'local' && ch.auto && ch.auto.newValue?.on !== ch.auto.oldValue?.on) armer();
});
chrome.alarms.onAlarm.addListener((al) => {
  if (al.name === 'veille') passage().catch(() => {});
});

// Sous Linux, passer d'une fenêtre d'Helium à l'autre donne un NONE fugace :
// seule la première perte du focus compte, le retour remet à zéro.
chrome.windows.onFocusChanged.addListener(async (w) => {
  if (w !== chrome.windows.WINDOW_ID_NONE) return chrome.storage.session.set({ fond: 0 });
  const { fond = 0 } = await chrome.storage.session.get('fond');
  if (!fond) await chrome.storage.session.set({ fond: Date.now() });
});

async function depuisFond(maintenant) {
  const fenetres = await chrome.windows.getAll();
  let { fond = 0 } = await chrome.storage.session.get('fond');
  if (fenetres.some((w) => w.focused)) fond = 0;
  else if (!fond) fond = maintenant;
  await chrome.storage.session.set({ fond });
  return fond ? maintenant - fond : 0;
}

// CPU d'un processus sur la dernière minute : écart de ticks avec le passage
// précédent, sinon la moyenne 1 min du démon battrack.
function cpuDe(p, avant, s) {
  const q = avant?.tk[p.pid];
  const dt = avant ? (s.t - avant.t) / 1000 : 0;
  if (q != null && dt > 20 && dt < 400) return Math.max(0, ((p.tk - q) / s.hz / dt) * 100);
  return typeof p.m1 === 'number' ? p.m1 : null;
}

async function passage() {
  await aJour();
  const a = await reglages();
  if (!a.on) return;
  const maintenant = Date.now();
  const onglets = await chrome.tabs.query({});
  const cids = new Map();
  const partage = new Map();
  const sites = new Map();
  for (const t of onglets) {
    if (t.discarded) continue;
    let fr = [];
    try {
      fr = (await chrome.webNavigation.getAllFrames({ tabId: t.id })) || [];
    } catch {}
    const l = [...new Set(fr.map((f) => f.processId).filter((x) => x > 0))];
    cids.set(t.id, l);
    for (const c of l) {
      partage.set(c, (partage.get(c) || 0) + 1);
      const h = site(t.url);
      if (h) sites.set(c, (sites.get(c) || new Set()).add(h));
    }
  }

  const liste = [...sites].map(([c, h]) => `${c}\t${[...h].join(', ')}`).sort().join('\n');
  const s = await chrome.runtime.sendNativeMessage(HOST, { cmd: 'snap', onglets: liste });
  if (!s || !s.procs) return;
  const { avant = null, coups = {}, son = {} } = await chrome.storage.session.get(['avant', 'coups', 'son']);
  const tk = {};
  for (const p of s.procs) tk[p.pid] = p.tk;
  await chrome.storage.session.set({ avant: { t: s.t, tk } });

  const al = await chrome.alarms.get('veille');
  if (!al || al.periodInMinutes !== PERIODE) chrome.alarms.create('veille', { periodInMinutes: PERIODE });

  const fond = await depuisFond(maintenant);
  for (const t of onglets) if (t.audible) son[t.id] = maintenant;
  for (const id in son) if (maintenant - son[id] > SON || !onglets.some((t) => t.id == id)) delete son[id];
  const claude = new Set((await chrome.tabGroups.query({})).filter((g) => CLAUDE.test(g.title || '')).map((g) => g.id));

  const parCid = new Map();
  let helium = 0;
  for (const p of s.procs) {
    const v = cpuDe(p, avant, s);
    helium += v ?? 0;
    if (p.cid) parCid.set(p.cid, v);
  }

  const suite = {};
  const endormis = [];
  for (const t of onglets) {
    if (t.discarded || t.active || t.audible || son[t.id] || t.pinned || t.autoDiscardable === false) continue;
    if (claude.has(t.groupId) || !endormable(t.url || '')) continue;
    if (t.lastAccessed && maintenant - t.lastAccessed < QUITTE) continue;
    const h = site(t.url);
    const garde = excepte(h, a.except);
    if (garde && !(fond >= FOND && excepte(h, FOND_AUSSI))) continue;
    // cpu : sa part des processus qu'il partage ; plein : ces processus entiers,
    // pour que des onglets d'un même site ne passent pas tous sous le seuil
    let cpu = null;
    let plein = 0;
    for (const c of cids.get(t.id) || []) {
      const v = parCid.get(c);
      if (v != null) {
        cpu = (cpu ?? 0) + v / (partage.get(c) || 1);
        plein += v;
      }
    }
    if (cpu == null) continue;
    let dormir = false;
    if (cpu >= SEUIL && !garde) {
      suite[t.id] = (coups[t.id] || 0) + 1;
      dormir = suite[t.id] >= FOIS;
    }
    if (fond >= FOND && plein >= SEUIL_FOND && !excepte(h, MESSAGERIES) && maintenant - (t.lastAccessed || 0) >= FOND) dormir = true;
    if (!dormir) continue;
    try {
      await chrome.tabs.discard(t.id);
      delete suite[t.id];
      endormis.push({ site: h, titre: t.title || h, cpu: Math.round(cpu * 10) / 10, quand: maintenant });
    } catch {}
  }

  const total = onglets.length;
  const dorment = onglets.filter((t) => t.discarded || t.status === 'unloaded').length + endormis.length;
  const geles = onglets.filter((t) => t.frozen && !t.discarded).length;
  const releve = `${new Date(maintenant).toLocaleString('sv-SE')}|${s.bat ? 'batterie' : 'secteur'}|fond ${Math.round(fond / 60e3)} min|${total} onglets|${dorment} endormis|${geles} gelés|Helium ${helium.toFixed(1)} %`;
  await chrome.storage.session.set({ coups: suite, son });
  await chrome.storage.local.set({ releve });
  if (endormis.length) {
    const b = await reglages();
    b.log = [...endormis, ...(b.log || [])].slice(0, GARDER);
    await chrome.storage.local.set({ auto: b });
  }
}
