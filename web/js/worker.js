// worker.js — module Web Worker hosting the wasm renderer (batch model).
//
// The unit of work is a *batch*: a deterministic slice of one animation
// frame's chaos-game input stream (seed = H(sheep_id|frame|idx)). Batches
// accumulate into per-frame integer histograms that merge — element-wise
// BigUint64Array addition — in the main thread. Everything here is pure,
// deterministic, and content-addressed by the histogram hash.
//
// main -> worker:
//   {type:'render-batch', jobId, genomeJson, sheepId, frame, idx, w, h, ss, spp}
//   {type:'batch-hash',   jobId, genomeJson, sheepId, frame, idx, w, h, ss, spp}
//   {type:'tonemap-int',  jobId, hist:ArrayBuffer(u64), genomeJson, w, h, ss}
//   {type:'total-count',  jobId, hist:ArrayBuffer(u64), w, h, ss}
//   {type:'subtract-check', jobId, acc:ArrayBuffer(u64), batch:ArrayBuffer(u64), w, h, ss}
//   {type:'frame',  jobId, genomeJson, phase, width, height, samples, seed}  // quick preview
//   {type:'breed' | 'mutate' | 'random-genome' | 'sheep-id', ...}
//   {type:'cancel', jobId}
//
// worker -> main:
//   {type:'ready'}
//   {type:'batch-done', jobId, hash, hist:ArrayBuffer, frame, idx}
//   {type:'done', jobId, ...}                  // hash / rgba / count / ok / id
//   {type:'breed-done', jobId, childJson, childId}
//   {type:'error', jobId, message}

import init, {
  render_batch,
  batch_hash,
  tonemap_hist_int,
  total_count,
  subtract_check,
  breed,
  mutate_genome,
  random_genome_json,
  sheep_id,
  render_frame,
} from '../pkg/flame_wasm.js';

const cancelled = new Set();
const wasmReady = init().then(() => self.postMessage({ type: 'ready' }));

self.onmessage = async (event) => {
  const msg = event.data;
  if (msg.type === 'cancel') { cancelled.add(msg.jobId); return; }
  try {
    await wasmReady;
    switch (msg.type) {
      case 'render-batch':    handleRenderBatch(msg); break;
      case 'batch-hash':      handleBatchHash(msg); break;
      case 'tonemap-int':     handleTonemap(msg); break;
      case 'total-count':     handleTotalCount(msg); break;
      case 'subtract-check':  handleSubtractCheck(msg); break;
      case 'frame':           handleFrame(msg); break;
      case 'breed':           handleBreed(msg); break;
      case 'mutate':          handleMutate(msg); break;
      case 'random-genome':   handleRandomGenome(msg); break;
      case 'sheep-id':        handleSheepId(msg); break;
      default: throw new Error(`unknown message type: ${msg.type}`);
    }
  } catch (err) {
    self.postMessage({
      type: 'error', jobId: msg.jobId,
      message: err instanceof Error ? err.message : String(err),
    });
  } finally {
    cancelled.delete(msg.jobId);
  }
};

// Render one batch: returns its content hash AND its integer histogram (for
// local accumulation). The histogram buffer is transferred.
function handleRenderBatch(msg) {
  const b = render_batch(
    msg.genomeJson, msg.sheepId, msg.frame, msg.idx, msg.w, msg.h, msg.ss, msg.spp,
  );
  const hash = b.hash;
  const hist = b.hist; // BigUint64Array (owns its buffer, copied out of wasm)
  b.free?.();
  if (cancelled.has(msg.jobId)) return;
  self.postMessage(
    { type: 'batch-done', jobId: msg.jobId, hash, hist: hist.buffer, frame: msg.frame, idx: msg.idx },
    [hist.buffer],
  );
}

// Audit/verify primitive: a batch's hash only, no pixels.
function handleBatchHash(msg) {
  const hash = batch_hash(
    msg.genomeJson, msg.sheepId, msg.frame, msg.idx, msg.w, msg.h, msg.ss, msg.spp,
  );
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'done', jobId: msg.jobId, hash, frame: msg.frame, idx: msg.idx });
}

// Tone-map an accumulated integer frame histogram for display.
function handleTonemap(msg) {
  const rgba = tonemap_hist_int(
    new BigUint64Array(msg.hist), msg.genomeJson, msg.w, msg.h, msg.ss,
  );
  if (cancelled.has(msg.jobId)) return;
  const buf = rgba.buffer;
  self.postMessage(
    { type: 'done', jobId: msg.jobId, rgba: buf, width: msg.w, height: msg.h },
    [buf],
  );
}

// Verification helper: total plotted-sample count of a histogram (as a string,
// since it can exceed Number.MAX_SAFE_INTEGER for very dense renders).
function handleTotalCount(msg) {
  const c = total_count(new BigUint64Array(msg.hist), msg.w, msg.h, msg.ss);
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'done', jobId: msg.jobId, count: c.toString() });
}

// Verification helper: is `batch` a subset of `acc` (no channel underflows)?
function handleSubtractCheck(msg) {
  const ok = subtract_check(
    new BigUint64Array(msg.acc), new BigUint64Array(msg.batch), msg.w, msg.h, msg.ss,
  );
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'done', jobId: msg.jobId, ok });
}

// Quick single-frame preview (low sample count) for an instant card placeholder
// before community batches accumulate. Display-only.
function handleFrame(msg) {
  const { jobId, genomeJson, phase, width, height, samples, seed } = msg;
  const rgba = render_frame(
    genomeJson, phase, width, height, 1, samples, seed ?? 7,
    msg.shutter ?? 0, msg.temporal ?? 1, msg.directional ?? 0,
  );
  if (cancelled.has(jobId)) return;
  const buf = rgba.buffer;
  self.postMessage({ type: 'done', jobId, rgba: buf, width, height }, [buf]);
}

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

function handleMutate(msg) {
  const childJson = mutate_genome(msg.genomeJson, msg.challengeHex, msg.rate);
  const childId = sheep_id(childJson);
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'breed-done', jobId: msg.jobId, childJson, childId });
}

function handleRandomGenome(msg) {
  const childJson = random_genome_json(msg.seed >>> 0, msg.transforms ?? 3);
  const childId = sheep_id(childJson);
  if (cancelled.has(msg.jobId)) return;
  self.postMessage({ type: 'breed-done', jobId: msg.jobId, childJson, childId });
}
