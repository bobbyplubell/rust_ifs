// app.js — the flock view (batch / community-render era).
//
// Each card shows frame 0 of a sheep, painted from a per-sheep accumulated
// integer histogram (`acc0`) that GROWS as batches arrive: our own idle
// contributions, batches gossiped by peers (onBatch), and verified merged
// renders pulled from the swarm (onRender). There are NO quality tiers and NO
// per-voter loop proof — a sheep's render quality, popularity, and selection
// weight are one number: the count of verified batches contributed to it.
//
// All state comes from the store + network (net.js); all pixels from the
// worker pool (pool.js). No framework, no build step.

import { WorkerPool } from './pool.js';
import { sha256Hex, utf8 } from './hash.js';
import { loadIdentity, sign, verify, PEER_NS } from './identity.js';
import { openStore } from './store.js';
import {
  Net, BroadcastTransport, CompositeTransport, gen, GEN_MS, GENESIS_GEN,
  BATCH_SPEC, BATCH_SPP, batchKey, batchSignBytes, sheepSignBytes, fraudSignBytes,
} from './net.js';
import { computeFlock, breedChallenge } from './gens.js';
import { handle, provenance } from './names.js';
import { Auditor } from './audit.js';
import { RELAYS } from '../config.js';

const $ = (s) => document.querySelector(s);
const params = new URLSearchParams(location.search);
// ?workers=N caps the worker pool (stress testing packs hundreds of peers on
// one machine; idle peers don't need 4 workers each).
const WORKERS_OVERRIDE = Number(params.get('workers')) || null;
const pool = new WorkerPool(WORKERS_OVERRIDE ?? undefined);

// Frame every card shows. The flock is a still gallery of frame 0; the
// fullscreen view (sheep.html) animates the whole loop.
const CARD_FRAME = 0;
// Cell count of one batch/frame histogram at BATCH_SPEC (BigUint64Array length).
const HIST_CELLS = BATCH_SPEC.width * BATCH_SPEC.ss * BATCH_SPEC.height * BATCH_SPEC.ss * 4;
// Quick low-sample placeholder so a card is never blank before batches land.
const PREVIEW = { width: BATCH_SPEC.width, height: BATCH_SPEC.height, samples: 200_000, seed: 7 };

// sheepId -> {
//   record, canvas, tallyEl, contribBtn, card, barFill, onScreen,
//   acc0: BigUint64Array|null,   // accumulated frame-0 histogram
//   covered: Set<number>,        // frame-0 batch idx values merged into acc0
//   tonemapPending, repaintQueued, paintedPreview,
// }
const cards = new Map();
const selected = [];       // up to two sheepIds picked as parents

let me, store, net, auditor, baked = [];
let shownGen = -1;
let banned = new Set();     // contributors with verified fraud proofs (local view)

async function main() {
  me = await loadIdentity();
  store = await openStore();

  // Tabs always talk via BroadcastChannel; the internet swarm joins in when
  // relays are configured (libp2p bundle loaded lazily, failure non-fatal).
  // Relays come from config.js, but a `?relay=<multiaddr>` URL param (or a
  // `relays` localStorage entry, comma-separated) overrides/augments them —
  // handy for pointing at a locally-run relay without editing config.js.
  const relayOverride = [
    ...(params.get('relay') ? [params.get('relay')] : []),
    ...((localStorage.getItem('relays') || '').split(',').map((s) => s.trim()).filter(Boolean)),
  ];
  const relays = relayOverride.length ? relayOverride : RELAYS;
  const transports = [new BroadcastTransport()];
  if (relays.length) {
    try {
      const { createLibp2pTransport } = await import('./vendor/libp2p.js');
      transports.push(await createLibp2pTransport({ relays }));
      console.log('libp2p transport up; relays:', relays);
    } catch (err) {
      console.error('libp2p transport unavailable:', err);
    }
  }

  // Resolve a sheep's genome from the live flock (cards hold every living
  // sheep, incl. derived children that aren't store facts), then baked, then
  // store — without the cards path peers drop batches for bred children.
  const lookupSheep = async (id) =>
    cards.get(id)?.record
    ?? baked.find((s) => s.id === id)
    ?? (await store.allSheep()).find((s) => s.id === id);

  net = new Net({
    transport: new CompositeTransport(transports),
    store,
    identity: { pubHex: me.pubHex, pair: me.pair },
    sign,
    verify,
    // Re-render one batch (hash only) — net uses this to confirm a fraud claim.
    checkBatchHash: (genomeJson, sheepId, frame, idx) =>
      pool.submit({
        type: 'batch-hash', genomeJson, sheepId, frame, idx,
        w: BATCH_SPEC.width, h: BATCH_SPEC.height, ss: BATCH_SPEC.ss, spp: BATCH_SPP,
      }).done.then((m) => m.hash),
    checkSheepId: (genomeJson) =>
      pool.submit({ type: 'sheep-id', genomeJson }).done.then((m) => m.id),
    lookupSheep,
    verifyRender: (args) => verifyRender(lookupSheep, args),
    onSheep: () => scheduleRebuild(),
    onBatch: (rec) => onBatch(rec).catch(console.error),
    onFraud: () => scheduleRebuild(), // a discredited contributor changes tallies
    onRender: (sheepId, frame, hist) => onRender(sheepId, frame, hist).catch(console.error),
  });

  auditor = new Auditor({
    pool, store, baked,
    publishFraud: (f) => net.publishFraud(f),
    identity: { pubHex: me.pubHex, pair: me.pair },
    sign,
    isBanned: (pub) => net.isBanned(pub),
    onUpdate: () => updateStatus(),
    intervalMs: Number(params.get('auditms')) || 8000, // tests can speed audits
    lookupSheep, // resolve derived children too
  });

  // Seed the baked gen-0 flock from the static manifest (local only, not
  // gossiped — origin 'seed', flagged .baked so gens.js treats them specially).
  const manifest = await (await fetch('genomes/manifest.json')).json();
  for (const s of manifest.sheep) {
    const genome = await (await fetch(s.file)).text();
    const id = (await pool.submit({ type: 'sheep-id', genomeJson: genome }).done).id;
    baked.push({
      id, genome, parents: null, gen: 0, origin: 'seed',
      author: null, sig: null, baked: true, name: s.name,
    });
  }

  // Install hooks before the (awaited) flock build so tests/harnesses that
  // wait on a card don't race the hook assignment.
  installDebugHooks();
  if (params.get('stress')) installStressHooks();

  await rebuildFlock();
  await net.start();
  banned = net.banned;
  if (!params.get('noaudit')) auditor.start();

  // ?nocontribute: a pure auditor/viewer peer (tests use this so an audit peer
  // keeps its pool free instead of constantly contributing).
  if (!params.get('nocontribute')) startContributeLoop();

  shownGen = gen();
  setInterval(() => {
    updateStatus();
    if (gen() !== shownGen) {
      shownGen = gen();
      scheduleRebuild(); // generation closed: survivors chosen, children born
    }
  }, 1000);
  updateStatus();
}

// ---- generation engine glue --------------------------------------------------

const mutateFn = (genomeJson, challengeHex, rate) =>
  pool.submit({ type: 'mutate', genomeJson, challengeHex, rate }).done;
const randomFn = (seed) =>
  pool.submit({ type: 'random-genome', seed }).done;
const breedFn = (aJson, bJson, challengeHex) =>
  pool.submit({ type: 'breed', aJson, bJson, challengeHex }).done;

let rebuildTimer = null;
function scheduleRebuild() {
  clearTimeout(rebuildTimer);
  rebuildTimer = setTimeout(() => rebuildFlock().catch(showError), 400);
}

// Recompute the living flock and diff it against the cards on screen — only
// added/removed cards change (the histograms persist across rebuilds).
async function rebuildFlock() {
  // Ban set: net's live view, plus any fraud proofs' offending contributors.
  banned = new Set(net?.banned ?? []);
  for (const f of await store.allFraud()) if (f.contributor) banned.add(f.contributor);

  const { living } = await computeFlock({
    store, baked, breedFn, mutateFn, randomFn, banned,
  });
  for (const [id, entry] of cards) {
    if (!living.has(id)) {
      stopReplay(entry);
      flockVisibility.unobserve(entry.card);
      const at = selected.indexOf(id);
      if (at !== -1) { selected.splice(at, 1); $('#nursery').hidden = true; }
      entry.card.remove();
      cards.delete(id);
    }
  }
  for (const record of living.values()) addCard(record);
  // Derived (born) sheep aren't store facts; stash the current ones so
  // sheep.html can show them full screen.
  try {
    localStorage.setItem(`sheep-derived-${PEER_NS}`,
      JSON.stringify([...living.values()].filter((r) => r.derived)));
  } catch { /* quota — full-screen view of derived sheep just won't resolve */ }
  for (const id of cards.keys()) updateTally(id);
  updateStatus();
}

// ---- flock cards ------------------------------------------------------------

function addCard(record) {
  if (cards.has(record.id)) return;

  const card = document.createElement('div');
  card.className = 'card';
  card.dataset.id = record.id;
  const canvas = document.createElement('canvas');
  canvas.width = BATCH_SPEC.width;
  canvas.height = BATCH_SPEC.height;
  const meta = document.createElement('div');
  meta.className = 'meta';
  const prov = provenance(record);
  const label = document.createElement('a');
  label.textContent = prov.who;
  label.title = `${prov.how}\n${record.id}`;
  label.href = `sheep.html?id=${record.id}${PEER_NS !== '0' ? `&peer=${PEER_NS}` : ''}`;
  label.target = '_blank';
  // Hand the already-rendered card pixels to the fullscreen page for an
  // instant first paint (it refines from there).
  label.addEventListener('click', () => {
    try {
      localStorage.setItem(`sheep-preview-${record.id}`, canvas.toDataURL('image/png'));
    } catch { /* quota: fullscreen just starts from black */ }
  });
  const tallyEl = document.createElement('span');
  tallyEl.className = 'tally';
  const contribBtn = document.createElement('button');
  contribBtn.textContent = 'contribute';
  contribBtn.title = 'pledge idle CPU to this sheep — every batch you render ' +
    'sharpens it for everyone and earns it a vote';
  meta.append(label, tallyEl, contribBtn);
  const bar = document.createElement('div');
  bar.className = 'bar';
  const barFill = document.createElement('div');
  barFill.className = 'bar-fill';
  bar.append(barFill);
  card.append(canvas, bar, meta);
  $('#flock').append(card);

  const entry = {
    record, canvas, tallyEl, contribBtn, card, barFill, onScreen: true,
    acc0: null, covered: new Set(),
    tonemapPending: false, repaintQueued: false, paintedPreview: false,
    pledged: false,
  };
  cards.set(record.id, entry);
  flockVisibility.observe(card);

  canvas.addEventListener('click', () => toggleSelect(record.id));
  contribBtn.addEventListener('click', () => {
    entry.pledged = !entry.pledged;
    contribBtn.textContent = entry.pledged ? 'pledged ✓' : 'contribute';
    contribBtn.classList.toggle('on', entry.pledged);
  });

  // Bootstrap this card's accumulation from any batches we already hold for
  // frame 0, then paint a quick preview placeholder, then the tally.
  bootstrapCard(entry).catch(showError);
  updateTally(record.id);
}

// Paint a worker tonemap/preview onto a canvas, resizing to match the image.
function paintTo(canvas, m) {
  if (canvas.width !== m.width || canvas.height !== m.height) {
    canvas.width = m.width;
    canvas.height = m.height;
  }
  canvas.getContext('2d').putImageData(
    new ImageData(new Uint8ClampedArray(m.rgba), m.width, m.height), 0, 0);
}

// Element-wise add of `add` into `acc` (both BigUint64Array, same length).
// Integer accumulation is exactly associative/commutative, so order doesn't
// matter and two peers with the same coverage hold byte-identical pixels.
function mergeHist(acc, add) {
  for (let i = 0; i < acc.length; i++) acc[i] += add[i];
}

// Render the batches we already hold for this card's frame and merge them, so a
// returning visitor's card starts at the coverage they last had (and so peers'
// earlier gossip is reflected). Then paint the placeholder/repaint.
async function bootstrapCard(entry) {
  const have = (await store.batchesForSheep(entry.record.id))
    .filter((b) => b.frame === CARD_FRAME);
  for (const b of have) {
    if (entry.covered.has(b.idx)) continue;
    if (banned.has(b.contributor)) continue;
    const reply = await pool.submit(renderBatchMsg(entry.record, CARD_FRAME, b.idx)).done;
    if (reply.type !== 'batch-done') continue;
    mergeBatchInto(entry, b.idx, new BigUint64Array(reply.hist));
  }
  if (entry.covered.size) repaint(entry);
  else paintPreview(entry);
}

function renderBatchMsg(record, frame, idx) {
  return {
    type: 'render-batch', genomeJson: record.genome, sheepId: record.id,
    frame, idx, w: BATCH_SPEC.width, h: BATCH_SPEC.height, ss: BATCH_SPEC.ss, spp: BATCH_SPP,
  };
}

// Merge a rendered frame-0 batch histogram into a card's accumulation.
function mergeBatchInto(entry, idx, hist) {
  if (entry.covered.has(idx)) return false;
  if (!entry.acc0) entry.acc0 = new BigUint64Array(HIST_CELLS);
  mergeHist(entry.acc0, hist);
  entry.covered.add(idx);
  return true;
}

async function paintPreview(entry) {
  if (entry.paintedPreview || entry.covered.size) return;
  const m = await pool.submit({
    type: 'frame', genomeJson: entry.record.genome, phase: 0, ...PREVIEW,
  }).done;
  if (m.type === 'done' && !entry.covered.size) {
    paintTo(entry.canvas, m);
    entry.paintedPreview = true;
  }
}

// Tonemap a card's accumulated histogram and paint it. Throttled: at most one
// tonemap in flight per card, with one coalesced repaint queued behind it.
function repaint(entry) {
  if (!entry.acc0 || !entry.covered.size) return;
  if (entry.tonemapPending) { entry.repaintQueued = true; return; }
  entry.tonemapPending = true;
  // Copy: the worker takes ownership of the transferred buffer.
  const hist = entry.acc0.slice().buffer;
  pool.submit({
    type: 'tonemap-int', hist, genomeJson: entry.record.genome,
    w: BATCH_SPEC.width, h: BATCH_SPEC.height, ss: BATCH_SPEC.ss,
  }).done.then((m) => {
    entry.tonemapPending = false;
    if (m.type === 'done' && cards.has(entry.record.id)) {
      paintTo(entry.canvas, m);
      entry.paintedPreview = true;
    }
    if (entry.repaintQueued) { entry.repaintQueued = false; repaint(entry); }
  }).catch((e) => { entry.tonemapPending = false; console.error(e); });
}

// ---- network callbacks ------------------------------------------------------

// A new batch contribution arrived. If it's for a shown card's displayed frame,
// render it locally (deterministic) and merge into that card's accumulation, so
// the card visibly sharpens as the community contributes. Always rebuild (the
// tally changed) and refresh the tally label.
async function onBatch(rec) {
  scheduleRebuild();
  const entry = cards.get(rec.sheepId);
  if (entry) {
    updateTally(rec.sheepId);
    if (rec.frame === CARD_FRAME && !entry.covered.has(rec.idx) && !banned.has(rec.contributor)) {
      const reply = await pool.submit(renderBatchMsg(entry.record, CARD_FRAME, rec.idx)).done;
      if (reply.type === 'batch-done' && mergeBatchInto(entry, rec.idx, new BigUint64Array(reply.hist))) {
        repaint(entry);
      }
    }
  }
}

// A verified merged histogram for (sheep, frame) arrived from the swarm (it
// already passed the Verification gate in net.js). Merge it wholesale into the
// card's accumulation. We can't dedup at idx granularity here (it's a sum of
// many batches), so only adopt it if it covers strictly more than we hold.
async function onRender(sheepId, frame, hist) {
  if (frame !== CARD_FRAME) return;
  const entry = cards.get(sheepId);
  if (!entry) return;
  // Count the merged render's batches via store coverage; only adopt if it
  // beats our current coverage (avoids double-counting our own contributions).
  const merged = (await store.batchesForSheep(sheepId)).filter((b) => b.frame === frame).length;
  if (merged <= entry.covered.size) return;
  entry.acc0 = hist instanceof BigUint64Array ? hist.slice() : new BigUint64Array(hist);
  entry.covered = new Set(
    (await store.batchesForSheep(sheepId)).filter((b) => b.frame === frame).map((b) => b.idx));
  repaint(entry);
}

// ---- the Verification gate --------------------------------------------------
//
// THE headline security property. A merged histogram offered by a peer is never
// trusted: we re-render a random sample of the batches it claims to contain,
// check each against the stored contribution record's signed hash, integer-
// subtract it from the histogram (no cell may underflow ⇒ it's genuinely
// present), and confirm the histogram's total sample count equals exactly the
// sum of the claimed batches' spp. Any hash mismatch publishes a fraud proof.

async function verifyRender(lookupSheep, { sheepId, frame, hist, batchKeys }) {
  const sheep = await lookupSheep(sheepId);
  if (!sheep) return false;
  if (!(hist instanceof BigUint64Array) || hist.length !== HIST_CELLS) return false;
  if (!Array.isArray(batchKeys) || !batchKeys.length) return false;

  // Map the claimed batchKeys to stored records (need their signed hashes + spp).
  const records = await store.batchesForSheep(sheepId);
  const byKey = new Map(records.map((b) => [b.key ?? batchKey(b), b]));
  const claimed = [];
  for (const k of batchKeys) {
    const b = byKey.get(k);
    if (!b || b.frame !== frame) return false; // claims a batch we can't account for
    if (banned.has(b.contributor)) return false; // tainted source
    claimed.push(b);
  }

  // Count conservation: total plotted count must equal the sum of claimed spp.
  const totalReply = await pool.submit({
    type: 'total-count', hist: hist.slice().buffer,
    w: BATCH_SPEC.width, h: BATCH_SPEC.height, ss: BATCH_SPEC.ss,
  }).done;
  if (totalReply.type !== 'done') return false;
  let expected = 0n;
  for (const b of claimed) expected += BigInt(b.spp);
  if (BigInt(totalReply.count) !== expected) return false;

  // Spot re-render: a random sample of the claimed batches. Each must hash to
  // its record's hash AND subtract cleanly from the merged histogram.
  const sample = shuffle(claimed.slice()).slice(0, Math.min(4, claimed.length));
  for (const b of sample) {
    const reply = await pool.submit(renderBatchMsg(sheep, frame, b.idx)).done;
    if (reply.type !== 'batch-done') return false;
    if (reply.hash !== b.hash) {
      // The contributor signed a hash that doesn't match the true render:
      // provable fraud. Publish it (bans them everywhere).
      await net.publishFraud({
        batchKey: b.key ?? batchKey(b), expected: reply.hash, reporter: me.pubHex,
        sig: await sign(me.pair, batchFraudBytes(b, reply.hash)),
      });
      return false;
    }
    const sub = await pool.submit({
      type: 'subtract-check', acc: hist.slice().buffer, batch: reply.hist,
      w: BATCH_SPEC.width, h: BATCH_SPEC.height, ss: BATCH_SPEC.ss,
    }).done;
    if (sub.type !== 'done' || !sub.ok) return false; // batch not actually present
  }
  return true;
}

// Sign-bytes for a fraud proof against batch `b` with the true hash.
const batchFraudBytes = (b, expected) =>
  fraudSignBytes({ batchKey: b.key ?? batchKey(b), expected, reporter: me.pubHex });

function shuffle(a) {
  for (let i = a.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [a[i], a[j]] = [a[j], a[i]];
  }
  return a;
}

// ---- idle contribution loop -------------------------------------------------
//
// A background loop that, when the pool has a free slot, renders the next
// un-rendered frame-0 batch for a visible/living card (round-robin), merges it
// into the card's accumulation, signs+publishes a batch record (which earns the
// sheep a vote), and re-tonemaps. Honest users never do makework: the work IS
// the sheep getting better.

const CONTRIB_CADENCE_MS = 250; // small breather between contributions
let contribCursor = 0;

function startContributeLoop() {
  const tick = () => {
    contributeStep()
      .catch(console.error)
      .finally(() => setTimeout(tick, CONTRIB_CADENCE_MS));
  };
  setTimeout(tick, 1500); // let the flock settle before contributing
}

// Round-robin over visible (or pledged) living cards; render the lowest free
// frame-0 idx for the chosen card. Returns the sheepId contributed to, or null.
async function contributeStep(force = false) {
  // Don't starve the UI: only contribute when the pool has a free slot.
  if (!force && pool.running >= pool.size) return null;
  const list = [...cards.values()];
  if (!list.length) return null;
  // Prefer visible-on-screen and explicitly pledged cards; fall back to all.
  let pickable = list.filter((e) => e.pledged || e.onScreen);
  if (!pickable.length) pickable = list;
  // Round-robin starting point.
  contribCursor = (contribCursor + 1) % pickable.length;
  for (let n = 0; n < pickable.length; n++) {
    const entry = pickable[(contribCursor + n) % pickable.length];
    const idx = nextFreeIdx(entry);
    const sheepId = await contributeBatch(entry, idx);
    if (sheepId) return sheepId;
  }
  return null;
}

// Lowest frame-0 idx this peer hasn't merged/seen yet for a card.
function nextFreeIdx(entry) {
  let i = 0;
  while (entry.covered.has(i)) i++;
  return i;
}

// Render frame-0 batch (entry, idx), merge it, publish the contribution record,
// re-tonemap. Returns the sheepId on a fresh contribution, else null.
async function contributeBatch(entry, idx) {
  if (entry.covered.has(idx)) return null;
  // Mark covered up-front so concurrent steps don't pick the same idx.
  entry.covered.add(idx);
  let reply;
  try {
    reply = await pool.submit(renderBatchMsg(entry.record, CARD_FRAME, idx)).done;
  } catch (e) { entry.covered.delete(idx); throw e; }
  if (reply.type !== 'batch-done') { entry.covered.delete(idx); return null; }

  if (!entry.acc0) entry.acc0 = new BigUint64Array(HIST_CELLS);
  mergeHist(entry.acc0, new BigUint64Array(reply.hist));
  repaint(entry);

  const g = gen();
  const record = {
    sheepId: entry.record.id, frame: CARD_FRAME, idx, hash: reply.hash,
    spp: BATCH_SPP, contributor: me.pubHex, gen: g,
  };
  record.sig = await sign(me.pair, batchSignBytes(record));
  await net.publishBatch(record);
  updateTally(entry.record.id);
  return entry.record.id;
}

// ---- replay ticker (fullscreen handoff only) --------------------------------
//
// Cards are stills now; the rAF/IntersectionObserver machinery is kept lean
// for the visibility gate that the contribution loop reads (onScreen). Cards
// don't animate in the flock view — the fullscreen sheep.html does.

const flockVisibility = new IntersectionObserver((records) => {
  for (const r of records) {
    const entry = cards.get(r.target.dataset.id);
    if (entry) entry.onScreen = r.isIntersecting;
  }
}, { rootMargin: '120px' });

// stopReplay is a no-op placeholder kept so rebuildFlock's teardown path reads
// cleanly (cards hold no live bitmaps in the still flock view).
function stopReplay(_entry) { /* cards are stills; nothing to release */ }

// ---- tally ------------------------------------------------------------------

function updateTally(sheepId) {
  const entry = cards.get(sheepId);
  if (!entry) return;
  entry.tallyEl.textContent = '…';
  net.tally(sheepId, gen()).then((n) => {
    if (!cards.has(sheepId)) return;
    entry.tallyEl.textContent = n > 0 ? `${n} ♥` : '';
    entry.tallyEl.title = `${n} verified batches this generation`;
  }).catch(console.error);
}

// ---- breeding lab -----------------------------------------------------------

function toggleSelect(sheepId) {
  const at = selected.indexOf(sheepId);
  if (at !== -1) selected.splice(at, 1);
  else {
    if (selected.length === 2) deselect(selected.shift());
    selected.push(sheepId);
  }
  cards.get(sheepId)?.card.classList.toggle('selected', selected.includes(sheepId));
  if (selected.length === 2) breedSelected().catch(showError);
  else $('#nursery').hidden = true;
}

function deselect(sheepId) {
  cards.get(sheepId)?.card.classList.remove('selected');
}

async function breedSelected() {
  // Sort so a pair has ONE canonical child regardless of click order.
  const [aId, bId] = [...selected].sort();
  const a = cards.get(aId).record;
  const b = cards.get(bId).record;
  const g = gen();
  // Same formula the generation engine uses — this preview IS the exact child
  // these two would have if they both survive this generation.
  const challengeHex = await breedChallenge(g, aId, bId);

  $('#nursery').hidden = false;
  $('#nursery-note').textContent = 'breeding…';
  $('#release').hidden = true;

  const { type, childJson, childId } = await pool.submit({
    type: 'breed', aJson: a.genome, bJson: b.genome, challengeHex,
  }).done;
  if (type !== 'breed-done') return;
  // Stale? (selection changed while breeding)
  if (selected.length !== 2 || [...selected].sort().join() !== [aId, bId].join()) return;

  $('#nursery-note').textContent =
    `the canonical child of ${aId.slice(0, 8)} × ${bId.slice(0, 8)} (gen ${g})`;

  // Preview the child as a quick frame (display-only; the child earns real
  // pixels once it's in the flock and gets contributed to).
  const canvas = $('#child-canvas');
  const preview = await pool.submit({
    type: 'frame', genomeJson: childJson, phase: 0, ...PREVIEW,
  }).done;
  if (preview.type === 'done') paintTo(canvas, preview);

  const release = $('#release');
  release.hidden = false;
  release.disabled = cards.has(childId);
  release.textContent = cards.has(childId) ? 'already in flock' : 'release';
  release.onclick = async () => {
    release.disabled = true;
    // No render proof needed to release now — a release earns votes by being
    // contributed to like any sheep. The release is a signed sheep record.
    const rg = gen();
    const record = {
      id: childId, genome: childJson, parents: [aId, bId], gen: rg,
      origin: 'release', author: me.pubHex,
    };
    record.sig = await sign(me.pair, sheepSignBytes(record));
    await net.publishSheep(record);
    release.textContent = 'released ✓';
    scheduleRebuild();
  };
}

// ---- chrome -----------------------------------------------------------------

let swarmLinkSet = false;
function updateStatus() {
  if (!swarmLinkSet) {
    const link = document.querySelector('a[href="swarm.html"]');
    if (link && PEER_NS !== '0') link.href = `swarm.html?peer=${PEER_NS}`;
    swarmLinkSet = true;
  }
  const msLeft = GEN_MS - (Date.now() % GEN_MS);
  const mm = String(Math.floor(msLeft / 60_000));
  const ss = String(Math.floor((msLeft % 60_000) / 1000)).padStart(2, '0');
  const a = auditor?.stats ?? { audits: 0, frauds: 0 };
  $('#status').textContent =
    `gen ${gen() - GENESIS_GEN} closes in ${mm}:${ss} · ` +
    `you are ${handle(me.pubHex)} · ${net.peerCount()} peers · ` +
    `${a.audits} audits${a.frauds ? `, ${a.frauds} frauds!` : ''}`;
}

function showError(err) {
  console.error(err);
  $('#status').textContent = 'error: ' + (err?.message || err);
}

// ---- debug + stress hooks ---------------------------------------------------

function installDebugHooks() {
  window.__sheepStats = {
    get audits() { return auditor.stats.audits; },
    get frauds() { return auditor.stats.frauds; },
    get banned() { return [...banned]; },
    get renders() { return store.allRenderKeys().then((k) => k.length); },
    get pool() {
      return { queued: pool.queue.length, running: pool.running, chunks: pool.chunksRendered };
    },
  };
}

function installStressHooks() {
  window.__sheepAct = {
    // Render+publish ONE batch (frame 0, next free idx) for a random living
    // card; resolves with the sheepId or null.
    async contributeRandom() {
      const list = [...cards.values()];
      if (!list.length) return null;
      const entry = list[Math.floor(Math.random() * list.length)];
      return contributeBatch(entry, nextFreeIdx(entry));
    },
    // Breed two random living sheep, release the child (publishSheep), return
    // the childId or null.
    async breedRandom() {
      const ids = [...cards.keys()];
      if (ids.length < 2) return null;
      const aId = ids[Math.floor(Math.random() * ids.length)];
      let bId = aId;
      while (bId === aId) bId = ids[Math.floor(Math.random() * ids.length)];
      const [x, y] = [aId, bId].sort();
      const g = gen();
      const challengeHex = await breedChallenge(g, x, y);
      const bred = await pool.submit({
        type: 'breed', aJson: cards.get(x).record.genome,
        bJson: cards.get(y).record.genome, challengeHex,
      }).done;
      if (bred.type !== 'breed-done' || cards.has(bred.childId)) return null;
      const record = {
        id: bred.childId, genome: bred.childJson, parents: [x, y], gen: g,
        origin: 'release', author: me.pubHex,
      };
      record.sig = await sign(me.pair, sheepSignBytes(record));
      await net.publishSheep(record);
      scheduleRebuild();
      return bred.childId;
    },
  };

  window.__sheepDump = async () => {
    const [sheep, batches, fraud, renderKeys] = await Promise.all([
      store.allSheep(), store.allBatches(), store.allFraud(), store.allRenderKeys(),
    ]);
    // Convergence fingerprint: current-gen [sheepId, batchCount] for non-banned
    // contributors, sorted, hashed. Two converged peers agree on this.
    const g = gen();
    const tally = new Map();
    for (const b of batches) {
      if (b.gen !== g || banned.has(b.contributor)) continue;
      tally.set(b.sheepId, (tally.get(b.sheepId) || 0) + 1);
    }
    const t = [...tally.entries()].sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
    return {
      peer: PEER_NS, pub: me.pubHex, gen: g - GENESIS_GEN,
      sheep: sheep.length, batches: batches.length, fraud: fraud.length,
      cards: cards.size, renders: renderKeys.length,
      audits: auditor.stats.audits, frauds: auditor.stats.frauds,
      net: JSON.parse(JSON.stringify(net.counts)),
      tallyFingerprint: await sha256Hex(utf8(JSON.stringify(t))),
    };
  };

  // Expose the verification gate so the e2e can prove directly that a forged
  // render (bytes that don't match the claimed batches) is rejected.
  // Resolve a sheep's genome from the live flock (cards hold every living
  // sheep, incl. derived children that aren't store facts), then baked, then
  // store — without the cards path peers drop batches for bred children.
  const lookupSheep = async (id) =>
    cards.get(id)?.record
    ?? baked.find((s) => s.id === id)
    ?? (await store.allSheep()).find((s) => s.id === id);
  window.__sheepVerify = (arg) => verifyRender(lookupSheep, arg);
}

main().catch(showError);
