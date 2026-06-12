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
//   2. `{type:'spin-frame', jobId, genomeJson, seed, width, height,
//       samples, rotate}` — a one-shot frame via the OLD `render_rgba`
//      export (which still exists), used for the sheep-view spin animation
//      where ChunkedRender would be far too slow per frame. Replies with a
//      normal `done` message carrying the RGBA buffer (empty `hashes`).
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
  render_rgba,
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
      case 'spin-frame': handleSpinFrame(msg); break;
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

function handleBreed(msg) {
  const childJson = breed(msg.aJson, msg.bJson, msg.challengeHex);
  const childId = sheep_id(childJson);
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'breed-done', jobId: msg.jobId, childJson, childId });
}

// Protocol extension (see header): single fast frame for spin animation.
function handleSpinFrame(msg) {
  const { jobId, genomeJson, seed, width, height, samples, rotate } = msg;
  const rgba = render_rgba(genomeJson, width, height, 1, samples, seed, rotate);
  if (cancelled.has(jobId)) return;
  const buf = rgba.buffer;
  self.postMessage({ type: 'done', jobId, hashes: [], rgba: buf, width, height }, [buf]);
}
