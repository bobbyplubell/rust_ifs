// app.js — minimal UI: a flock grid, click-two-to-breed, render-to-vote.
// All state comes from the store + network (net.js); all pixels from the
// worker pool (pool.js). No framework, no build step.

import { WorkerPool } from './pool.js';
import { sha256Hex, utf8 } from './hash.js';
import { loadIdentity, sign, PEER_NS } from './identity.js';
import { openStore } from './store.js';
import {
  Net, BroadcastTransport, CompositeTransport, gen, GEN_MS, GENESIS_GEN,
  PROOF_SPEC, PROOF_TIERS, voteWeight,
  sheepSignBytes, voteSignBytes, voteChallenge,
} from './net.js';
import { computeFlock, breedChallenge } from './gens.js';
import { handle, provenance } from './names.js';
import { Auditor } from './audit.js';
import { RELAYS } from '../config.js';

const $ = (s) => document.querySelector(s);
const pool = new WorkerPool();

// Display quality (free to change): supersampled + more samples than the
// protocol PROOF_SPEC, which vote renders must match exactly.
const VIEW_SPEC = { width: 256, height: 256, ss: 2, nChunks: 32, samplesPerChunk: 120_000 };

// Attention = resolution: cards re-render deeper as votes accumulate. Tally
// is shared state every peer agrees on, so every peer independently spends
// its own CPU on the flock's favorites — the swarm's love is visible as
// fidelity, with no pixels exchanged.
const BOOST_STEPS = [1, 1.6, 2.4, 3.5]; // x samplesPerChunk at tiers 0..3
const boostTier = (tally) => (tally >= 6 ? 3 : tally >= 3 ? 2 : tally >= 1 ? 1 : 0);
const boostedSpec = (tier) => ({
  ...VIEW_SPEC,
  samplesPerChunk: Math.round(VIEW_SPEC.samplesPerChunk * BOOST_STEPS[tier]),
});

const cards = new Map();   // sheepId -> {record, canvas, tallyEl, voteBtn, card}
const tallies = new Map(); // sheepId -> Set of vote keys
const voteWeights = new Map(); // vote key -> tier weight
const voteRecords = new Map(); // vote key -> full record (for sumHash verification)
const sumRequested = new Set(); // vote keys we've asked the swarm for
let sumsReceived = 0;
const selected = [];       // up to two sheepIds picked as parents

let me, store, net, auditor, baked = [];
let shownGen = -1;
let bannedVoters = new Set(); // voters with verified fraud proofs (local view)

async function main() {
  me = await loadIdentity();
  store = await openStore();

  // Tabs always talk via BroadcastChannel; the internet swarm joins in when
  // relays are configured (libp2p bundle loaded lazily, failure non-fatal).
  const transports = [new BroadcastTransport()];
  if (RELAYS.length) {
    try {
      const { createLibp2pTransport } = await import('./vendor/libp2p.js');
      transports.push(await createLibp2pTransport({ relays: RELAYS }));
    } catch (err) {
      console.error('libp2p transport unavailable:', err);
    }
  }

  const checkFrameHash = (genomeJson, challengeHex, idx, tier = 'std') => {
    const spec = PROOF_TIERS[tier] ?? PROOF_TIERS.std;
    return pool.submit({
      type: 'audit-frame', genomeJson, challengeHex, idx,
      width: spec.width, height: spec.height, ss: spec.ss,
      samplesPerFrame: spec.samplesPerFrame,
      nFrames: spec.nFrames, temporal: spec.temporal,
    }).done.then((m) => m.hash);
  };

  net = new Net({
    transport: new CompositeTransport(transports),
    store,
    pubHex: me.pubHex,
    checkSheepId: (genomeJson) =>
      pool.submit({ type: 'sheep-id', genomeJson }).done.then((m) => m.id),
    checkFrameHash,
    lookupSheep: async (id) =>
      baked.find((s) => s.id === id) ?? (await store.allSheep()).find((s) => s.id === id),
    onSheep: () => scheduleRebuild(),
    onVote: (v) => {
      bumpTally(v);
      requestSums(v.sheepId);
      if (v.gen < gen()) scheduleRebuild(); // late vote can rewrite a closed gen
    },
    onFraud: () => scheduleRebuild(), // a discredited voter changes tallies
    onSumData: (voteKey, buf) => mergeSum(voteKey, buf).catch(console.error),
  });

  auditor = new Auditor({
    pool, store, net, me, baked,
    onUpdate: () => updateStatus(),
  });

  // Seed the baked gen-0 flock from the static manifest (local only, not gossiped).
  const manifest = await (await fetch('genomes/manifest.json')).json();
  for (const s of manifest.sheep) {
    const genome = await (await fetch(s.file)).text();
    const id = (await pool.submit({ type: 'sheep-id', genomeJson: genome }).done).id;
    baked.push({ id, genome, parents: null, gen: 0, author: null, sig: null, baked: true, name: s.name });
  }

  for (const v of await store.allVotes()) bumpTally(v, true);
  await rebuildFlock();

  await net.start();
  if (!new URLSearchParams(location.search).get('noaudit')) auditor.start();
  // Test hook (e2e) + curious users.
  window.__sheepStats = {
    get audits() { return auditor.stats.audits; },
    get frauds() { return auditor.stats.frauds; },
    get banned() { return [...bannedVoters]; },
    get sums() { return sumsReceived; },
    get pool() {
      return { queued: pool.queue.length, running: pool.running, chunks: pool.chunksRendered };
    },
  };
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

const breedFn = (aJson, bJson, challengeHex) =>
  pool.submit({ type: 'breed', aJson, bJson, challengeHex }).done;

let rebuildTimer = null;
function scheduleRebuild() {
  clearTimeout(rebuildTimer);
  rebuildTimer = setTimeout(() => rebuildFlock().catch(showError), 400);
}

// Recompute the living flock and diff it against the cards on screen — only
// changed cards re-render (renders are the expensive part).
async function rebuildFlock() {
  bannedVoters = new Set((await store.allFraud()).map((f) => f.vote.voter));
  const { living } = await computeFlock({ store, baked, breedFn, banned: bannedVoters });
  for (const [id, entry] of cards) {
    if (!living.has(id)) {
      if (spinning?.entry === entry) { spinning.stop = true; spinning = null; }
      stopReplay(entry);
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
  updateStatus();
}

// ---- flock -----------------------------------------------------------------

function addCard(record) {
  if (cards.has(record.id)) return;

  const card = document.createElement('div');
  card.className = 'card';
  card.dataset.id = record.id;
  const canvas = document.createElement('canvas');
  canvas.width = PROOF_SPEC.width;
  canvas.height = PROOF_SPEC.height;
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
  const spinBtn = document.createElement('button');
  spinBtn.textContent = 'spin';
  const voteBtn = document.createElement('button');
  voteBtn.textContent = 'vote';
  meta.append(label, tallyEl, spinBtn, voteBtn);
  const bar = document.createElement('div');
  bar.className = 'bar';
  const barFill = document.createElement('div');
  barFill.className = 'bar-fill';
  bar.append(barFill);
  card.append(canvas, bar, meta);
  $('#flock').append(card);

  const entry = { record, canvas, tallyEl, spinBtn, voteBtn, card, barFill };
  cards.set(record.id, entry);
  updateTally(record.id);

  canvas.addEventListener('click', () => toggleSelect(record.id));
  spinBtn.addEventListener('click', () => toggleSpin(entry).catch(showError));
  voteBtn.addEventListener('click', () => vote(entry).catch(showError));

  entry.boostTier = 0;
  drawProgressively(canvas, record.genome, `view|${record.id}`).catch(showError);
  requestSums(record.id);
}

// Render a genome onto a canvas through the pool, painting as chunks land.
// Returns the chunk hashes (= the render proof when challenge is a vote challenge).
// Paint a worker result onto a canvas, resizing the canvas to match the
// image (renders at different specs share card canvases — a size mismatch
// leaves the image in the top-left corner).
function paintTo(canvas, m) {
  if (canvas.width !== m.width || canvas.height !== m.height) {
    canvas.width = m.width;
    canvas.height = m.height;
  }
  canvas.getContext('2d').putImageData(
    new ImageData(new Uint8ClampedArray(m.rgba), m.width, m.height), 0, 0);
}

async function drawProgressively(canvas, genomeJson, challengeSource, challengeHex, spec = VIEW_SPEC) {
  challengeHex ??= await sha256Hex(utf8(challengeSource));
  canvas.classList.add('rendering');
  const job = pool.submit(
    { type: 'render', genomeJson, challengeHex, ...spec, tonemapEvery: 8 },
    { onProgress: (p) => { if (p.rgba) paintTo(canvas, p); } },
  );
  const done = await job.done;
  canvas.classList.remove('rendering');
  if (done.type === 'done') {
    paintTo(canvas, done);
    return done.hashes;
  }
  return null;
}

// ---- spin (flam3-style animation, not a camera move) ------------------------
//
// "spin" rotates each transform's affine basis through 2π over the loop and
// drifts the palette — the original Electric Sheep motion. One sheep spins at
// a time; frames pipeline two-deep through the pool for ~2x the frame rate.

const LOOP_MS = 14_000;
const FRAME = { width: 256, height: 256, samples: 350_000, seed: 7 };
let spinning = null; // {entry, stop}

async function toggleSpin(entry) {
  if (spinning?.entry === entry) { stopSpin(); return; }
  stopSpin();
  const s = { entry, stop: false };
  spinning = s;
  entry.spinBtn.textContent = 'stop';
  const frameJob = () => pool.submit({
    type: 'frame', genomeJson: entry.record.genome,
    phase: (performance.now() % LOOP_MS) / LOOP_MS, ...FRAME,
    shutter: 0.012, temporal: 4, directional: 0.4, // flam3 motion blur (sec 9.1)
  }).done;

  let pending = frameJob();
  while (!s.stop) {
    const next = frameJob();
    const done = await pending;
    pending = next;
    if (s.stop) break;
    if (done.type === 'done') paintTo(entry.canvas, done);
  }
}

function stopSpin() {
  if (!spinning) return;
  const { entry } = spinning;
  spinning.stop = true;
  spinning = null;
  entry.spinBtn.textContent = 'spin';
  // Settle back to the crisp base render.
  drawProgressively(entry.canvas, entry.record.genome, `view|${entry.record.id}`)
    .catch(showError);
}

// ---- loop proofs (protocol v3) -----------------------------------------------

// Render the 64-frame loop proof, fanned out across the worker pool. Frames
// paint into `canvas` as they land (watching the loop assemble IS the work);
// returns ordered hashes + the frames for replay, or null on cancellation.
async function renderLoopProof(canvas, genomeJson, challengeHex, onProgress, spec = PROOF_TIERS.std) {
  const { nFrames } = spec;
  const frames = new Array(nFrames);
  const hashes = new Array(nFrames);
  // Element-wise sum of all frame histograms: the voter's whole proof as ONE
  // accumulation buffer — committed via sumHash and serveable to peers for
  // cross-peer accumulated rendering.
  const cells = spec.width * spec.ss * spec.height * spec.ss * 4;
  const sum = new Float64Array(cells);
  let done = 0;
  const jobs = [];
  for (let i = 0; i < nFrames; i++) {
    jobs.push(pool.submit({
      type: 'proof-frame', genomeJson, challengeHex, idx: i,
      width: spec.width, height: spec.height, ss: spec.ss,
      samplesPerFrame: spec.samplesPerFrame,
      nFrames, temporal: spec.temporal, wantHist: true,
    }).done.then((m) => {
      if (m.type !== 'done') throw new Error('proof cancelled');
      hashes[m.idx] = m.hash;
      const hist = new Float64Array(m.hist);
      for (let k = 0; k < cells; k++) sum[k] += hist[k];
      frames[m.idx] = new ImageData(new Uint8ClampedArray(m.rgba), m.width, m.height);
      if (canvas.width !== m.width) { canvas.width = m.width; canvas.height = m.height; }
      canvas.getContext('2d').putImageData(frames[m.idx], 0, 0);
      onProgress?.(++done, nFrames);
    }));
  }
  try {
    await Promise.all(jobs);
  } catch (err) {
    console.error('loop proof failed:', err?.message || err);
    return null;
  }
  // Same byte layout Rust hashes: cells [r,g,b,count] as f64 little-endian.
  const sumHash = await sha256Hex(new Uint8Array(sum.buffer));
  return { hashes, frames, sum, sumHash };
}

// ---- cross-peer accumulation --------------------------------------------------
//
// Votes commit to the voter's summed proof histogram (sumHash). Peers fetch
// those histograms from whoever holds them, verify against the signed
// commitment, and ADD them: the displayed render's true sample count grows
// with every reachable voter. No pixels cross the network — histograms are
// pre-tonemap accumulation state, content-addressed by the votes themselves.

function requestSums(sheepId) {
  const entry = cards.get(sheepId);
  if (!entry) return;
  const g = gen();
  for (const k of tallies.get(sheepId) ?? []) {
    if (!k.endsWith(`:${g}`)) continue;
    const v = voteRecords.get(k);
    if (!v || v.voter === me.pubHex || sumRequested.has(k)) continue;
    if ((entry.sumCount ?? 0) >= 16) break; // bound memory
    sumRequested.add(k);
    net.requestSum(k);
  }
}

async function mergeSum(voteKey, buf) {
  const v = voteRecords.get(voteKey);
  if (!v || mergedSums.has(voteKey)) return;
  const entry = cards.get(v.sheepId);
  if (!entry) return;
  const spec = PROOF_TIERS[v.tier];
  const cells = spec.width * spec.ss * spec.height * spec.ss * 4;
  if (!(buf instanceof ArrayBuffer) || buf.byteLength !== cells * 8) return;
  // Verify the bytes against the vote's signed commitment.
  if ((await sha256Hex(new Uint8Array(buf))) !== v.sumHash) return;
  mergedSums.add(voteKey);
  sumsReceived++;

  if (!entry.sumAccum) {
    entry.sumAccum = new Float64Array(cells);
    entry.sumCount = 0;
  }
  const inc = new Float64Array(buf);
  for (let k = 0; k < cells; k++) entry.sumAccum[k] += inc[k];
  entry.sumCount++;
  paintAccum(entry, spec).catch(console.error);
}

const mergedSums = new Set();

async function paintAccum(entry, spec) {
  if (entry.votePending || entry.replay || !entry.sumAccum) return;
  // Copy: the worker takes ownership of the transferred buffer.
  const hist = entry.sumAccum.slice().buffer;
  const m = await pool.submit({
    type: 'tonemap-hist', hist, genomeJson: entry.record.genome,
    width: spec.width, height: spec.height, ss: spec.ss,
  }, {}).done;
  if (m.type !== 'done' || entry.votePending || entry.replay) return;
  paintTo(entry.canvas, m);
  entry.tallyEl.title = `${entry.sumCount} voters' render work accumulated`;
}

// Replay a completed proof loop on a card — the reward for voting: your
// proven sheep dances. Costs nothing (cached frames).
function startReplay(entry, frames) {
  stopReplay(entry);
  if (entry.canvas.width !== frames[0].width) {
    entry.canvas.width = frames[0].width;
    entry.canvas.height = frames[0].height;
  }
  const ctx = entry.canvas.getContext('2d');
  let i = 0;
  entry.replay = setInterval(() => {
    ctx.putImageData(frames[i], 0, 0);
    i = (i + 1) % frames.length;
  }, Math.round(14_000 / frames.length));
}

function stopReplay(entry) {
  if (entry.replay) {
    clearInterval(entry.replay);
    entry.replay = null;
  }
}

// ---- voting ----------------------------------------------------------------

async function vote(entry) {
  const tier = 'std';
  if (spinning?.entry === entry) stopSpin();
  const g = gen();
  const myKey = `${me.pubHex}:${entry.record.id}:${g}`;
  if (tallies.get(entry.record.id)?.has(myKey)) return; // already voted this gen

  entry.voteBtn.disabled = true;
  entry.votePending = true;
  stopReplay(entry);
  // The proof render IS watching the sheep: your personal challenge, one full
  // loop of its animation, hashed frame by frame. Tier sets the work and the
  // vote's weight (the challenge is tier-independent).
  const challengeHex = await voteChallenge(entry.record.id, me.pubHex, g);
  const res = await renderLoopProof(
    entry.canvas, entry.record.genome, challengeHex,
    (d, n) => {
      entry.voteBtn.textContent = `${d}/${n}`;
      entry.barFill.style.width = `${(100 * d) / n}%`;
    },
    PROOF_TIERS[tier],
  );
  entry.votePending = false;
  entry.barFill.style.width = '0';
  if (!res) {
    entry.voteBtn.disabled = false;
    entry.voteBtn.textContent = 'vote';
    return;
  }

  const record = {
    sheepId: entry.record.id, gen: g, voter: me.pubHex, tier,
    sumHash: res.sumHash, chunkHashes: res.hashes,
  };
  record.sig = await sign(me.pair, voteSignBytes(record));
  await net.publishVote(record);
  // Keep the summed histogram: peers can fetch it (verified against sumHash)
  // and add our 41M samples to their own view of this sheep.
  await store.putSum(record.key, res.sum.buffer);
  const spec = PROOF_TIERS[tier];
  const cells = res.sum.length;
  if (!entry.sumAccum) { entry.sumAccum = new Float64Array(cells); entry.sumCount = 0; }
  for (let k = 0; k < cells; k++) entry.sumAccum[k] += res.sum[k];
  entry.sumCount++;
  entry.voteBtn.textContent = 'voted ✓';
  startReplay(entry, res.frames);
  requestSums(entry.record.id);
}

function bumpTally(v, quiet) {
  if (!tallies.has(v.sheepId)) tallies.set(v.sheepId, new Set());
  tallies.get(v.sheepId).add(v.key);
  voteWeights.set(v.key, voteWeight(v));
  voteRecords.set(v.key, v);
  if (!quiet) updateTally(v.sheepId);
}

function updateTally(sheepId) {
  const entry = cards.get(sheepId);
  if (!entry) return;
  const set = tallies.get(sheepId);
  // Display this generation's weighted votes, excluding discredited voters.
  const g = gen();
  let now = 0;
  let mine = false;
  if (set) {
    for (const k of set) {
      if (!k.endsWith(`:${g}`)) continue;
      const voter = k.split(':')[0];
      if (bannedVoters.has(voter)) continue;
      now += voteWeights.get(k) ?? 1;
      if (voter === me.pubHex) mine = true;
    }
  }
  entry.tallyEl.textContent = now ? `${now} ♥` : '';
  // Re-render deeper when the tally crosses a boost step (skip while a proof
  // is rendering or the card is replaying the user's own proof loop).
  const tier = boostTier(now);
  if (tier > (entry.boostTier ?? 0) && !entry.votePending && !entry.replay
      && !(entry.sumCount > 0)) {
    entry.boostTier = tier;
    drawProgressively(entry.canvas, entry.record.genome, `view|${sheepId}`,
      undefined, boostedSpec(tier)).catch(showError);
  }
  if (mine) {
    if (!entry.votePending) entry.voteBtn.textContent = 'voted ✓';
    entry.voteBtn.disabled = true;
  } else if (!entry.votePending) {
    entry.voteBtn.textContent = 'vote';
    entry.voteBtn.disabled = false;
  }
}

// ---- breeding ---------------------------------------------------------------

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

  const { childJson, childId } = await pool.submit({
    type: 'breed', aJson: a.genome, bJson: b.genome, challengeHex,
  }).done;
  // Stale? (selection changed while breeding)
  if (selected.length !== 2 || [...selected].sort().join() !== [aId, bId].join()) return;

  $('#nursery-note').textContent =
    `the canonical child of ${aId.slice(0, 8)} × ${bId.slice(0, 8)} (gen ${g})`;
  const canvas = $('#child-canvas');
  await drawProgressively(canvas, childJson, `view|${childId}`);

  const release = $('#release');
  release.hidden = false;
  release.disabled = cards.has(childId);
  release.textContent = cards.has(childId) ? 'already in flock' : 'render & release';
  release.onclick = async () => {
    release.disabled = true;
    // Releasing costs the same loop proof as voting (challenge bound to you +
    // this gen), and the same hashes double as your vote for the child.
    const rg = gen();
    const proofChallenge = await voteChallenge(childId, me.pubHex, rg);
    const res = await renderLoopProof(canvas, childJson, proofChallenge,
      (d, n) => { release.textContent = `rendering proof ${d}/${n}`; });
    if (!res) { release.disabled = false; release.textContent = 'render & release'; return; }
    const chunkHashes = res.hashes;

    const record = {
      id: childId, genome: childJson, parents: [aId, bId], gen: rg,
      author: me.pubHex, chunkHashes,
    };
    record.sig = await sign(me.pair, sheepSignBytes(record));
    await net.publishSheep(record);

    const voteRec = {
      sheepId: childId, gen: rg, voter: me.pubHex, tier: 'std',
      sumHash: res.sumHash, chunkHashes,
    };
    voteRec.sig = await sign(me.pair, voteSignBytes(voteRec));
    await net.publishVote(voteRec);
    await store.putSum(voteRec.key, res.sum.buffer);

    release.textContent = 'released ✓ (and voted)';
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

main().catch(showError);
