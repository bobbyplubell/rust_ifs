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
  voteSignBytes, BREED_MIN_TILES, PROTOCOL_VERSION, specForGen, specCells,
} from './net.js';
import { computeFlock, breedChallenge } from './gens.js';
import { handle, provenance, sheepName } from './names.js';
import { Auditor } from './audit.js';
import { FrameLoop } from './loop.js';
import { RELAYS } from '../config.js';

const $ = (s) => document.querySelector(s);
const params = new URLSearchParams(location.search);
// ?workers=N caps the worker pool (stress testing packs hundreds of peers on
// one machine; idle peers don't need 4 workers each).
const WORKERS_OVERRIDE = Number(params.get('workers')) || null;
const pool = new WorkerPool(WORKERS_OVERRIDE ?? undefined);

// Contribution is MANUAL: the gallery passively shows what the swarm has
// rendered so far; rendering work (and the vote it earns) only happens for
// sheep the user explicitly pledges via the "contribute" button. Load/headless
// modes auto-contribute to keep generating work without a human.
const AUTO_CONTRIBUTE = !!(params.get('stress') || params.get('autocontribute'));
// ?fetchonly: CPU-light viewing. Don't re-render gossiped tiles; instead fetch
// the swarm's accumulated render (one transfer + a sampled verify) and display
// that. The cheap path the lossless-compressed render-data was built for.
const FETCH_ONLY = !!params.get('fetchonly');
let fetchedRenders = 0; // verified renders adopted from peers (vs rendered locally)

// Frame every card shows. The flock is a still gallery of frame 0; the
// fullscreen view (sheep.html) animates the whole loop.
const CARD_FRAME = 0;              // the frame each flock card displays
// A sheep's render spec is keyed to its birth generation (so specs can change
// for new sheep without breaking old ones). Resolve it per record.
const specOf = (record) => specForGen(record.gen);
// Contribution targets frame 0 until it has at least this many tiles (so the
// thumbnail still looks decent), then spreads tiles across other frames to
// build the whole community animation — not just the first frame.
const FRAME0_MIN = 8;
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

// Build stamp (written by the deploy workflow), so you can tell which build is
// live. Absent in local dev → shows nothing.
let buildVersion = '';
// Resolve before importing the libp2p bundle so we can cache-bust it: GitHub
// Pages serves the 620 KB bundle with a 10-min cache, so without busting a
// connectivity fix can't reach a returning peer until that cache expires — the
// "I deployed but it didn't take effect" trap.
// no-store: version.txt must be read FRESH, or its own 10-min cache hands back a
// stale build id and we'd cache-bust the bundle to the wrong (old) version.
const versionReady = fetch('version.txt', { cache: 'no-store' })
  .then((r) => (r.ok ? r.text() : '')).then((t) => (buildVersion = t.trim())).catch(() => '');
let shownGen = -1;
let lastPrune = 0;          // throttle render-cache eviction (Date.now() ms)
let banned = new Set();     // contributors with verified fraud proofs (local view)

async function main() {
  me = await loadIdentity();
  store = await openStore();

  // Tabs always talk via BroadcastChannel; the internet swarm joins in when
  // relays are configured (libp2p bundle loaded lazily, failure non-fatal).
  // Relay set: the hardcoded bootstrap (config.js) MERGED with relays discovered
  // via gossip and persisted to localStorage — so the set grows to the whole
  // community without dropping the bootstrap. `?relay=<multiaddr>` replaces the
  // lot (for pointing at a local relay); `?stress` pages take NO production relay
  // unless one is passed explicitly, so e2e/the docker harness can't leak test
  // sheep+votes into the live swarm.
  const explicitRelay = params.get('relay');
  const discoveredRelays = (localStorage.getItem('relays') || '')
    .split(',').map((s) => s.trim()).filter(Boolean);
  const relays = explicitRelay ? [explicitRelay]
    : params.get('stress') ? []
    : [...new Set([...RELAYS, ...discoveredRelays])];
  const transport = new CompositeTransport([new BroadcastTransport()]);
  // Connect libp2p in the BACKGROUND — the 620 KB bundle + relay handshake takes
  // seconds, and blocking boot on it makes the whole page (incl. buttons)
  // unresponsive until it lands. Anti-entropy's running inv timer carries the
  // link into sync the moment it joins.
  if (relays.length) {
    (async () => {
      try {
        await versionReady;
        const bust = buildVersion ? `?v=${buildVersion}` : '';
        const { createLibp2pTransport } = await import(`./vendor/libp2p.js${bust}`);
        const stun = (params.get('stun') || '').split(',').map((s) => s.trim()).filter(Boolean);
        const lp = await createLibp2pTransport({ relays, stun });
        transport.add(lp);
        window.__libp2p = lp.node; // diagnostics: connection/discovery inspection
        console.log('libp2p transport up; relays:', relays);
      } catch (err) {
        console.error('libp2p transport unavailable:', err);
      }
    })();
  }

  // Resolve a sheep's genome from the live flock (cards hold every living
  // sheep, incl. derived children that aren't store facts), then baked, then
  // store — without the cards path peers drop batches for bred children.
  const lookupSheep = async (id) =>
    cards.get(id)?.record
    ?? baked.find((s) => s.id === id)
    ?? (await store.allSheep()).find((s) => s.id === id);

  net = new Net({
    transport,
    store,
    identity: { pubHex: me.pubHex, pair: me.pair },
    sign,
    verify,
    // Re-render one batch (hash only) — net uses this to confirm a fraud claim.
    // The spec is the sheep's (keyed to its birth gen), not a global constant.
    checkBatchHash: async (genomeJson, sheepId, frame, idx) => {
      const s = specForGen((await lookupSheep(sheepId))?.gen ?? gen());
      return pool.submit({
        type: 'batch-hash', genomeJson, sheepId, frame, idx,
        w: s.width, h: s.height, ss: s.ss, spp: s.spp, nFrames: s.nFrames,
      }).done.then((m) => m.hash);
    },
    checkSheepId: (genomeJson) =>
      pool.submit({ type: 'sheep-id', genomeJson }).done.then((m) => m.id),
    lookupSheep,
    verifyRender: (args) => verifyRender(lookupSheep, args),
    onSheep: () => scheduleRebuild(),
    onBatch: (rec) => onBatch(rec).catch(console.error),
    onFraud: () => scheduleRebuild(), // a discredited contributor changes tallies
    onRender: (sheepId, frame, hist, keys) => onRender(sheepId, frame, hist, keys).catch(console.error),
    onVote: (rec) => {
      markTally(rec.sheepId);                     // this sheep's backing changed
      if (rec.from === me.pubHex) creditsDirty = true; // only my own spend moves my balance
      if (rec.gen < gen()) scheduleRebuild();     // retroactive: a closed gen's outcome moved
    },
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

  startVersionPoll();
  shownGen = gen();
  setInterval(() => {
    if (gen() !== shownGen) {
      shownGen = gen();
      voteSeqByGen.clear(); // new gen: vote seq restarts
      creditsDirty = true;  // new gen: credits reset (use-it-or-lose-it)
      scheduleRebuild();    // generation closed: survivors chosen, children born
    }
    refreshTallies().catch(console.error); // batched backing repaint
    refreshCredits().catch(console.error); // refresh cached credit balance
    refreshTileTotals().catch(console.error); // per-card + global tile totals
    updateStatus();     // shows the activity pulse + credit balance
    batchActivity = 0;  // reset the per-tick pulse after showing it
  }, 1000);
  // A backgrounded or slept tab has its timers throttled/frozen, so the 1 Hz
  // gen-advance tick above stops firing — and the displayed flock freezes at an
  // OLD generation (showing immigrants from a fresh-blood window that has since
  // slid past). That's the "overnight client shows different sheep" bug: same
  // code, same facts, but a stale snapshot. On becoming visible again, force a
  // catch-up to the CURRENT generation so every client at gen G shows the same
  // flock.
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && gen() !== shownGen) {
      shownGen = gen();
      voteSeqByGen.clear();
      creditsDirty = true;
      scheduleRebuild();
    }
  });
  updateStatus();
}

// ---- auto-update on new deploy ----------------------------------------------
//
// The on-screen flock is a deterministic function of (wall-clock generation,
// synced facts, AND this JS code version): two clients on DIFFERENT bundles
// compute DIFFERENT flocks from the same facts and diverge. A tab left open for
// days never reloads, so it silently falls behind newer deploys. Poll the build
// stamp and reload once when it changes, so long-running clients converge on the
// latest code with everyone else.
const VERSION_POLL_MS = 5 * 60_000; // re-check the deployed build every 5 min
const VERSION_SETTLE_MS = 60_000;   // let the page settle before the first poll
function startVersionPoll() {
  // buildVersion (set via versionReady at load) is the version THIS page is
  // running. It's fixed for the life of the tab, so after a reload it equals the
  // freshly deployed build → fetched === loaded → no further reload. That's the
  // loop guard: we reload exactly once per deploy.
  setTimeout(() => setInterval(async () => {
    const loaded = buildVersion;
    if (!loaded) return; // never reloaded with an unknown loaded version
    const fetched = await fetch('version.txt', { cache: 'no-store' })
      .then((r) => (r.ok ? r.text() : '')).then((t) => t.trim()).catch(() => '');
    if (fetched && loaded && fetched !== loaded) {
      // Only reload a BACKGROUNDED tab — never interrupt active use during a
      // deploy. A hidden tab updates silently; a foreground one keeps its loaded
      // version (the loop guard still holds), so it'll pick up the new build the
      // next time it's hidden, exactly once.
      if (document.hidden) {
        console.log('new build ' + fetched + ' (was ' + loaded + ') — reloading hidden tab to stay in sync');
        location.reload();
      }
    }
  }, VERSION_POLL_MS), VERSION_SETTLE_MS);
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

  const { living, history } = await computeFlock({
    store, baked, breedFn, mutateFn, randomFn, banned,
  });
  // Evict the render cache (heavy histogram buffers, the main storage cost) for
  // sheep that are neither living nor enshrined in the Hall of Fame. Re-derivable
  // and on-demand-synced, so this is safe; throttled so it doesn't run every
  // rebuild. Without it the cache grows unbounded (hundreds of MB seen).
  if (Date.now() - lastPrune > 60_000) {
    lastPrune = Date.now();
    const keep = new Set([...living.keys(), ...history.map((h) => h.record.id)]);
    store.pruneRenders(keep)
      .then((n) => { if (n) console.log(`pruned ${n} cached renders`); })
      .catch(() => {});
  }
  for (const [id, entry] of cards) {
    if (!living.has(id)) {
      stopReplay(entry);
      disposeHover(entry);
      flockVisibility.unobserve(entry.card);
      const at = selected.indexOf(id);
      if (at !== -1) { selected.splice(at, 1); showNurseryHint(); }
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

  const spec = specOf(record);
  const card = document.createElement('div');
  card.className = 'card';
  card.dataset.id = record.id;
  const canvas = document.createElement('canvas');
  canvas.width = spec.width;
  canvas.height = spec.height;
  const meta = document.createElement('div');
  meta.className = 'meta';
  // The PRIMARY name shown is the unique id-derived name (sheepName); the
  // origin sentence (provenance.how) lives in the hover title as context.
  const prov = provenance(record);
  const label = document.createElement('a');
  label.textContent = sheepName(record);
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
  contribBtn.title = 'pledge idle CPU to this sheep — every batch sharpens it ' +
    'for everyone; every 128 tiles earns you one credit to spend on selection';
  const backBtn = document.createElement('button');
  backBtn.className = 'back';
  backBtn.textContent = 'back ▲';
  backBtn.title = 'spend one earned credit to back this sheep — backing (not render ' +
    'work) decides who survives the generation';
  meta.append(label, tallyEl, contribBtn, backBtn);
  const bar = document.createElement('div');
  bar.className = 'bar';
  const barFill = document.createElement('div');
  barFill.className = 'bar-fill';
  bar.append(barFill);
  // Per-sheep tile totals: your contributed tiles vs the whole swarm's, for
  // THIS sheep. Filled by the 1 Hz tile scan (refreshTileTotals).
  const tilesEl = document.createElement('div');
  tilesEl.className = 'tiles';
  card.append(canvas, bar, meta, tilesEl);
  $('#flock').append(card);

  const entry = {
    record, canvas, tallyEl, tilesEl, contribBtn, card, barFill, onScreen: true,
    spec, histCells: specCells(spec),
    acc0: null, covered: new Set(),
    tonemapPending: false, repaintQueued: false, paintedPreview: false,
    pledged: false,
  };
  // Per-frame idx coverage for the WHOLE loop (so contribution can spread
  // across frames). frame 0's set IS entry.covered (display accumulation), so
  // the two never drift.
  entry.frameCov = new Map([[CARD_FRAME, entry.covered]]);
  cards.set(record.id, entry);
  flockVisibility.observe(card);

  canvas.addEventListener('click', () => toggleSelect(record.id));
  card.addEventListener('mouseenter', () => startHover(entry));
  card.addEventListener('mouseleave', () => stopHover(entry));
  contribBtn.addEventListener('click', () => {
    entry.pledged = !entry.pledged;
    contribBtn.textContent = entry.pledged ? 'pledged ✓' : 'contribute';
    contribBtn.classList.toggle('on', entry.pledged);
  });
  backBtn.addEventListener('click', () => backSheep(entry).catch(showError));

  // Bootstrap this card's accumulation from any batches we already hold for
  // frame 0, then paint a quick preview placeholder, then the tally.
  bootstrapCard(entry).catch(showError);
  updateTally(record.id);
}

// ---- hover-to-animate (flock, display-only) ---------------------------------
//
// Hovering a card plays a quick animated preview of the sheep's loop — cheap
// render_frame previews cross-faded by a FrameLoop, NOT contributed work (no
// batches, no votes). It shows the motion; the fullscreen view is where the
// community's accumulated high-quality loop lives. Preview frames render lazily
// and are cached, so a second hover is instant.
const HOVER_FRAMES = 24;
const HOVER_PREVIEW = { width: 256, height: 256, samples: 130_000, seed: 7 };

async function ensureHoverFrames(entry) {
  if (entry.hoverFrames) return;
  entry.hoverFrames = new Array(HOVER_FRAMES).fill(null);
  for (let f = 0; f < HOVER_FRAMES; f++) {
    if (!entry.hovering) return; // left before it filled — keep what we have
    const m = await pool.submit({
      type: 'frame', genomeJson: entry.record.genome,
      phase: f / HOVER_FRAMES, ...HOVER_PREVIEW,
    }).done;
    if (m.type === 'done') {
      entry.hoverFrames[f] = await createImageBitmap(
        new ImageData(new Uint8ClampedArray(m.rgba), m.width, m.height));
    }
  }
}

function startHover(entry) {
  entry.hovering = true;
  if (!entry.hoverLoop) {
    entry.hoverLoop = new FrameLoop(entry.canvas, {
      nFrames: HOVER_FRAMES,
      getFrame: (i) => entry.hoverFrames?.[i] || null,
    });
  }
  ensureHoverFrames(entry).catch(showError);
  entry.hoverLoop.start();
}

function stopHover(entry) {
  entry.hovering = false;
  entry.hoverLoop?.stop();
  // Restore the still: the accumulated frame-0 render, or its frame-0 preview.
  if (entry.acc0 && entry.covered.size) repaint(entry);
  else if (entry.hoverFrames?.[0]) {
    const c = entry.canvas;
    c.getContext('2d').drawImage(entry.hoverFrames[0], 0, 0, c.width, c.height);
  }
}

function disposeHover(entry) {
  entry.hoverLoop?.stop();
  entry.hovering = false;
  if (entry.hoverFrames) {
    for (const b of entry.hoverFrames) b?.close?.();
    entry.hoverFrames = null;
  }
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
  for (const b of await store.batchesForSheep(entry.record.id)) {
    if (banned.has(b.contributor)) continue;
    if (b.frame === CARD_FRAME) {
      if (entry.covered.has(b.idx)) continue;
      const reply = await pool.submit(renderBatchMsg(entry.record, CARD_FRAME, b.idx)).done;
      if (reply.type !== 'batch-done') continue;
      mergeBatchInto(entry, b.idx, new BigUint64Array(reply.hist));
    } else {
      coveredFor(entry, b.frame).add(b.idx); // track other frames' coverage
    }
  }
  if (entry.covered.size) repaint(entry);
  else paintPreview(entry);
}

function renderBatchMsg(record, frame, idx) {
  const s = specOf(record);
  return {
    type: 'render-batch', genomeJson: record.genome, sheepId: record.id,
    frame, idx, w: s.width, h: s.height, ss: s.ss, spp: s.spp, nFrames: s.nFrames,
  };
}

// Merge a rendered frame-0 batch histogram into a card's accumulation.
function mergeBatchInto(entry, idx, hist) {
  if (entry.covered.has(idx)) return false;
  if (!entry.acc0) entry.acc0 = new BigUint64Array(entry.histCells);
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

// Seed the render store with this card's accumulation so peers can fetch it
// (cheap viewing: they verify a sample of tiles instead of re-rendering all).
// Throttled — the histogram is multi-MB. Persists across reloads as a bonus.
const SEED_EVERY_MS = 5000;
function maybeSeed(entry) {
  if (!entry.acc0 || !entry.covered.size) return;
  const now = Date.now();
  if (now - (entry.lastSeed || 0) < SEED_EVERY_MS) return;
  entry.lastSeed = now;
  // Keys are EXACTLY the tiles in acc0 (entry.covered) → render + keys consistent.
  const keys = [...entry.covered].map((i) => `${entry.record.id}:${CARD_FRAME}:${i}`);
  store.putRender(entry.record.id, CARD_FRAME, entry.acc0.slice().buffer, keys).catch(console.error);
}

// Tonemap a card's accumulated histogram and paint it. Throttled: at most one
// tonemap in flight per card, with one coalesced repaint queued behind it.
function repaint(entry) {
  if (!entry.acc0 || !entry.covered.size) return;
  maybeSeed(entry);
  if (entry.tonemapPending) { entry.repaintQueued = true; return; }
  entry.tonemapPending = true;
  // Copy: the worker takes ownership of the transferred buffer.
  const hist = entry.acc0.slice().buffer;
  pool.submit({
    type: 'tonemap-int', hist, genomeJson: entry.record.genome,
    w: entry.spec.width, h: entry.spec.height, ss: entry.spec.ss,
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
  batchActivity++;
  // A batch *I* contributed — including from the fullscreen view in another tab,
  // which arrives here over the local BroadcastChannel — builds my credit
  // balance. Mark it dirty even if this sheep isn't carded below, otherwise the
  // balance looks frozen while you render in fullscreen.
  if (rec.contributor === me.pubHex) creditsDirty = true;
  // Batches change tallies, not flock MEMBERSHIP — membership only changes at a
  // generation close (or a new release, via onSheep). So we do NOT rebuild the
  // flock per-batch (that was the gallery churn); only a retroactive batch for
  // an already-closed generation can rewrite lineage.
  if (rec.gen < gen()) scheduleRebuild();
  const entry = cards.get(rec.sheepId);
  if (!entry || banned.has(rec.contributor)) return;
  markTally(rec.sheepId);
  if (rec.frame === CARD_FRAME) {
    if (FETCH_ONLY) {
      maybeFetch(entry); // pull the verified accumulation instead of rendering
    } else if (!entry.covered.has(rec.idx)) {
      // The displayed frame: render it locally and merge so the card sharpens.
      const reply = await pool.submit(renderBatchMsg(entry.record, CARD_FRAME, rec.idx)).done;
      if (reply.type === 'batch-done' && mergeBatchInto(entry, rec.idx, new BigUint64Array(reply.hist))) {
        repaint(entry);
      }
    }
  } else {
    coveredFor(entry, rec.frame).add(rec.idx); // other frames: track coverage
  }
}

// Fetch-only viewing: pull the swarm's accumulated frame-0 render (verified)
// instead of rendering each gossiped tile ourselves. Throttled per card.
const FETCH_EVERY_MS = 4000;
function maybeFetch(entry) {
  const now = Date.now();
  if (now - (entry.lastFetch || 0) < FETCH_EVERY_MS) return;
  entry.lastFetch = now;
  net.requestRender(entry.record.id, CARD_FRAME);
}

// A verified merged histogram for (sheep, frame) arrived from the swarm (it
// already passed the Verification gate in net.js). Merge it wholesale into the
// card's accumulation. We can't dedup at idx granularity here (it's a sum of
// many batches), so only adopt it if it covers strictly more than we hold.
async function onRender(sheepId, frame, hist, batchKeys = []) {
  if (frame !== CARD_FRAME) return;
  const entry = cards.get(sheepId);
  if (!entry) return;
  // batchKeys are the tiles the verified histogram actually contains (the gate
  // checked them), so covered/acc0 stay consistent for re-seeding. Adopt only
  // if it beats what we already display.
  const idxs = batchKeys.map((k) => Number(k.split(':')[2])).filter((n) => Number.isInteger(n));
  if (idxs.length <= entry.covered.size) return;
  entry.acc0 = hist instanceof BigUint64Array ? hist.slice() : new BigUint64Array(hist);
  entry.covered = new Set(idxs);
  fetchedRenders++;
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
  const spec = specOf(sheep);
  if (!(hist instanceof BigUint64Array) || hist.length !== specCells(spec)) return false;
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
    w: spec.width, h: spec.height, ss: spec.ss,
  }).done;
  if (totalReply.type !== 'done') return false;
  let expected = 0n;
  for (const b of claimed) expected += BigInt(b.count);
  if (BigInt(totalReply.count) !== expected) return false;

  // Spot re-render: a random sample of the claimed batches. Each must hash to
  // its record's hash AND subtract cleanly from the merged histogram.
  const sample = shuffle(claimed.slice()).slice(0, Math.min(4, claimed.length));
  for (const b of sample) {
    const reply = await pool.submit(renderBatchMsg(sheep, frame, b.idx)).done;
    if (reply.type !== 'batch-done') return false;
    if (Number(reply.count) !== b.count) return false; // record's count was inflated
    if (reply.hash !== b.hash) {
      // The contributor signed a hash that doesn't match the true render:
      // provable fraud. Publish it (bans them everywhere).
      await net.publishFraud({
        v: PROTOCOL_VERSION,
        batchKey: b.key ?? batchKey(b), expected: reply.hash, reporter: me.pubHex,
        sig: await sign(me.pair, batchFraudBytes(b, reply.hash)),
      });
      return false;
    }
    const sub = await pool.submit({
      type: 'subtract-check', acc: hist.slice().buffer, batch: reply.hist,
      w: spec.width, h: spec.height, ss: spec.ss,
    }).done;
    if (sub.type !== 'done' || !sub.ok) return false; // batch not actually present
  }
  return true;
}

// Sign-bytes for a fraud proof against batch `b` with the true hash.
const batchFraudBytes = (b, expected) =>
  fraudSignBytes({ v: PROTOCOL_VERSION, batchKey: b.key ?? batchKey(b), expected, reporter: me.pubHex });

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

// When true, the background loop skips contributing. Used by the stress harness
// to quiesce the swarm during its settle window so convergence can be measured
// against a fixed batch set (a never-quiescent swarm always has an in-flight
// tail). No effect on normal use.
let contributePaused = false;

// Keep the worker pool SATURATED instead of rendering one tile at a time. The
// old loop awaited each contribution then slept, so it used a SINGLE worker no
// matter how many the pool had — making the worker count irrelevant. Batches are
// independent + deterministic, and contributeBatch reserves its idx
// synchronously before its first await, so firing pool.size of them at once
// renders distinct tiles in parallel with no collision. Only pledged sheep (or
// auto/stress) are pickable, so an idle viewer with nothing pledged still does
// no work — saturation only kicks in on explicit intent.
let contribInFlight = 0;
function pumpContribute() {
  if (contributePaused) return;
  while (contribInFlight < pool.size) {
    contribInFlight++;
    let did = false;
    contributeStep(true)
      .then((id) => { did = !!id; })
      .catch(console.error)
      .finally(() => {
        contribInFlight--;
        if (did && !contributePaused) pumpContribute(); // refill the freed slot
      });
  }
}

function startContributeLoop() {
  setTimeout(pumpContribute, 1500); // let the flock settle before contributing
  // Restart the pump if it ever fully drains (e.g. nothing pledged yet, then a
  // pledge appears) — cheap since an empty pickable set returns instantly.
  setInterval(() => { if (!contributePaused) pumpContribute(); }, CONTRIB_CADENCE_MS * 4);
}

// Round-robin over visible (or pledged) living cards; render the lowest free
// frame-0 idx for the chosen card. Returns the sheepId contributed to, or null.
async function contributeStep(force = false) {
  // Don't starve the UI: only contribute when the pool has a free slot.
  if (!force && pool.running >= pool.size) return null;
  const list = [...cards.values()];
  if (!list.length) return null;
  // Only render for sheep the user explicitly pledged to (manual contribution).
  // Auto mode (stress/headless) also takes on-screen cards to generate load.
  let pickable = list.filter((e) => e.pledged || (AUTO_CONTRIBUTE && e.onScreen));
  if (!pickable.length) return null;
  // Round-robin starting point.
  contribCursor = (contribCursor + 1) % pickable.length;
  for (let n = 0; n < pickable.length; n++) {
    const entry = pickable[(contribCursor + n) % pickable.length];
    const sheepId = await contributeBatch(entry);
    if (sheepId) return sheepId;
  }
  return null;
}

// The idx set this peer has rendered/seen for a given frame of a card. Frame 0
// is entry.covered (the displayed accumulation); others are tracked so we don't
// re-render a tile and so we can pick the next free idx.
function coveredFor(entry, frame) {
  let s = entry.frameCov.get(frame);
  if (!s) { s = new Set(); entry.frameCov.set(frame, s); }
  return s;
}

// The fuzziest frame = the one with the fewest tiles (every tile is an equal
// batch of samples, so fewest samples = most noise). Coverage is shared
// knowledge, so every peer targets the same fuzzy frame and the swarm fills it
// together — no per-frame histogram needed.
function leastCoveredFrame(entry) {
  let best = CARD_FRAME;
  let bestN = Infinity;
  for (let f = 0; f < entry.spec.nFrames; f++) {
    const n = entry.frameCov.get(f)?.size ?? 0;
    if (n < bestN) { bestN = n; best = f; }
  }
  return best;
}

// Which frame to contribute to next: frame 0 until it has a baseline of tiles
// (the thumbnail must look decent), then the fuzziest (least-rendered) frame,
// so contribution flows where it's needed and the whole loop converges evenly.
function pickFrame(entry) {
  return entry.covered.size < FRAME0_MIN ? CARD_FRAME : leastCoveredFrame(entry);
}

// idx is just a sample-batch SEED (chunk_seed = sha256(challenge ‖ le32(idx))),
// so ANY distinct u32 is a valid, independently-verifiable batch — tiles needn't
// be contiguous. Pick a RANDOM free idx from a large space rather than the
// lowest: the lowest-first scan only avoids duplicate work when coverage is
// SHARED (synced). Two peers contributing while unsynced both grind 0,1,2,… and
// render byte-identical duplicate tiles — so the slower peer's work is mostly a
// dupe of the faster one's, and the union after they finally sync is ~max(A,B)
// instead of ~A+B. Random picks make blind concurrent work disjoint (two subsets
// of a few thousand in a 16M space collide ~never), so unsynced peers' tiles
// ADD UP. Synced peers still skip covered idxs, so no double-work within a shared
// set either. Falls back to a dense scan only if a region ever saturates (it
// won't at <1% density). No protocol/golden impact: idx N still renders the same.
const IDX_SPACE = 1 << 24;
function nextFreeIdx(entry, frame) {
  const cov = coveredFor(entry, frame);
  for (let t = 0; t < 64; t++) {
    const i = Math.floor(Math.random() * IDX_SPACE);
    if (!cov.has(i)) return i;
  }
  let i = 0;
  while (cov.has(i)) i++;
  return i;
}

// Render one tile for the chosen frame, publish the contribution record
// (earning a vote), and — only for the displayed frame 0 — merge it into the
// card's accumulation and re-tonemap. Returns the sheepId, or null.
async function contributeBatch(entry) {
  const frame = pickFrame(entry);
  const idx = nextFreeIdx(entry, frame);
  const cov = coveredFor(entry, frame);
  if (cov.has(idx)) return null;
  cov.add(idx); // reserve up-front so concurrent steps don't collide
  let reply;
  try {
    reply = await pool.submit(renderBatchMsg(entry.record, frame, idx)).done;
  } catch (e) { cov.delete(idx); throw e; }
  if (reply.type !== 'batch-done') { cov.delete(idx); return null; }

  if (frame === CARD_FRAME) {
    if (!entry.acc0) entry.acc0 = new BigUint64Array(entry.histCells);
    mergeHist(entry.acc0, new BigUint64Array(reply.hist));
    repaint(entry);
  }

  const g = gen();
  const record = {
    v: PROTOCOL_VERSION,
    sheepId: entry.record.id, frame, idx, hash: reply.hash,
    spp: entry.spec.spp, count: Number(reply.count), contributor: me.pubHex, gen: g,
  };
  record.sig = await sign(me.pair, batchSignBytes(record));
  await net.publishBatch(record);
  creditsDirty = true; // earned a credit
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

// Update a card's tally IN PLACE — keep the old number visible until the new
// one resolves (no '…' flash). Display is decoupled from the firehose of
// incoming batches: onBatch just marks the tally dirty and the 1 Hz UI tick
// refreshes it, so the gallery never churns per-batch.
const tallyDirty = new Set();
let batchActivity = 0; // batches seen since the last tick (the "live" pulse)
const markTally = (sheepId) => tallyDirty.add(sheepId);

// A card's ♥ is its BACKING (credits spent on it this gen) — the selection
// score. Backing is a gen-wide computation, and rank is global (top-3 get
// medals — the live leaderboard), so recompute once and repaint ALL cards.
async function refreshTallies() {
  if (!tallyDirty.size) return;
  tallyDirty.clear();
  const backing = await net.tallies(gen());
  const ranked = [...cards.keys()]
    .map((id) => [id, backing.get(id) || 0])
    .filter(([, n]) => n > 0)
    .sort((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : 1));
  const rankOf = new Map(ranked.slice(0, 3).map(([id], i) => [id, i + 1]));
  for (const [id, entry] of cards) {
    const n = backing.get(id) || 0;
    const r = rankOf.get(id);
    entry.tallyEl.textContent = n > 0 ? `${r ? `#${r} · ` : ''}${n} ♥` : '';
    entry.tallyEl.title = n > 0
      ? `${n} credit${n === 1 ? '' : 's'} backing${r ? ` · rank #${r} this generation` : ''}`
      : '';
  }
}

// Defer a single card's repaint to the next tick's batched recompute.
function updateTally(sheepId) { markTally(sheepId); }

// ---- tile totals (you vs swarm) ---------------------------------------------
//
// Scan the whole batch store ONCE per tick and distribute by sheepId: each card
// shows your tiles vs the swarm's for that sheep, and the footer shows the
// global totals across every sheep. One pass keeps it cheap regardless of flock
// size (a per-card store scan would be O(cards × batches)).
let globalTiles = { mine: 0, total: 0 };
async function refreshTileTotals() {
  const batches = await store.allBatches();
  const bySheep = new Map(); // sheepId -> { mine, total }
  let gMine = 0, gTotal = 0;
  for (const b of batches) {
    if (banned.has(b.contributor)) continue;
    gTotal++;
    const mine = b.contributor === me.pubHex;
    if (mine) gMine++;
    let e = bySheep.get(b.sheepId);
    if (!e) { e = { mine: 0, total: 0 }; bySheep.set(b.sheepId, e); }
    e.total++;
    if (mine) e.mine++;
  }
  globalTiles = { mine: gMine, total: gTotal };
  for (const [id, entry] of cards) {
    const e = bySheep.get(id) || { mine: 0, total: 0 };
    entry.tilesEl.textContent = e.total ? `you ${e.mine} · swarm ${e.total} tiles` : '';
    entry.tilesEl.title = e.total
      ? `${e.mine} tiles you contributed · ${e.total} tiles from the whole swarm` : '';
  }
}

// ---- credits (the vote-credit economy) --------------------------------------
//
// Earn one credit per batch you render this gen; spend credits to BACK sheep.
// Credits are use-it-or-lose-it (reset every generation). The balance is cached
// and recomputed only when your own render/vote activity changes it.
let creditsView = { tiles: 0, earned: 0, spent: 0, available: 0, perCredit: 128 };
let creditsDirty = true;
const voteSeqByGen = new Map(); // gen -> next seq for my votes (collision-free)

async function refreshCredits() {
  if (!creditsDirty) return;
  creditsDirty = false;
  creditsView = await net.credits(gen());
}

// Spend one credit to back a sheep for survival this generation.
async function backSheep(entry) {
  const g = gen();
  const c = await net.credits(g);
  if (c.available <= 0) {
    flashNoCredits();
    return;
  }
  let seq = voteSeqByGen.get(g);
  if (seq == null) seq = await net.voteCount(g, me.pubHex);
  voteSeqByGen.set(g, seq + 1);
  const record = {
    v: PROTOCOL_VERSION, from: me.pubHex, gen: g,
    sheepId: entry.record.id, n: 1, seq,
  };
  record.sig = await sign(me.pair, voteSignBytes(record));
  await net.publishVote(record);
  markTally(entry.record.id);
  creditsDirty = true;
  await refreshTallies();
  await refreshCredits();
  updateStatus();
}

let noCreditsUntil = 0;
function flashNoCredits() {
  noCreditsUntil = Date.now() + 2500; // updateStatus shows the hint for ~2.5s
  updateStatus();
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
  else showNurseryHint();
}

// Show the explanatory placeholder (fewer than two parents picked): the nursery
// box stays visible with the "pick two parents" hint instead of going blank.
function showNurseryHint() {
  $('#nursery').classList.add('picking');
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

  $('#nursery').classList.remove('picking'); // show the child preview, not the hint
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

  // Breeding gate (UI mirror of the protocol rule in gens.js): you must have
  // contributed >= BREED_MIN_TILES tiles to BOTH parents to release their child.
  const myTiles = async (sid) =>
    (await store.batchesForSheep(sid)).filter((b) => b.contributor === me.pubHex).length;
  const [ta, tb] = await Promise.all([myTiles(aId), myTiles(bId)]);
  const ready = ta >= BREED_MIN_TILES && tb >= BREED_MIN_TILES;

  const release = $('#release');
  release.hidden = false;
  if (cards.has(childId)) {
    release.disabled = true;
    release.textContent = 'already in flock';
  } else if (!ready) {
    release.disabled = true;
    release.title = 'render tiles for both parents first — that’s your stake in the cross';
    release.textContent =
      `contribute to breed · ${Math.min(ta, BREED_MIN_TILES)}/${BREED_MIN_TILES} & ` +
      `${Math.min(tb, BREED_MIN_TILES)}/${BREED_MIN_TILES} tiles`;
  } else {
    release.disabled = false;
    release.title = '';
    release.textContent = 'release';
  }
  release.onclick = async () => {
    release.disabled = true;
    // No render proof needed to release now — a release earns votes by being
    // contributed to like any sheep. The release is a signed sheep record.
    const rg = gen();
    const record = {
      v: PROTOCOL_VERSION,
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
  const pulse = batchActivity ? ` · ⟳ ${batchActivity} tiles/s` : '';
  const credHint = Date.now() < noCreditsUntil ? ' · ⚠ render more to earn a credit' : '';
  const per = creditsView.perCredit || 128;
  const toNext = per - (creditsView.tiles % per);
  const creds = `${creditsView.available} credits (${toNext} tiles to next)`;
  // Global tile total across ALL sheep: your contributed tiles vs the swarm's.
  const tiles = `${globalTiles.mine}/${globalTiles.total} tiles`;
  $('#status').textContent =
    `gen ${gen() - GENESIS_GEN} closes in ${mm}:${ss} · ` +
    `you are ${handle(me.pubHex)} · ${net.peerCount()} peers · ` +
    `${creds} · ${tiles} · ` +
    `${a.audits} audits${a.frauds ? `, ${a.frauds} frauds!` : ''}${pulse}${credHint}` +
    (buildVersion ? ` · build ${buildVersion}` : '');
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
    get fetchedRenders() { return fetchedRenders; },
    get pool() {
      return { queued: pool.queue.length, running: pool.running, chunks: pool.chunksRendered };
    },
  };
}

function installStressHooks() {
  window.__sheepAct = {
    // Quiesce/resume the background contribute loop — lets the harness freeze
    // the batch set during its settle window to measure true convergence.
    pauseContribute(p = true) { contributePaused = !!p; return contributePaused; },
    // Render+publish ONE tile (frame-0-first, then random frame) for a random
    // living card; resolves with the sheepId or null.
    async contributeRandom() {
      const list = [...cards.values()];
      if (!list.length) return null;
      const entry = list[Math.floor(Math.random() * list.length)];
      return contributeBatch(entry);
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
        v: PROTOCOL_VERSION,
        id: bred.childId, genome: bred.childJson, parents: [x, y], gen: g,
        origin: 'release', author: me.pubHex,
      };
      record.sig = await sign(me.pair, sheepSignBytes(record));
      await net.publishSheep(record);
      scheduleRebuild();
      return bred.childId;
    },
    // Ask a peer for the verified accumulated render of (sheep, frame) — the
    // cheap-viewing path (fetch + sample-verify instead of re-rendering all).
    fetchRender: (sheepId, frame = CARD_FRAME) => net.requestRender(sheepId, frame),
    // Contribute one tile to a SPECIFIC sheep (deterministic, for tests).
    async contributeTo(sheepId) {
      const entry = cards.get(sheepId);
      return entry ? contributeBatch(entry) : null;
    },
    // Spend one credit backing a sheep; returns sheepId, or null if no card / no
    // available credits. (The vote-credit economy's spend action.)
    async back(sheepId) {
      const entry = cards.get(sheepId);
      if (!entry) return null;
      if ((await net.credits(gen())).available <= 0) return null;
      await backSheep(entry);
      return sheepId;
    },
    // Read hooks for tests/harness.
    backing: async (sheepId) => (await net.tallies(gen())).get(sheepId) || 0,
    credits: () => net.credits(gen()),
    // Publish a vote record directly, BYPASSING the credit gate — for testing the
    // wire/anti-entropy path of the new record kind. Safe: an uncredited vote has
    // no selection effect (computeBacking caps spend at earned credits), so this
    // can't actually buy influence; it just injects a record to watch it sync.
    async castVote(sheepId) {
      const g = gen();
      const seq = await net.voteCount(g, me.pubHex);
      const record = { v: PROTOCOL_VERSION, from: me.pubHex, gen: g, sheepId, n: 1, seq };
      record.sig = await sign(me.pair, voteSignBytes(record));
      await net.publishVote(record);
      return record.key;
    },
    hasVoteFor: async (sheepId) => (await store.allVotes()).some((v) => v.sheepId === sheepId),
  };

  window.__sheepDump = async () => {
    const [sheep, batches, fraud, renderKeys] = await Promise.all([
      store.allSheep(), store.allBatches(), store.allFraud(), store.allRenderKeys(),
    ]);
    // True P2P-convergence signal: a hash of the WHOLE replicated batch set
    // (every batch key, sorted). Two peers that have exchanged all data agree on
    // this regardless of wall-clock. (Unlike tallyFingerprint below, which only
    // covers the current generation — the freshest, least-propagated slice — and
    // so is sensitive to gen-boundary timing; keep it only as a live-view check.)
    const batchSetHash = await sha256Hex(
      utf8(batches.map((b) => b.key).filter(Boolean).sort().join(',')),
    );
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
      batchSetHash,
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
