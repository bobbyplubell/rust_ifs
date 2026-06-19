// contribute.js — the v3 render-contribution loop.
//
// One cycle:
//   GET /api/assign?pub=<hex>   → advisory blocks (+ audit tiles)
//   for each block: look up the sheep's GENOME (from the cached flock), render
//     the tile (frame,idx) in the WASM pool, deflate the histogram, and submit:
//        - a signed PieceUpload Envelope (the heavy histogram artifact), and
//        - a signed Coverage Envelope (the "have" / progress claim)
//     via POST /api/msg.
//   for each audit tile: re-render and submit a signed Attestation Envelope.
//
// The render is byte-deterministic, so every contributor that renders the same
// (sheep, frame, idx, pass) produces an identical histogram + hash; the node's
// accumulator content-addresses by that hash, so duplicate work merges (no
// double-count) and the JS↔Rust render math is the integration contract: the
// node re-hashes the decoded histogram and rejects any mismatch with no render.
//
// Render parameters (must match the node, crates/sheep-node):
//   w = h = the sheep's resolution edge (384/512/768/1024)   [SheepView.resolution]
//   ss = 1            (ResolutionTier supersample is 1; accumulator decodes edge×edge)
//   spp = 200_000     (spec::SPP)
//   nFrames = 128     (spec::N_FRAMES)

import { signEnvelope, T } from './api.js';

export const SPP = 200_000;     // spec::SPP
export const N_FRAMES = 128;    // spec::N_FRAMES
export const SS = 1;            // ResolutionTier supersample

// base64(deflate(LE-u64 histogram cells)). The node decodes with
// hist::decode_accum, which magic-sniffs zstd vs zlib/deflate; CompressionStream
// ('deflate') emits a zlib stream (magic 0x78) the node accepts. The histogram
// the worker hands us is a BigUint64Array whose .buffer is already the flat
// little-endian u64 cells [r,g,b,count] per pixel — exactly what decode_accum
// reads back — so we deflate the raw buffer with no re-layout. Async because
// CompressionStream is stream-based.
export async function histToBase64(arrayBuffer) {
  const cs = new CompressionStream('deflate');
  const writer = cs.writable.getWriter();
  writer.write(new Uint8Array(arrayBuffer));
  writer.close();
  const compressed = new Uint8Array(await new Response(cs.readable).arrayBuffer());

  // base64 over a binary string; chunk to avoid String.fromCharCode arg-length
  // limits on large buffers.
  let binary = '';
  const CHUNK = 0x8000;
  for (let i = 0; i < compressed.length; i += CHUNK) {
    binary += String.fromCharCode.apply(null, compressed.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

// Build the render-batch worker message for a block + its sheep's genome.
function renderBatchMsg(genomeJson, sheepId, frame, idx, edge) {
  return {
    type: 'render-batch',
    genomeJson,
    sheepId,
    frame,
    idx,
    w: edge,
    h: edge,
    ss: SS,
    spp: SPP,
    nFrames: N_FRAMES,
  };
}

/**
 * Render ONE block and submit its PieceUpload + Coverage envelopes.
 * Returns the node's reply for the PieceUpload (or null if cancelled/no genome).
 * Exported so the e2e test can drive a single tile end-to-end.
 *
 * @param pool      WorkerPool
 * @param identity  { pubHex, pair }
 * @param api       the api.js module
 * @param block     { sheep_id, frame, idx, pass, block_id }
 * @param genomeJson the sheep's genome JSON string (from the cached flock)
 * @param edge      the sheep's resolution edge (px)
 */
export async function renderAndSubmit(pool, identity, api, block, genomeJson, edge) {
  const handle = pool.submit(renderBatchMsg(genomeJson, block.sheep_id, block.frame, block.idx, edge));
  const m = await handle.done;
  if (m.type !== 'batch-done') return null; // cancelled / unexpected
  const histB64 = await histToBase64(m.hist);
  const pass = block.pass ?? 0;

  // PieceUpload (the heavy artifact) — the node accumulates + re-hashes it.
  const pieceEnv = await signEnvelope(identity, T.PIECE, {
    sheep_id: block.sheep_id,
    frame: block.frame,
    idx: block.idx,
    pass,
    hash: m.hash,
    count: String(m.count),
    hist_b64: histB64,
  });
  // Coverage / have (the progress claim) — earns credit toward selection.
  const coverEnv = await signEnvelope(identity, T.PROGRESS, {
    sheep_id: block.sheep_id,
    frame: block.frame,
    idx: block.idx,
    pass,
    hash: m.hash,
  });

  // Batch both into one POST (the node accepts an array of envelopes).
  const reply = await api.postMsg([pieceEnv, coverEnv]);
  return reply;
}

/**
 * Render an assigned audit tile and submit a signed Attestation (hash only,
 * no pixels). Returns the node reply, or null if cancelled / no genome.
 */
export async function renderAndAttest(pool, identity, api, audit, genomeJson, edge) {
  const handle = pool.submit(renderBatchMsg(genomeJson, audit.sheep_id, audit.frame, audit.idx, edge));
  const m = await handle.done;
  if (m.type !== 'batch-done') return null;
  const env = await signEnvelope(identity, T.ATTEST, {
    sheep_id: audit.sheep_id,
    frame: audit.frame,
    idx: audit.idx,
    pass: audit.pass ?? 0,
    hash: m.hash,
  });
  return api.postMsg(env);
}

/**
 * A self-driving contribute loop. Construct, call start(); it pulls assign
 * bundles and submits rendered pieces until stop() is called.
 *
 * @param pool      WorkerPool (the WASM render pool)
 * @param identity  { pubHex, pair } from identity.js
 * @param api       the api.js module (assign / postMsg / signEnvelope)
 * @param opts.genomeFor  (sheepId) => { genomeJson, edge } | null — resolves a
 *                        sheep's genome + resolution from the cached flock.
 * @param opts.onResult   (reply) => void   per accepted submission
 * @param opts.onError    (err) => void
 * @param opts.idleMs     pause between cycles when there is no work (default 4000)
 */
export class Contributor {
  constructor(pool, identity, api, opts = {}) {
    this.pool = pool;
    this.identity = identity;
    this.api = api;
    this.opts = opts;
    this.running = false;
    this._inflight = new Set();
  }

  start() {
    if (this.running) return;
    this.running = true;
    this._loop().catch((e) => this.opts.onError?.(e));
  }

  stop() {
    this.running = false;
    for (const h of this._inflight) {
      try { h.cancel(); } catch { /* already settled */ }
    }
    this._inflight.clear();
  }

  async _loop() {
    while (this.running) {
      let bundle;
      try {
        bundle = await this.api.assign(this.identity.pubHex);
      } catch (e) {
        this.opts.onError?.(e);
        await this._sleep(this.opts.idleMs ?? 4000);
        continue;
      }
      if (!this.running) return;

      const blocks = bundle.blocks || [];
      const audits = bundle.audits || [];
      if (!blocks.length && !audits.length) {
        await this._sleep(this.opts.idleMs ?? 4000);
        continue;
      }

      // Render + submit each block; resolve the genome from the cached flock.
      const jobs = blocks.map(async (b) => {
        const g = this.opts.genomeFor?.(b.sheep_id);
        if (!g) return null; // unknown sheep (flock not yet polled) — skip
        try {
          return await renderAndSubmit(this.pool, this.identity, this.api, b, g.genomeJson, g.edge);
        } catch (e) {
          this.opts.onError?.(e);
          return null;
        }
      });

      const auditJobs = audits.map(async (a) => {
        const g = this.opts.genomeFor?.(a.sheep_id);
        if (!g) return null;
        try {
          return await renderAndAttest(this.pool, this.identity, this.api, a, g.genomeJson, g.edge);
        } catch (e) {
          this.opts.onError?.(e);
          return null;
        }
      });

      const replies = (await Promise.all([...jobs, ...auditJobs])).filter(Boolean);
      if (!this.running) return;
      for (const r of replies) this.opts.onResult?.(r);
    }
  }

  _sleep(ms) {
    return new Promise((resolve) => {
      const t = setTimeout(resolve, ms);
      const check = setInterval(() => {
        if (!this.running) { clearTimeout(t); clearInterval(check); resolve(); }
      }, 200);
      setTimeout(() => clearInterval(check), ms + 50);
    });
  }
}
