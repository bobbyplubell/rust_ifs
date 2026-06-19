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

// Contribute stats (this session). Contribution is GLOBAL — one shared loop
// pulls /api/assign which hands out work across the whole flock, so these are
// per-browser totals, not per-sheep.
let renderedTiles = 0;        // blocks this browser rendered + submitted (local count)
let acceptedLocal = 0;        // fallback Accepted count (older nodes w/o confirmed_tiles)
let confirmedTiles = -1;      // node-authoritative running total of confirmed tiles (-1 = unseen)
let tilesPerCredit = 0;       // node-reported tiles per credit (e.g. 128); 0 = unknown
let contribError = '';        // last contribute error (surfaced briefly in the stats line)

const cards = new Map();      // sheepId -> { record, el, video, ... }
const flockGenomes = new Map(); // sheepId -> { genomeJson, edge } for the renderer
const selected = [];          // up to 2 sheepIds picked for breeding

// ---- boot -------------------------------------------------------------------

async function main() {
  me = await loadIdentity();
  loadContribStats();   // restore this browser's contribute totals across reloads
  pool = new WorkerPool();
  pool.onStatus = (status, detail) => {
    if (status === 'failed') setStatus(`renderer failed to load: ${detail || ''}`);
  };

  wireContribute();
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

  // NOTE: contribution is GLOBAL — driven by the single header toggle, not a
  // per-sheep button. The card keeps only "back ▲".
  const backBtn = document.createElement('button');
  backBtn.className = 'back';
  backBtn.textContent = 'back ▲';
  backBtn.title = 'spend an earned credit to back this sheep — backing decides survival';
  backBtn.addEventListener('click', () => doVote(rec.id));

  meta.append(label, tallyEl, backBtn);

  const bar = document.createElement('div');
  bar.className = 'bar';
  const barFill = document.createElement('div');
  barFill.className = 'bar-fill';
  bar.append(barFill);

  const tilesEl = document.createElement('div');
  tilesEl.className = 'tiles';

  card.append(video, bar, meta, tilesEl);
  return { record: rec, el: card, video, tallyEl, tilesEl, backBtn, barFill };
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

// ---- global contribute toggle + live stats ----------------------------------
//
// Contribution is GLOBAL: the single shared Contributor pulls /api/assign, which
// hands out work across ALL living sheep. There is exactly one contribute state
// for the whole app (one toggle, one Contributor instance), driven from the
// header control — never per sheep.

// Persist this browser's contribute totals across reloads, keyed by identity so
// the counters don't reset to 0 on refresh. The node remains authoritative —
// `credits`/`confirmed_tiles` from any /api/msg reply overwrite these (via max),
// and tiles confirmed while the tab was closed appear on the next contribution.
function contribStoreKey() { return `sheep-contrib-${me?.pubHex || 'anon'}`; }

function loadContribStats() {
  try {
    const s = JSON.parse(localStorage.getItem(contribStoreKey()) || '{}');
    if (Number.isFinite(s.rendered)) renderedTiles = s.rendered;
    if (Number.isFinite(s.accepted)) acceptedLocal = s.accepted;
    if (Number.isFinite(s.confirmed)) confirmedTiles = s.confirmed;
    if (Number.isFinite(s.credits)) myCredits = s.credits;
    if (Number.isFinite(s.tilesPerCredit)) tilesPerCredit = s.tilesPerCredit;
  } catch { /* corrupt/absent — start fresh */ }
}

function saveContribStats() {
  try {
    localStorage.setItem(contribStoreKey(), JSON.stringify({
      rendered: renderedTiles, accepted: acceptedLocal,
      confirmed: confirmedTiles, credits: myCredits, tilesPerCredit,
    }));
  } catch { /* private mode / quota — non-fatal */ }
}

function wireContribute() {
  const btn = $('#contribute');
  if (btn) btn.addEventListener('click', toggleContribute);
  renderContribStats();
}

function toggleContribute() {
  contributing = !contributing;
  if (contributing) {
    if (!contributor) {
      contributor = new Contributor(pool, me, api, {
        genomeFor: (id) => flockGenomes.get(id) || null,
        onResult: onContribResult,
        onError: (e) => {
          contribError = e?.message || String(e);
          console.warn('contribute:', contribError);
          renderContribStats();
        },
      });
    }
    contribError = '';
    contributor.start();
  } else {
    contributor?.stop();
  }
  renderContribStats();
  setStatus();
}

// Per-submission callback from the Contributor. `reply` is the node's /api/msg
// result: a single object for one envelope, or `{results:[...]}` for a batch
// (contribute.js posts [pieceEnv, coverEnv] as an array, so expect a batch).
// Each result item carries: accepted (bool), credits (number, running total),
// and — on newer nodes — confirmed_tiles (number, submitter's running total)
// and tiles_per_credit (number, e.g. 128).
function onContribResult(reply) {
  contribError = '';
  // One "block" was rendered + submitted regardless of acceptance.
  renderedTiles += 1;

  const items = reply?.results || [reply];
  for (const it of items) {
    if (!it || typeof it !== 'object') continue;
    if (typeof it.credits === 'number') myCredits = Math.max(myCredits, it.credits);
    if (typeof it.tiles_per_credit === 'number') tilesPerCredit = it.tiles_per_credit;
    if (typeof it.confirmed_tiles === 'number') {
      // Node-authoritative running total — SET to the max seen, don't increment.
      confirmedTiles = Math.max(confirmedTiles, it.confirmed_tiles);
    } else if (it.accepted === true) {
      // Older node (no confirmed_tiles): fall back to a local accepted counter.
      acceptedLocal += 1;
    }
  }

  saveContribStats();
  renderContribStats();
  refreshAllCards();
  setStatus();
}

// The authoritative Accepted count: prefer the node's confirmed_tiles total,
// else the locally-incremented fallback.
function acceptedCount() {
  return confirmedTiles >= 0 ? confirmedTiles : acceptedLocal;
}

function renderContribStats() {
  const btn = $('#contribute');
  if (btn) {
    btn.classList.toggle('on', contributing);
    btn.textContent = contributing ? 'contributing…' : 'contribute';
  }

  const renderedEl = $('#stat-rendered');
  if (renderedEl) renderedEl.textContent = String(renderedTiles);

  const acceptedEl = $('#stat-accepted');
  if (acceptedEl) acceptedEl.textContent = String(acceptedCount());

  const creditsEl = $('#stat-credits');
  if (creditsEl) {
    let txt = String(myCredits);
    // Progress to the next credit, when the node tells us the ratio.
    if (tilesPerCredit > 0 && confirmedTiles >= 0) {
      txt += ` (${confirmedTiles % tilesPerCredit}/${tilesPerCredit} to next)`;
    }
    creditsEl.textContent = txt;
  }

  const statusEl = $('#contrib-status');
  if (statusEl) {
    let s;
    if (contribError) s = `error: ${contribError}`;
    else if (contributing) s = 'contributing — rendering…';
    else s = 'idle';
    statusEl.textContent = s;
    statusEl.classList.toggle('err', !!contribError);
  }
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
