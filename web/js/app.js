// app.js — the flock gallery (v3, node HTTP API).
//
// The v3 node owns the flock. This page is a thin view + control layer:
//   - POLL  GET /api/flock  → a card per LIVING sheep (merged-loop WebM video,
//           name, vitality, backing, creator, lineage, coverage).
//   - the contribute loop (contribute.js) drives /api/assign → WASM pool →
//           POST /api/msg (signed PieceUpload + Coverage envelopes).
//   - "back" → a signed Vote envelope; "breed" → a signed Breed envelope.
//
// No P2P / gossip / IndexedDB / local replay — the node is the source of truth.
// We keep NO authoritative state, only the keypair (identity.js) + transient UI.

import { WorkerPool } from './pool.js';
import { loadIdentity } from './identity.js';
import { sheepName } from './names.js';
import * as api from './api.js';
import { Contributor } from './contribute.js';
import { mountWorldPicker } from './world-picker.js';

const $ = (s) => document.querySelector(s);

const FLOCK_POLL_MS = 4000;

// ---- module state -----------------------------------------------------------

let me;                       // { pubHex, pair }
let pool;                     // WorkerPool (shared by the contribute loop)
let contributor;              // Contributor (the render loop), lazily started
let contributing = false;

let myCredits = 0;            // last credits the node reported on a write
let spendSeq = Date.now();    // monotonic per-key spend sequence (§7) for Vote/Breed
let selfPub = '';             // the node's own pubkey (from /api/flock)

const cards = new Map();      // sheepId -> { record, el, video, ... }
const flockGenomes = new Map(); // sheepId -> { genomeJson, edge } for the renderer
const selected = [];          // up to 2 sheepIds picked for breeding

// ---- boot -------------------------------------------------------------------

async function main() {
  mountWorldPicker('#world-picker');
  me = await loadIdentity();
  pool = new WorkerPool();
  pool.onStatus = (status, detail) => {
    if (status === 'failed') setStatus(`renderer failed to load: ${detail || ''}`);
  };

  wireNursery();

  await refreshFlock().catch((e) => setStatus(`flock load failed: ${e.message}`));

  setInterval(() => refreshFlock().catch((e) => console.error('flock poll', e)), FLOCK_POLL_MS);
}

// ---- flock polling + card rendering -----------------------------------------

async function refreshFlock() {
  const data = await api.getFlock();
  if (data.self) selfPub = data.self;

  const flockEl = $('#flock');
  if (flockEl.textContent === 'loading the flock…') flockEl.textContent = '';

  const live = new Set();
  for (const rec of data.sheep || []) {
    live.add(rec.id);
    // Cache the genome so the contribute loop can render this sheep's tiles.
    if (rec.genome) {
      flockGenomes.set(rec.id, {
        genomeJson: JSON.stringify(rec.genome),
        edge: rec.resolution || 384,
      });
    }
    upsertCard(rec);
  }
  for (const [id, c] of cards) {
    if (!live.has(id)) { c.el.remove(); cards.delete(id); flockGenomes.delete(id); pruneSelected(id); }
  }
  if (!cards.size) flockEl.textContent = 'no living sheep yet';
  setStatus();
}

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

  const video = document.createElement('video');
  video.muted = true; video.loop = true; video.autoplay = true;
  video.playsInline = true; video.setAttribute('playsinline', '');
  video.addEventListener('click', () => toggleSelect(rec.id));

  const meta = document.createElement('div');
  meta.className = 'meta';

  const label = document.createElement('a');
  label.textContent = sheepName(rec);
  label.href = `sheep.html?id=${encodeURIComponent(rec.id)}`;
  label.target = '_blank';

  const tallyEl = document.createElement('span');
  tallyEl.className = 'tally';

  const contribBtn = document.createElement('button');
  contribBtn.textContent = 'contribute';
  contribBtn.title = 'pledge idle CPU to render the flock — accepted tiles earn ' +
    'credits to spend on selection';
  contribBtn.addEventListener('click', toggleContribute);

  const backBtn = document.createElement('button');
  backBtn.className = 'back';
  backBtn.textContent = 'back ▲';
  backBtn.title = 'spend an earned credit to back this sheep — backing decides survival';
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

  const src = rec.video ? api.absoluteUrl(rec.video) : api.videoUrl(rec.id);
  if (c.video.dataset.src !== src) {
    c.video.dataset.src = src;
    c.video.src = src;
    c.video.play?.().catch(() => { /* autoplay policy: harmless */ });
  }

  // Backing tally (the selection ♥).
  const n = rec.backing || 0;
  c.tallyEl.textContent = n > 0 ? `${n} ♥` : '';
  c.tallyEl.title = n > 0 ? `${n} credit${n === 1 ? '' : 's'} backing this sheep` : '';

  // Vitality bar (§2.2 survival): 0..1+ → bar width. Coverage in the caption.
  const vit = typeof rec.vitality === 'number' ? rec.vitality : 0;
  c.barFill.style.width = `${Math.max(0, Math.min(1, vit)) * 100}%`;
  c.barFill.title = `vitality ${vit.toFixed(2)}`;

  const cov = rec.coverage || 0;
  const who = rec.creator ? ` · by ${rec.creator.slice(0, 8)}` : '';
  c.tilesEl.textContent = `${cov} tiles${who}`;
  c.tilesEl.title = rec.creator ? `creator ${rec.creator}\n${rec.coverage} accepted tiles` : '';

  c.el.classList.toggle('selected', selected.includes(rec.id));
  c.contribBtn.classList.toggle('on', contributing);
  c.contribBtn.textContent = contributing ? 'contributing…' : 'contribute';
  c.backBtn.disabled = myCredits <= 0;
}

function refreshAllCards() {
  for (const c of cards.values()) updateCard(c);
}

// ---- voting (back a sheep) — a signed Vote envelope -------------------------

async function doVote(sheepId) {
  try {
    const r = await api.vote(me, sheepId, spendSeq++);
    if (!r.accepted) {
      setStatus(`back rejected: ${r.reason || 'no credits — contribute to earn some'}`);
      return;
    }
    if (typeof r.credits === 'number') myCredits = r.credits;
    setStatus(`backed ${sheepName(sheepId)}`);
    refreshFlock().catch(() => {});
  } catch (e) {
    setStatus(`back failed: ${e.message}`);
  }
}

// ---- contribute toggle ------------------------------------------------------

function toggleContribute() {
  contributing = !contributing;
  if (contributing) {
    if (!contributor) {
      contributor = new Contributor(pool, me, api, {
        genomeFor: (id) => flockGenomes.get(id) || null,
        onResult: (reply) => {
          // Reply may be a single result or { results:[...] }.
          const items = reply?.results || [reply];
          for (const it of items) {
            if (it && typeof it.credits === 'number') myCredits = it.credits;
          }
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

// ---- nursery (breed two selected sheep) -------------------------------------

function wireNursery() {
  const release = $('#release');
  if (release) release.addEventListener('click', doBreed);
  updateNursery();
}

function toggleSelect(id) {
  const at = selected.indexOf(id);
  if (at !== -1) selected.splice(at, 1);
  else {
    selected.push(id);
    if (selected.length > 2) selected.shift();
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
  if (!nursery) return;
  if (selected.length < 2) {
    nursery.classList.add('picking');
    if (release) release.hidden = true;
    return;
  }
  nursery.classList.remove('picking');
  const [a, b] = selected;
  if (note) note.textContent = `${sheepName(a)} + ${sheepName(b)}`;
  if (release) {
    release.hidden = false;
    release.disabled = myCredits <= 0;
    release.textContent = myCredits > 0 ? 'breed this pairing' : 'need a credit to breed';
  }
}

async function doBreed() {
  if (selected.length < 2) return;
  const [a, b] = selected;
  const release = $('#release');
  if (release) { release.disabled = true; release.textContent = 'breeding…'; }
  try {
    // Default to the base resolution tier (R384). Breed re-derives the genome
    // server-side from the recorded parents + seed; we send the signed birth.
    const r = await api.breed(me, a, b, 'R384', spendSeq++);
    if (!r.accepted) {
      setStatus(`breed rejected: ${r.reason || 'need a credit'}`);
    } else {
      if (typeof r.credits === 'number') myCredits = r.credits;
      setStatus('child released into the flock');
      selected.length = 0;
    }
    updateNursery();
    await refreshFlock().catch(() => {});
  } catch (e) {
    setStatus(`breed failed: ${e.message}`);
    if (release) { release.disabled = false; release.textContent = 'breed this pairing'; }
  }
}

// ---- status line ------------------------------------------------------------

function setStatus(msg) {
  const parts = [];
  if (msg) parts.push(msg);
  parts.push(`${myCredits} credit${myCredits === 1 ? '' : 's'}`);
  if (contributing) parts.push('contributing');
  const el = $('#status');
  if (el) el.textContent = parts.join(' · ');
}

main().catch((err) => {
  console.error(err);
  const el = $('#status');
  if (el) el.textContent = 'error: ' + (err?.message || err);
});
