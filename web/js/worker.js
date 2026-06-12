// worker.js — module Web Worker hosting the wasm renderer.
//
// Implements the worker message protocol from PLAN.md exactly, plus two
// small documented extensions:
//
//   1. `render` accepts `challengeSeed` (number) as an alternative to
//      `challengeHex`; the worker derives the challenge internally via the
//      wasm export `challenge_from_seed(seed)`. Used by the flock view for
//      casual (non-proof) renders keyed by the manifest seed.
//
//   2. `{type:'frame', jobId, genomeJson, phase, width, height, samples,
//       seed}` — one animation frame at loop `phase` (0..1) via the wasm
//      `render_frame` export (flam3-style transform rotation + palette
//      drift). Replies with a normal `done` message (empty `hashes`).
//
// main -> worker:
//   {type:'render', jobId, genomeJson, challengeHex | challengeSeed,
//    width, height, ss, samplesPerChunk, nChunks, tonemapEvery}
//   {type:'audit',  jobId, genomeJson, challengeHex, width, height, ss,
//    samplesPerChunk, chunkIdx}
//   {type:'breed',  jobId, aJson, bJson, challengeHex}
//   {type:'cancel', jobId}
//   {type:'spin-frame', ...}                  // extension, see above
//
// worker -> main:
//   {type:'ready'}
//   {type:'progress', jobId, chunkIdx, hash, rgba?, width, height}
//   {type:'done', jobId, hashes, rgba, width, height}   // render / spin-frame
//   {type:'done', jobId, hash}                          // audit
//   {type:'breed-done', jobId, childJson, childId}
//   {type:'error', jobId, message}

import init, {
  ChunkedRender,
  audit_chunk,
  challenge_from_seed,
  breed,
  sheep_id,
  render_frame,
  proof_frame,
  audit_frame,
} from '../pkg/flame_wasm.js';

// Job ids cancelled by the main thread. Checked between chunks; once a job
// is cancelled the worker posts nothing more for it.
const cancelled = new Set();

const wasmReady = init().then(() => self.postMessage({ type: 'ready' }));

// Let queued messages (notably `cancel`) be delivered between chunks.
const yieldToEventLoop = () => new Promise((resolve) => setTimeout(resolve, 0));

self.onmessage = async (event) => {
  const msg = event.data;

  if (msg.type === 'cancel') {
    cancelled.add(msg.jobId);
    return;
  }

  try {
    await wasmReady;
    switch (msg.type) {
      case 'render':     await handleRender(msg); break;
      case 'audit':      handleAudit(msg); break;
      case 'breed':      handleBreed(msg); break;
      case 'sheep-id':   handleSheepId(msg); break;
      case 'frame':      handleFrame(msg); break;
      case 'proof-frame': handleProofFrame(msg); break;
      case 'audit-frame': handleAuditFrame(msg); break;
      default:
        throw new Error(`unknown message type: ${msg.type}`);
    }
  } catch (err) {
    self.postMessage({
      type: 'error',
      jobId: msg.jobId,
      message: err instanceof Error ? err.message : String(err),
    });
  } finally {
    cancelled.delete(msg.jobId);
  }
};

function resolveChallenge(msg) {
  if (typeof msg.challengeHex === 'string') return msg.challengeHex;
  if (typeof msg.challengeSeed === 'number') return challenge_from_seed(msg.challengeSeed);
  throw new Error('render: need challengeHex or challengeSeed');
}

async function handleRender(msg) {
  const { jobId, genomeJson, width, height, ss, samplesPerChunk, nChunks } = msg;
  const tonemapEvery = msg.tonemapEvery || 4;
  const challengeHex = resolveChallenge(msg);

  const renderer = new ChunkedRender(
    genomeJson, width, height, ss, samplesPerChunk, nChunks, challengeHex,
  );
  try {
    const hashes = [];
    for (let i = 0; i < nChunks; i++) {
      if (cancelled.has(jobId)) return; // silent: nothing more for this job

      const hash = renderer.render_chunk(i);
      hashes.push(hash);

      const isLast = i === nChunks - 1;
      const progress = { type: 'progress', jobId, chunkIdx: i, hash, width, height };
      if ((i + 1) % tonemapEvery === 0 || isLast) {
        const rgba = renderer.tonemap(); // fresh copy out of wasm memory
        progress.rgba = rgba.buffer;
        self.postMessage(progress, [progress.rgba]);
      } else {
        self.postMessage(progress);
      }

      if (!isLast) await yieldToEventLoop();
    }

    if (cancelled.has(jobId)) return;
    const rgba = renderer.tonemap().buffer;
    self.postMessage({ type: 'done', jobId, hashes, rgba, width, height }, [rgba]);
  } finally {
    renderer.free();
  }
}

function handleAudit(msg) {
  const hash = audit_chunk(
    msg.genomeJson, msg.width, msg.height, msg.ss,
    msg.samplesPerChunk, msg.challengeHex, msg.chunkIdx,
  );
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'done', jobId: msg.jobId, hash });
}

// {type:'sheep-id', jobId, genomeJson} -> {type:'done', jobId, id}
function handleSheepId(msg) {
  const id = sheep_id(msg.genomeJson);
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'done', jobId: msg.jobId, id });
}

function handleBreed(msg) {
  const childJson = breed(msg.aJson, msg.bJson, msg.challengeHex);
  const childId = sheep_id(childJson);
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'breed-done', jobId: msg.jobId, childJson, childId });
}

// Protocol extension (see header): one animation frame at loop `phase`.
// One frame of a protocol-v3 loop proof: returns the proof hash AND the
// frame's pixels (the proof render IS watching the loop; frames are cached
// for replay). {type:'proof-frame', jobId, genomeJson, challengeHex, idx,
// width, height, ss, samplesPerFrame, nFrames, temporal}
function handleProofFrame(msg) {
  const pf = proof_frame(
    msg.genomeJson, msg.width, msg.height, msg.ss,
    msg.samplesPerFrame, msg.challengeHex, msg.idx, msg.nFrames, msg.temporal,
  );
  if (cancelled.has(msg.jobId)) return;
  const rgba = pf.rgba.buffer;
  const hash = pf.hash;
  pf.free();
  self.postMessage(
    { type: 'done', jobId: msg.jobId, idx: msg.idx, hash, rgba, width: msg.width, height: msg.height },
    [rgba],
  );
}

// Audit one loop-proof frame: hash only.
function handleAuditFrame(msg) {
  const hash = audit_frame(
    msg.genomeJson, msg.width, msg.height, msg.ss,
    msg.samplesPerFrame, msg.challengeHex, msg.idx, msg.nFrames, msg.temporal,
  );
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'done', jobId: msg.jobId, idx: msg.idx, hash });
}

function handleFrame(msg) {
  const { jobId, genomeJson, phase, width, height, samples, seed } = msg;
  // shutter/temporal: flam3-style motion blur (budget split, cost-neutral).
  const rgba = render_frame(
    genomeJson, phase, width, height, 1, samples, seed ?? 7,
    msg.shutter ?? 0, msg.temporal ?? 1,
  );
  if (cancelled.has(jobId)) return;
  const buf = rgba.buffer;
  self.postMessage({ type: 'done', jobId, hashes: [], rgba: buf, width, height }, [buf]);
}
