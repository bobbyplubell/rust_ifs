// contribute.js — the render-contribution loop, v2.
//
// One cycle: POST /api/assign → render each returned WorkUnit in the WASM pool
// (the existing `render-batch` worker message) → POST /api/submit with the
// results. The coordinator assigns DISTINCT idxs, so no two clients ever render
// the same tile (the v1 collision problem is gone by construction); the client
// just renders exactly what it's handed and reports back.
//
// Saturating the pool (like v1): we submit every unit in a bundle to the pool
// at once and await them in parallel, so all workers stay busy; pool.js FIFO-
// queues the overflow. When a bundle finishes we immediately ask for another,
// so the loop runs back-to-back for as long as `running` holds.

// A WorkUnit (API.md) names exactly the args of render-batch, so it maps 1:1
// onto the worker message. We pass the unit's fields straight through.
function renderBatchMsg(unit) {
  return {
    type: 'render-batch',
    genomeJson: unit.genomeJson,
    sheepId: unit.sheepId,
    frame: unit.frame,
    idx: unit.idx,
    w: unit.w,
    h: unit.h,
    ss: unit.ss,
    spp: unit.spp,
    nFrames: unit.nFrames,
  };
}

// base64(deflate(the u64 histogram buffer)). API.md: hist = base64(zstd/deflate
// of the u64 histogram contribution). The coordinator's histio.rs decoder
// magic-sniffs the compression — zlib/deflate (header byte 0x78) or zstd — so we
// deflate first with the browser-native CompressionStream('deflate'), which
// emits the zlib wrapper (magic 0x78) the coordinator accepts. The single-tile
// histogram is overwhelmingly zeros, so this shrinks the ~4.5 MB raw buffer to a
// few percent on the wire. Async because CompressionStream is stream-based.
export async function histToBase64(arrayBuffer) {
  const cs = new CompressionStream('deflate');
  const writer = cs.writable.getWriter();
  writer.write(new Uint8Array(arrayBuffer));
  writer.close();
  const compressed = new Uint8Array(await new Response(cs.readable).arrayBuffer());

  // base64 over a binary string; chunk to avoid String.fromCharCode arg-length
  // limits / apply.call blowups on large buffers.
  let binary = '';
  const CHUNK = 0x8000;
  for (let i = 0; i < compressed.length; i += CHUNK) {
    binary += String.fromCharCode.apply(null, compressed.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

/**
 * Render one WorkUnit → the API.md Result shape, with hist base64-encoded.
 * Returns null if the job was cancelled or errored.
 */
export async function renderUnit(pool, unit) {
  const handle = pool.submit(renderBatchMsg(unit));
  const m = await handle.done;
  if (m.type !== 'batch-done') return null; // cancelled / unexpected
  return {
    sheepId: unit.sheepId,
    frame: unit.frame,
    idx: unit.idx,
    hash: m.hash,
    count: m.count,            // string (may exceed Number.MAX_SAFE_INTEGER)
    hist: await histToBase64(m.hist),
  };
}

/**
 * A self-driving contribute loop. Construct, call start(); it pulls bundles and
 * submits results until stop() is called.
 *
 * @param pool      WorkerPool (the WASM render pool)
 * @param identity  { pubHex, pair } from identity.js
 * @param api       the api.js module (assign/submit)
 * @param opts.sheepId    optional — pin contribution to one sheep
 * @param opts.onResult   ({ accepted, rejected, credits, reputation }) => void
 * @param opts.onError    (err) => void
 * @param opts.idleMs     pause between cycles when the server has no work (default 4000)
 */
export class Contributor {
  constructor(pool, identity, api, opts = {}) {
    this.pool = pool;
    this.identity = identity;
    this.api = api;
    this.opts = opts;
    this.running = false;
    this._inflight = new Set(); // job handles, so stop() can cancel mid-render
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
        bundle = await this.api.assign(this.identity, this.opts.sheepId);
      } catch (e) {
        this.opts.onError?.(e);
        await this._sleep(this.opts.idleMs ?? 4000);
        continue;
      }
      if (!this.running) return;

      const units = bundle.units || [];
      // Peer-audit tasks (ARCHITECTURE §4): each is a WorkUnit naming someone
      // ELSE's tile (the submitter's claimed hash is withheld). We re-render it
      // through the SAME WASM pool and report the observed hash; the coordinator
      // grades the report (match → validate, mismatch → dispute, honeypot →
      // graded for free) instead of re-rendering everything itself.
      const audits = bundle.audits || [];

      if (!units.length && !audits.length) {
        // No work right now — back off briefly, then ask again.
        await this._sleep(this.opts.idleMs ?? 4000);
        continue;
      }

      // Saturate the pool with our OWN render work; build Result objects.
      const jobs = units.map((u) => {
        const handle = this.pool.submit(renderBatchMsg(u));
        this._inflight.add(handle);
        return handle.done
          .then(async (m) => {
            this._inflight.delete(handle);
            if (m.type !== 'batch-done') return null;
            return {
              sheepId: u.sheepId, frame: u.frame, idx: u.idx,
              hash: m.hash, count: m.count, hist: await histToBase64(m.hist),
            };
          })
          .catch(() => { this._inflight.delete(handle); return null; });
      });

      // Audit work: re-render each assigned audit unit and observe its hash.
      // We do NOT upload pixels for audits — only the hash is reported. The
      // contributor's own render work above is unchanged.
      const auditJobs = audits.map((u) => {
        const handle = this.pool.submit(renderBatchMsg(u));
        this._inflight.add(handle);
        return handle.done
          .then((m) => {
            this._inflight.delete(handle);
            if (m.type !== 'batch-done') return null;
            return { sheepId: u.sheepId, frame: u.frame, idx: u.idx, hash: m.hash };
          })
          .catch(() => { this._inflight.delete(handle); return null; });
      });

      const [results, auditReports] = await Promise.all([
        Promise.all(jobs).then((r) => r.filter(Boolean)),
        Promise.all(auditJobs).then((r) => r.filter(Boolean)),
      ]);
      if (!this.running) return;
      if (!results.length && !auditReports.length) continue;

      try {
        const reply = await this.api.submit(this.identity, results, auditReports);
        this.opts.onResult?.(reply);
      } catch (e) {
        this.opts.onError?.(e);
        await this._sleep(this.opts.idleMs ?? 4000);
      }
    }
  }

  _sleep(ms) {
    return new Promise((resolve) => {
      const t = setTimeout(resolve, ms);
      // If stopped mid-sleep, resolve promptly so the loop exits.
      const check = setInterval(() => {
        if (!this.running) { clearTimeout(t); clearInterval(check); resolve(); }
      }, 200);
      setTimeout(() => clearInterval(check), ms + 50);
    });
  }
}
