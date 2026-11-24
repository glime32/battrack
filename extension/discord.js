'use strict';
// Discord sans animation (emojis, avatars, décorations, autocollants, GIF), et
// caché depuis 2 min : passage sur « Messages privés », qui ne suit ni le salon
// ni la liste des membres ; retour au salon dès qu'il réapparaît.

const ATTENTE = 2 * 60e3;
const ACCUEIL = '/channels/@me';
const CDN = /(^|\.)discord(app)?\.(com|net)$/;

function fixe(src) {
  let u;
  try {
    u = new URL(src, location.href);
  } catch {
    return null;
  }
  if (!CDN.test(u.hostname)) return null;
  let change = false;
  if (u.searchParams.get('animated') === 'true') {
    u.searchParams.delete('animated');
    change = true;
  }
  if (u.searchParams.get('passthrough') === 'true') {
    u.searchParams.set('passthrough', 'false');
    change = true;
  }
  if (/\/(emojis|avatars|icons|banners|guilds)\//.test(u.pathname) && u.pathname.endsWith('.gif')) {
    u.pathname = u.pathname.slice(0, -4) + '.webp';
    change = true;
  }
  return change ? u.href : null;
}

const voulus = new WeakSet();
const gif = (v) => v.loop && v.muted && !v.controls;

function figer(el) {
  if (el.tagName === 'IMG') {
    const s = fixe(el.getAttribute('src') || '');
    if (s) el.setAttribute('src', s);
    const ss = el.getAttribute('srcset');
    if (ss) {
      const neuf = ss.replace(/\S+(?=\s+\d+(\.\d+)?[wx]|\s*,|\s*$)/g, (x) => fixe(x) || x);
      if (neuf !== ss) el.setAttribute('srcset', neuf);
    }
  } else if (el.tagName === 'VIDEO' && gif(el) && !voulus.has(el)) {
    el.autoplay = false;
    if (!el.paused) el.pause();
  }
}

function parcourir(n) {
  if (n.nodeType !== 1) return;
  if (n.tagName === 'IMG' || n.tagName === 'VIDEO') figer(n);
  else if (n.firstElementChild) for (const e of n.querySelectorAll('img, video')) figer(e);
}

const veille = new MutationObserver((ms) => {
  for (const m of ms) {
    if (m.type === 'attributes') figer(m.target);
    else for (const n of m.addedNodes) parcourir(n);
  }
});
const suivre = () => veille.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ['src', 'srcset'] });

// un GIF qu'il lance lui-même d'un clic peut tourner
document.addEventListener('click', (e) => {
  const v = e.target.closest?.('video');
  if (v && gif(v)) {
    voulus.add(v);
    v.play().catch(() => {});
  }
}, true);
document.addEventListener('play', (e) => {
  const v = e.target;
  if (v.tagName === 'VIDEO' && gif(v) && !voulus.has(v)) v.pause();
}, true);

let minuteur = 0;
let parti = false;

const absent = () => document.hidden || !document.hasFocus();
const sonEnCours = () => [...document.querySelectorAll('video, audio')].some((m) => !m.paused && !m.muted);

function partir() {
  minuteur = 0;
  if (!absent() || location.pathname.startsWith(ACCUEIL) || sonEnCours()) return;
  history.pushState(history.state, '', ACCUEIL);
  dispatchEvent(new PopStateEvent('popstate', { state: history.state }));
  parti = true;
}

function revenir() {
  clearTimeout(minuteur);
  minuteur = 0;
  if (parti && location.pathname === ACCUEIL) history.back();
  parti = false;
}

function changement() {
  if (document.hidden) {
    veille.disconnect();
  } else {
    suivre();
    parcourir(document.documentElement);
  }
  if (!absent()) revenir();
  else if (!minuteur) minuteur = setTimeout(partir, ATTENTE);
}

document.addEventListener('visibilitychange', changement);
addEventListener('focus', changement);
addEventListener('blur', changement);
if (!document.hidden) suivre();
if (absent()) minuteur = setTimeout(partir, ATTENTE);
