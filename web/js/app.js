// app.js — minimal UI: a flock grid, click-two-to-breed, render-to-vote.
// All state comes from the store + network (net.js); all pixels from the
// worker pool (pool.js). No framework, no build step.

import { WorkerPool } from './pool.js';
import { sha256Hex, utf8 } from './hash.js';
import { loadIdentity, sign, PEER_NS } from './identity.js';
import { openStore } from './store.js';
import {
  Net, BroadcastTransport, gen, PROOF_SPEC,
  sheepSignBytes, voteSignBytes, voteChallenge,
} from './net.js';

const $ = (s) => document.querySelector(s);
const pool = new WorkerPool();

// Display quality (free to change): supersampled + more samples than the
// protocol PROOF_SPEC, which vote renders must match exactly.
const VIEW_SPEC = { width: 256, height: 256, ss: 2, nChunks: 32, samplesPerChunk: 120_000 };

const cards = new Map();   // sheepId -> {record, canvas, tallyEl, voteBtn, card}
const tallies = new Map(); // sheepId -> Set of vote keys
const selected = [];       // up to two sheepIds picked as parents

let me, store, net;

async function main() {
  me = await loadIdentity();
  store = await openStore();
  net = new Net({
    transport: new BroadcastTransport(),
    store,
    pubHex: me.pubHex,
    checkSheepId: (genomeJson) =>
      pool.submit({ type: 'sheep-id', genomeJson }).done.then((m) => m.id),
    onSheep: (r) => addCard(r),
    onVote: (v) => bumpTally(v),
  });

  // Seed the baked gen-0 flock from the static manifest (local only, not gossiped).
  const manifest = await (await fetch('genomes/manifest.json')).json();
  for (const s of manifest.sheep) {
    const genome = await (await fetch(s.file)).text();
    const id = (await pool.submit({ type: 'sheep-id', genomeJson: genome }).done).id;
    await store.addSheep({ id, genome, parents: null, gen: 0, author: null, sig: null, baked: true, name: s.name });
  }

  for (const v of await store.allVotes()) bumpTally(v, true);
  for (const r of await store.allSheep()) addCard(r);

  await net.start();
  setInterval(updateStatus, 2000);
  updateStatus();
}

// ---- flock -----------------------------------------------------------------

function addCard(record) {
  if (cards.has(record.id)) return;

  const card = document.createElement('div');
  card.className = 'card';
  const canvas = document.createElement('canvas');
  canvas.width = PROOF_SPEC.width;
  canvas.height = PROOF_SPEC.height;
  const meta = document.createElement('div');
  meta.className = 'meta';
  const label = document.createElement('span');
  label.textContent = record.name || (record.parents ? 'child ' : 'sheep ') + record.id.slice(0, 8);
  label.title = record.id;
  const tallyEl = document.createElement('span');
  tallyEl.className = 'tally';
  const spinBtn = document.createElement('button');
  spinBtn.textContent = 'spin';
  const voteBtn = document.createElement('button');
  voteBtn.textContent = 'vote';
  meta.append(label, tallyEl, spinBtn, voteBtn);
  card.append(canvas, meta);
  $('#flock').append(card);

  const entry = { record, canvas, tallyEl, spinBtn, voteBtn, card };
  cards.set(record.id, entry);
  updateTally(record.id);

  canvas.addEventListener('click', () => toggleSelect(record.id));
  spinBtn.addEventListener('click', () => toggleSpin(entry).catch(showError));
  voteBtn.addEventListener('click', () => vote(entry).catch(showError));

  drawProgressively(canvas, record.genome, `view|${record.id}`).catch(showError);
}

// Render a genome onto a canvas through the pool, painting as chunks land.
// Returns the chunk hashes (= the render proof when challenge is a vote challenge).
async function drawProgressively(canvas, genomeJson, challengeSource, challengeHex, spec = VIEW_SPEC) {
  challengeHex ??= await sha256Hex(utf8(challengeSource));
  const ctx = canvas.getContext('2d');
  canvas.classList.add('rendering');
  const job = pool.submit(
    { type: 'render', genomeJson, challengeHex, ...spec, tonemapEvery: 8 },
    {
      onProgress: (p) => {
        if (p.rgba) {
          ctx.putImageData(
            new ImageData(new Uint8ClampedArray(p.rgba), p.width, p.height), 0, 0);
        }
      },
    },
  );
  const done = await job.done;
  canvas.classList.remove('rendering');
  if (done.type === 'done') {
    ctx.putImageData(new ImageData(new Uint8ClampedArray(done.rgba), done.width, done.height), 0, 0);
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
  const ctx = entry.canvas.getContext('2d');

  const frameJob = () => pool.submit({
    type: 'frame', genomeJson: entry.record.genome,
    phase: (performance.now() % LOOP_MS) / LOOP_MS, ...FRAME,
  }).done;

  let pending = frameJob();
  while (!s.stop) {
    const next = frameJob();
    const done = await pending;
    pending = next;
    if (s.stop) break;
    if (done.type === 'done') {
      ctx.putImageData(
        new ImageData(new Uint8ClampedArray(done.rgba), done.width, done.height), 0, 0);
    }
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

// ---- voting ----------------------------------------------------------------

async function vote(entry) {
  if (spinning?.entry === entry) stopSpin();
  const g = gen();
  const myKey = `${me.pubHex}:${entry.record.id}:${g}`;
  if (tallies.get(entry.record.id)?.has(myKey)) return; // already voted this gen

  entry.voteBtn.disabled = true;
  entry.voteBtn.textContent = 'rendering…';
  // The proof render IS watching the sheep: personal challenge, full spec.
  const challengeHex = await voteChallenge(entry.record.id, me.pubHex, g);
  const chunkHashes = await drawProgressively(
    entry.canvas, entry.record.genome, null, challengeHex, PROOF_SPEC);
  if (!chunkHashes) { entry.voteBtn.disabled = false; entry.voteBtn.textContent = 'vote'; return; }

  const record = { sheepId: entry.record.id, gen: g, voter: me.pubHex, chunkHashes };
  record.sig = await sign(me.pair, voteSignBytes(record));
  await net.publishVote(record);
  entry.voteBtn.textContent = 'voted ✓';
  // The proof render is protocol-quality; settle back to display quality.
  drawProgressively(entry.canvas, entry.record.genome, `view|${entry.record.id}`)
    .catch(showError);
}

function bumpTally(v, quiet) {
  if (!tallies.has(v.sheepId)) tallies.set(v.sheepId, new Set());
  tallies.get(v.sheepId).add(v.key);
  if (!quiet) updateTally(v.sheepId);
}

function updateTally(sheepId) {
  const entry = cards.get(sheepId);
  if (!entry) return;
  const set = tallies.get(sheepId);
  entry.tallyEl.textContent = set?.size ? `${set.size} ♥` : '';
  const myKey = `${me.pubHex}:${sheepId}:${gen()}`;
  if (set?.has(myKey)) {
    entry.voteBtn.textContent = 'voted ✓';
    entry.voteBtn.disabled = true;
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
  const challengeHex = await sha256Hex(utf8(`breed|${g}|${aId}|${bId}`));

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
  release.textContent = cards.has(childId) ? 'already in flock' : 'release into flock';
  release.onclick = async () => {
    const record = { id: childId, genome: childJson, parents: [aId, bId], gen: g, author: me.pubHex };
    record.sig = await sign(me.pair, sheepSignBytes(record));
    await net.publishSheep(record);
    release.disabled = true;
    release.textContent = 'released ✓';
  };
}

// ---- chrome -----------------------------------------------------------------

function updateStatus() {
  $('#status').textContent =
    `peer ${PEER_NS} · ${me.pubHex.slice(0, 8)} · ${net.peerCount()} peers · ` +
    `${pool.chunksRendered} chunks rendered`;
}

function showError(err) {
  console.error(err);
  $('#status').textContent = 'error: ' + (err?.message || err);
}

main().catch(showError);
