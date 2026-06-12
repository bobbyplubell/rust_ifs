// app.js — flock view, sheep view (modal), breeding lab.
// No framework, no build step; everything renders through the worker pool.

import { WorkerPool } from './pool.js';
import { sha256Hex, utf8 } from './hash.js';

// Render budgets ---------------------------------------------------------

const FLOCK  = { width: 256, height: 256, ss: 1, nChunks: 64, samplesPerChunk: 20_000, tonemapEvery: 4 };
const DETAIL = { width: 512, height: 512, ss: 1, nChunks: 64, samplesPerChunk: 60_000, tonemapEvery: 4 };
const CHILD  = { width: 256, height: 256, ss: 1, nChunks: 64, samplesPerChunk: 20_000, tonemapEvery: 4 };
const WHATIF = { width: 128, height: 128, ss: 1, nChunks: 32, samplesPerChunk: 10_000, tonemapEvery: 4 };
const SPIN   = { width: 256, height: 256, samples: 400_000 };

// State -------------------------------------------------------------------

const pool = new WorkerPool();
const flock = []; // {name, file, seed, genomeJson, card, canvas, handle}
let parentA = null;
let parentB = null;
let labJobs = []; // cancellable handles for the current lab pairing
let labRun = 0;   // monotonically increasing; stale lab async work checks it

// DOM helpers ---------------------------------------------------------------

const $ = (sel, root = document) => root.querySelector(sel);

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function drawRgba(canvas, rgbaBuffer, w, h) {
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
  }
  const img = new ImageData(new Uint8ClampedArray(rgbaBuffer), w, h);
  canvas.getContext('2d').putImageData(img, 0, 0);
}

// Draw a frame scaled to whatever size the canvas already is.
const scratch = document.createElement('canvas');
function drawRgbaScaled(canvas, rgbaBuffer, w, h) {
  scratch.width = w;
  scratch.height = h;
  scratch.getContext('2d').putImageData(new ImageData(new Uint8ClampedArray(rgbaBuffer), w, h), 0, 0);
  const ctx = canvas.getContext('2d');
  ctx.imageSmoothingEnabled = true;
  ctx.drawImage(scratch, 0, 0, canvas.width, canvas.height);
}

function downloadText(filename, text) {
  const blob = new Blob([text], { type: 'application/json' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  a.click();
  URL.revokeObjectURL(url);
}

function progressCard(opts) {
  // Shared card scaffold: canvas + progress bar + caption row.
  const card = el('div', `card ${opts.className || ''}`);
  const frame = el('div', 'frame');
  const canvas = el('canvas');
  canvas.width = opts.width;
  canvas.height = opts.height;
  const bar = el('div', 'bar');
  const fill = el('div', 'bar-fill');
  bar.append(fill);
  frame.append(canvas, bar);
  card.append(frame);
  if (opts.caption) card.append(opts.caption);
  return { card, canvas, fill, bar };
}

function renderProgressively(canvas, fill, genomeJson, budget, challenge) {
  // challenge: {challengeHex} or {challengeSeed}
  const handle = pool.submit({ type: 'render', genomeJson, ...budget, ...challenge });
  handle.onProgress = (msg) => {
    fill.style.width = `${(((msg.chunkIdx + 1) / budget.nChunks) * 100).toFixed(1)}%`;
    if (msg.rgba) drawRgba(canvas, msg.rgba, msg.width, msg.height);
  };
  handle.done
    .then((msg) => {
      if (msg.type === 'done') {
        drawRgba(canvas, msg.rgba, msg.width, msg.height);
        fill.parentElement.classList.add('bar-done');
      }
    })
    .catch((err) => {
      fill.parentElement.classList.add('bar-error');
      console.error('render failed:', err);
    });
  return handle;
}

// Tabs ---------------------------------------------------------------------

function initTabs() {
  const tabs = document.querySelectorAll('nav .tab');
  tabs.forEach((tab) => {
    tab.addEventListener('click', () => {
      tabs.forEach((t) => t.classList.toggle('active', t === tab));
      document.querySelectorAll('main > section').forEach((s) => {
        s.hidden = s.id !== tab.dataset.view;
      });
    });
  });
}

// Flock view -----------------------------------------------------------------

async function buildFlock() {
  const manifest = await (await fetch('genomes/manifest.json')).json();
  const grid = $('#flock-grid');

  for (const sheep of manifest.sheep) {
    const genomeJson = await (await fetch(sheep.file)).text();

    const caption = el('div', 'meta');
    const name = el('span', 'name', sheep.name);
    // TODO(integration): show first 8 hex chars of wasm sheep_id(genomeJson)
    // here; until the new exports land we show the manifest seed instead.
    const idTag = el('span', 'sheep-id', `#${sheep.seed}`);
    caption.append(name, idTag);

    const { card, canvas, fill } = progressCard({ ...FLOCK, caption, className: 'flock-card' });
    grid.append(card);

    const entry = { ...sheep, genomeJson, card, canvas };
    flock.push(entry);

    // Flock renders are casual: challenge derived in the worker from the
    // manifest seed via challenge_from_seed (challengeSeed protocol extension).
    renderProgressively(canvas, fill, genomeJson, FLOCK, { challengeSeed: sheep.seed });

    card.addEventListener('click', () => openSheepModal(entry));
  }

  buildLabPicker();
}

// Sheep view (modal) ---------------------------------------------------------

let modalCleanup = null;

function openSheepModal(sheep) {
  const modal = $('#sheep-modal');
  const canvas = $('#modal-canvas');
  const fill = $('#modal-bar-fill');
  $('#modal-bar').classList.remove('bar-done', 'bar-error');
  $('#modal-title').textContent = sheep.name;
  $('#modal-sub').textContent = `seed ${sheep.seed} · ${DETAIL.width}×${DETAIL.height}, ${DETAIL.nChunks} chunks`;

  canvas.width = DETAIL.width;
  canvas.height = DETAIL.height;
  canvas.getContext('2d').clearRect(0, 0, canvas.width, canvas.height);
  fill.style.width = '0%';

  let spinning = false;
  let lastRgba = null; // latest progressive frame, restored when spin stops

  const renderHandle = pool.submit({
    type: 'render',
    genomeJson: sheep.genomeJson,
    ...DETAIL,
    challengeSeed: sheep.seed,
  });
  renderHandle.onProgress = (msg) => {
    fill.style.width = `${(((msg.chunkIdx + 1) / DETAIL.nChunks) * 100).toFixed(1)}%`;
    if (msg.rgba) {
      lastRgba = { rgba: msg.rgba, width: msg.width, height: msg.height };
      if (!spinning) drawRgba(canvas, msg.rgba, msg.width, msg.height);
    }
  };
  renderHandle.done
    .then((msg) => {
      if (msg.type !== 'done') return;
      lastRgba = { rgba: msg.rgba, width: msg.width, height: msg.height };
      if (!spinning) drawRgba(canvas, msg.rgba, msg.width, msg.height);
      $('#modal-bar').classList.add('bar-done');
    })
    .catch((err) => console.error('sheep render failed:', err));

  // Spin: low-cost render_rgba frames via the worker (spin-frame extension),
  // drawn scaled up; the progressive ChunkedRender keeps accumulating
  // underneath and is restored when spin stops.
  let angle = 0;
  let spinJob = null;
  const spinBtn = $('#modal-spin');
  spinBtn.classList.remove('on');
  spinBtn.onclick = () => {
    spinning = !spinning;
    spinBtn.classList.toggle('on', spinning);
    if (spinning) spinLoop();
  };

  async function spinLoop() {
    while (spinning) {
      spinJob = pool.submit({
        type: 'spin-frame',
        genomeJson: sheep.genomeJson,
        seed: sheep.seed,
        ...SPIN,
        rotate: angle,
      });
      let msg;
      try {
        msg = await spinJob.done;
      } catch (err) {
        console.error('spin frame failed:', err);
        break;
      }
      if (!spinning || msg.type !== 'done') break;
      drawRgbaScaled(canvas, msg.rgba, msg.width, msg.height);
      angle += 0.07;
    }
    spinJob = null;
    if (lastRgba) drawRgba(canvas, lastRgba.rgba, lastRgba.width, lastRgba.height);
  }

  $('#modal-download').onclick = () =>
    downloadText(`${sheep.name.replace(/\W+/g, '_').toLowerCase()}.json`, sheep.genomeJson);

  modalCleanup = () => {
    spinning = false;
    renderHandle.cancel();
    if (spinJob) spinJob.cancel();
  };
  modal.hidden = false;
}

function closeSheepModal() {
  if (modalCleanup) modalCleanup();
  modalCleanup = null;
  $('#sheep-modal').hidden = true;
}

// Breeding lab ---------------------------------------------------------------

function buildLabPicker() {
  const picker = $('#lab-picker');
  picker.textContent = '';
  for (const sheep of flock) {
    const chip = el('button', 'pick-chip');
    const mini = el('canvas', 'pick-mini');
    mini.width = 64;
    mini.height = 64;
    chip.append(mini, el('span', null, sheep.name));
    chip.addEventListener('click', () => {
      // Refresh the thumbnail from the flock canvas (may still be rendering).
      mini.getContext('2d').drawImage(sheep.canvas, 0, 0, 64, 64);
      pickParent(sheep);
    });
    // Initial thumbnail once the flock card has something to show.
    setTimeout(() => mini.getContext('2d').drawImage(sheep.canvas, 0, 0, 64, 64), 3000);
    picker.append(chip);
  }

  $('#slot-a').addEventListener('click', () => clearParent('a'));
  $('#slot-b').addEventListener('click', () => clearParent('b'));
}

function pickParent(sheep) {
  if (!parentA) parentA = sheep;
  else if (!parentB && sheep !== parentA) parentB = sheep;
  else { parentA = sheep; parentB = null; } // start a new pairing
  refreshSlots();
}

function clearParent(which) {
  if (which === 'a') parentA = null;
  else parentB = null;
  refreshSlots();
}

function refreshSlots() {
  fillSlot($('#slot-a'), parentA);
  fillSlot($('#slot-b'), parentB);
  resetLabResults();
  if (parentA && parentB) breedPair(parentA, parentB);
}

function fillSlot(slot, sheep) {
  const cv = $('canvas', slot);
  const label = $('.slot-label', slot);
  const ctx = cv.getContext('2d');
  ctx.clearRect(0, 0, cv.width, cv.height);
  if (sheep) {
    ctx.drawImage(sheep.canvas, 0, 0, cv.width, cv.height);
    label.textContent = sheep.name;
    slot.classList.add('filled');
  } else {
    label.textContent = 'pick a sheep';
    slot.classList.remove('filled');
  }
}

function resetLabResults() {
  labRun++;
  for (const h of labJobs) h.cancel();
  labJobs = [];
  $('#lab-child').textContent = '';
  $('#lab-whatif').textContent = '';
  $('#lab-hint').hidden = !!(parentA && parentB);
}

// Breed one child via the worker and render it progressively into a card.
async function breedChild(run, { aJson, bJson, challengeStr, mount, budget, label }) {
  const challengeHex = await sha256Hex(utf8(challengeStr));
  if (run !== labRun) return;

  const breedHandle = pool.submit({ type: 'breed', aJson, bJson, challengeHex });
  labJobs.push(breedHandle);

  const caption = el('div', 'meta');
  const nameSpan = el('span', 'name', label);
  const dl = el('button', 'mini-btn', 'genome ↓');
  dl.disabled = true;
  caption.append(nameSpan, dl);

  const { card, canvas, fill } = progressCard({ ...budget, caption, className: 'child-card' });
  mount.append(card);

  let bred;
  try {
    bred = await breedHandle.done;
  } catch (err) {
    if (run === labRun) {
      card.classList.add('failed');
      nameSpan.textContent = `${label} — breed failed`;
    }
    console.error('breed failed:', err);
    return;
  }
  if (run !== labRun || bred.type !== 'breed-done') return;

  nameSpan.title = bred.childId;
  nameSpan.textContent = `${label} · ${bred.childId.slice(0, 8)}`;
  dl.disabled = false;
  dl.onclick = (e) => {
    e.stopPropagation();
    downloadText(`child_${bred.childId.slice(0, 8)}.json`, bred.childJson);
  };

  const renderHandle = renderProgressively(canvas, fill, bred.childJson, budget, { challengeHex });
  labJobs.push(renderHandle);
}

function breedPair(a, b) {
  const run = labRun;
  // TODO(integration): use wasm sheep_id for idA/idB; names stand in until
  // the new exports are merged.
  const idA = a.name;
  const idB = b.name;

  breedChild(run, {
    aJson: a.genomeJson,
    bJson: b.genomeJson,
    challengeStr: `breed:0:${idA}:${idB}`,
    mount: $('#lab-child'),
    budget: CHILD,
    label: 'the child these two will have',
  });

  for (let i = 0; i < 9; i++) {
    breedChild(run, {
      aJson: a.genomeJson,
      bJson: b.genomeJson,
      challengeStr: `whatif:0:${idA}:${idB}:${i}`,
      mount: $('#lab-whatif'),
      budget: WHATIF,
      label: `what-if ${i}`,
    });
  }
}

// Status footer ---------------------------------------------------------------

function initStatus() {
  const node = $('#status-bar');
  pool.onStats = ({ size, queued, running, chunks }) => {
    node.textContent =
      `${size} workers · ${running} running · ${queued} queued · ` +
      `${chunks.toLocaleString()} chunks rendered this session`;
  };
  pool.onStats({ size: pool.size, queued: 0, running: 0, chunks: 0 });
}

// Boot -------------------------------------------------------------------------

initTabs();
initStatus();
$('#modal-close').addEventListener('click', closeSheepModal);
$('#sheep-modal').addEventListener('click', (e) => {
  if (e.target.id === 'sheep-modal') closeSheepModal();
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') closeSheepModal();
});

buildFlock().catch((err) => {
  $('#status-bar').textContent = `failed to load flock: ${err.message}`;
  console.error(err);
});
