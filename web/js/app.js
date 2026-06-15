// app.js — the flock gallery (v2, coordinator architecture).
//
// The coordinator owns the flock now. This page is a thin view + control layer:
//   - POLL  GET /api/flock  → render a card per living sheep (served video,
//           name, tile totals, backing count, back/contribute buttons).
//   - POLL  GET /api/me     → the user's credits / reputation.
//   - POST  /api/vote       → spend a credit to back a sheep.
//   - POST  /api/breed      → propose a parent pairing (the nursery).
//   - the contribute loop (contribute.js) drives /api/assign → WASM pool →
//     /api/submit, earning credits.
//
// No P2P / gossip / IndexedDB fact-store / local replay — the server is the
// single source of truth. We keep NO authoritative state, only the keypair
// (identity.js) and transient UI state.

import { WorkerPool } from './pool.js';
import { loadIdentity } from './identity.js';
import { sheepName, provenance } from './names.js';
import * as api from './api.js';
import { Contributor } from './contribute.js';
import { mountWorldPicker } from './world-picker.js';

const $ = (s) => document.querySelector(s);

const FLOCK_POLL_MS = 4000; // re-poll the flock for live state
const ME_POLL_MS = 5000;    // re-poll the user's credits/reputation

// ---- module state -----------------------------------------------------------

let me;                       // { pubHex, pair }
let pool;                     // WorkerPool (shared by the contribute loop)
let contributor;             // Contributor (the render loop), lazily started
let contributing = false;     // global contribute toggle

let myCredits = 0;            // last-known credits from /api/me
let myReputation = 0;
let genClosesAt = 0;          // wall-clock ms when the current gen closes
let currentGen = -1;

const cards = new Map();      // sheepId -> { record, el, video, tallyEl, tilesEl, backBtn }
const selected = [];          // up to 2 sheepIds picked for breeding

// ---- boot -------------------------------------------------------------------

async function main() {
  mountWorldPicker('#world-picker'); // header world <select> (config.js / WORLDS)
  me = await loadIdentity();
  pool = new WorkerPool();
  pool.onStatus = (status, detail) => {
    if (status === 'failed') setStatus(`renderer failed to load: ${detail || ''}`);
  };

  $('#nursery-hint'); // present in DOM
  wireNursery();

  await refreshFlock().catch((e) => setStatus(`flock load failed: ${e.message}`));
  await refreshMe().catch(() => { /* a fresh key may 404 until it has a row */ });

  setInterval(() => refreshFlock().catch((e) => console.error('flock poll', e)), FLOCK_POLL_MS);
  setInterval(() => refreshMe().catch(() => {}), ME_POLL_MS);
  setInterval(tickCountdown, 1000);
  tickCountdown();
}

// ---- flock polling + card rendering -----------------------------------------

async function refreshFlock() {
  const data = await api.getFlock();
  currentGen = data.gen;
  if (typeof data.gen_closes_in_ms === 'number') {
    genClosesAt = Date.now() + data.gen_closes_in_ms;
  }

  const flockEl = $('#flock');
  if (flockEl.firstChild && flockEl.textContent === 'loading the flock…') {
    flockEl.textContent = '';
  }

  const live = new Set();
  for (const rec of data.sheep || []) {
    live.add(rec.id);
    upsertCard(rec);
  }
  // Drop cards for sheep that left the living flock (perished this gen).
  for (const [id, c] of cards) {
    if (!live.has(id)) { c.el.remove(); cards.delete(id); pruneSelected(id); }
  }
  setStatus('');
}

// Create or update a card for a flock record.
function upsertCard(rec) {
  let c = cards.get(rec.id);
  if (!c) {
    c = buildCard(rec);
    cards.set(rec.id, c);
    $('#flock').append(c.el);
  }
  c.record = rec;
  updateCard(c);
}

function buildCard(rec) {
  const card = document.createElement('div');
  card.className = 'card';
  card.dataset.id = rec.id;

  // The merged loop video served by the coordinator — looping, muted, autoplay
  // (replaces v1's locally-rendered canvas). Clicking opens the fullscreen page.
  const video = document.createElement('video');
  video.muted = true; video.loop = true; video.autoplay = true;
  video.playsInline = true; video.setAttribute('playsinline', '');
  video.addEventListener('click', () => toggleSelect(rec.id));

  const meta = document.createElement('div');
  meta.className = 'meta';

  const prov = provenance(rec);
  const label = document.createElement('a');
  label.textContent = sheepName(rec);
  label.title = `${prov.how}\n${rec.id}`;
  label.href = `sheep.html?id=${encodeURIComponent(rec.id)}`;
  label.target = '_blank';

  const tallyEl = document.createElement('span');
  tallyEl.className = 'tally';

  const contribBtn = document.createElement('button');
  contribBtn.textContent = 'contribute';
  contribBtn.title = 'pledge idle CPU to render the flock — every 128 accepted ' +
    'tiles earns you one credit to spend on selection';
  contribBtn.addEventListener('click', toggleContribute);

  const backBtn = document.createElement('button');
  backBtn.className = 'back';
  backBtn.textContent = 'back ▲';
  backBtn.title = 'spend one earned credit to back this sheep — backing (not ' +
    'render work) decides who survives the generation';
  backBtn.addEventListener('click', () => doVote(rec.id));

  meta.append(label, tallyEl, contribBtn, backBtn);

  const bar = document.createElement('div');
  bar.className = 'bar';
  const barFill = document.createElement('div');
  barFill.className = 'bar-fill';
  bar.append(barFill);

  const tilesEl = document.createElement('div');
  tilesEl.className = 'tiles';

  card.append(video, bar, meta, tilesEl);

  return { record: rec, el: card, video, tallyEl, tilesEl, backBtn, barFill, contribBtn };
}

function updateCard(c) {
  const rec = c.record;

  // Video source: only (re)assign when it changes, so playback isn't restarted
  // on every poll. The coordinator serves the merged loop at /api/video/:id;
  // `rec.video` may be a ready URL, else fall back to the endpoint.
  const src = rec.video || api.videoUrl(rec.id);
  if (c.video.dataset.src !== src) {
    c.video.dataset.src = src;
    c.video.src = src;
    c.video.classList.remove('pending');
    c.video.play?.().catch(() => { /* autoplay policy: stays paused, harmless */ });
  }

  // Backing tally (the selection ♥). `backings` = credits backing it this gen.
  const n = rec.backings || 0;
  c.tallyEl.textContent = n > 0 ? `${n} ♥` : '';
  c.tallyEl.title = n > 0 ? `${n} credit${n === 1 ? '' : 's'} backing this sheep this generation` : '';

  // Per-card tile totals (the swarm's accepted tiles for this sheep).
  const t = rec.tiles || 0;
  c.tilesEl.textContent = t ? `swarm ${t} tiles` : '';
  c.tilesEl.title = t ? `${t} accepted tiles merged into this sheep` : '';

  // selection highlight + contribute button state
  c.el.classList.toggle('selected', selected.includes(rec.id));
  c.contribBtn.classList.toggle('on', contributing);
  c.contribBtn.textContent = contributing ? 'contributing…' : 'contribute';
  c.backBtn.disabled = myCredits <= 0;
}

function refreshAllCards() {
  for (const c of cards.values()) updateCard(c);
}

// ---- /api/me (credits) ------------------------------------------------------

async function refreshMe() {
  const m = await api.getMe(me.pubHex);
  myCredits = m.credits ?? 0;
  myReputation = m.reputation ?? 0;
  refreshAllCards();
  setStatus();
}

// ---- voting (back a sheep) --------------------------------------------------

async function doVote(sheepId) {
  if (myCredits <= 0) { setStatus('no credits — contribute renders to earn some'); return; }
  try {
    const r = await api.vote(me, sheepId);
    myCredits = r.credits ?? myCredits - 1;
    const c = cards.get(sheepId);
    if (c && typeof r.backings === 'number') { c.record.backings = r.backings; updateCard(c); }
    refreshAllCards();
    setStatus(`backed ${sheepName({ id: sheepId })}`);
  } catch (e) {
    setStatus(`vote failed: ${e.message}`);
  }
}

// ---- contribute toggle ------------------------------------------------------

function toggleContribute() {
  contributing = !contributing;
  if (contributing) {
    if (!contributor) {
      contributor = new Contributor(pool, me, api, {
        onResult: (reply) => {
          if (typeof reply.credits === 'number') myCredits = reply.credits;
          if (typeof reply.reputation === 'number') myReputation = reply.reputation;
          refreshAllCards();
          setStatus();
        },
        onError: (e) => console.warn('contribute:', e.message),
      });
    }
    contributor.start();
  } else {
    contributor?.stop();
  }
  refreshAllCards();
  setStatus();
}

// ---- nursery (propose a pairing) --------------------------------------------

function wireNursery() {
  const release = $('#release');
  release.addEventListener('click', doBreed);
  updateNursery();
}

function toggleSelect(id) {
  const at = selected.indexOf(id);
  if (at !== -1) {
    selected.splice(at, 1);
  } else {
    selected.push(id);
    if (selected.length > 2) selected.shift(); // keep the two most recent
  }
  refreshAllCards();
  updateNursery();
}

function pruneSelected(id) {
  const at = selected.indexOf(id);
  if (at !== -1) { selected.splice(at, 1); updateNursery(); }
}

function updateNursery() {
  const nursery = $('#nursery');
  const note = $('#nursery-note');
  const release = $('#release');
  const video = $('#child-video');

  if (selected.length < 2) {
    nursery.classList.add('picking');
    release.hidden = true;
    return;
  }
  nursery.classList.remove('picking');
  const [a, b] = selected;
  note.textContent = `${sheepName({ id: a })} + ${sheepName({ id: b })}`;
  // No local render of the child anymore — the coordinator breeds + renders it.
  // Show the (yet-to-exist) child as a blank stage until released.
  video.removeAttribute('src');
  release.hidden = false;
  release.disabled = myCredits <= 0;
  release.textContent = myCredits > 0 ? 'propose this pairing' : 'need a credit to breed';
}

async function doBreed() {
  if (selected.length < 2) return;
  const [a, b] = selected;
  const release = $('#release');
  release.disabled = true;
  release.textContent = 'proposing…';
  try {
    const r = await api.breed(me, a, b);
    setStatus(`child proposed: ${r.childId ? sheepName({ id: r.childId }) : 'queued'}`);
    selected.length = 0;
    updateNursery();
    refreshAllCards();
    await refreshFlock().catch(() => {});
    await refreshMe().catch(() => {});
  } catch (e) {
    setStatus(`breed failed: ${e.message}`);
    release.disabled = false;
    release.textContent = 'propose this pairing';
  }
}

// ---- gen countdown ----------------------------------------------------------

function tickCountdown() {
  setStatus();
}

function fmtCountdown() {
  if (!genClosesAt) return '';
  const ms = Math.max(0, genClosesAt - Date.now());
  const s = Math.floor(ms / 1000);
  const mm = String(Math.floor(s / 60)).padStart(2, '0');
  const ss = String(s % 60).padStart(2, '0');
  return `gen ${currentGen} closes in ${mm}:${ss}`;
}

// ---- status line ------------------------------------------------------------

function setStatus(msg) {
  const parts = [];
  if (msg) parts.push(msg);
  parts.push(`${myCredits} credit${myCredits === 1 ? '' : 's'}`);
  if (myReputation) parts.push(`rep ${myReputation}`);
  const cd = fmtCountdown();
  if (cd) parts.push(cd);
  if (contributing) parts.push('contributing');
  $('#status').textContent = parts.join(' · ');
}

main().catch((err) => {
  console.error(err);
  $('#status').textContent = 'error: ' + (err?.message || err);
});
