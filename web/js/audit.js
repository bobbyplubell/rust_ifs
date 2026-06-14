// audit.js — the background skeptic (batch era). Every few seconds (only when
// the worker pool is idle, so it never competes with the user's renders or the
// idle contribution loop) it picks one stored batch contribution it hasn't
// checked, re-renders that ONE batch's hash (the audit primitive — 1 batch of
// work), and compares it against the hash the contributor signed.
//
// A mismatch yields a fraud proof: a self-contained, signed, objectively
// verifiable accusation (any peer re-renders the same batch to confirm — see
// net.js _ingestFraud). On a confirmed proof every contribution from the
// discredited key is excluded from tallies everywhere it reaches.

import { batchKey, fraudSignBytes, PROTOCOL_VERSION, specForGen } from './net.js';

export class Auditor {
  /**
   * @param pool          WorkerPool
   * @param store         store.js instance
   * @param baked         baked gen-0 sheep records (for genome lookup)
   * @param publishFraud  async (record) => void  (net.publishFraud)
   * @param identity      { pubHex, pair }
   * @param sign          async (pair, bytes) => sigHex
   * @param isBanned      (pubHex) => bool — skip already-discredited keys
   * @param onUpdate      () => void — fired after each audit (stats changed)
   * @param intervalMs    audit cadence (default 8000)
   * @param lookupSheep   optional async (id) => record (overrides baked+store)
   */
  constructor({ pool, store, baked = [], publishFraud, identity, sign,
                isBanned, onUpdate, intervalMs = 8000, lookupSheep }) {
    Object.assign(this, {
      pool, store, baked, publishFraud, identity, sign,
      isBanned, onUpdate, intervalMs, lookupSheep,
    });
    this.audited = new Set(); // batch keys checked this session
    this.stats = { audits: 0, frauds: 0 };
  }

  start() {
    this.timer = setInterval(() => this.tick().catch(console.error), this.intervalMs);
  }

  stop() {
    clearInterval(this.timer);
  }

  async _lookup(id) {
    if (this.lookupSheep) return this.lookupSheep(id);
    return this.baked.find((s) => s.id === id)
      ?? (await this.store.allSheep()).find((s) => s.id === id);
  }

  async tick() {
    // Audit one batch per tick; yield to the user but don't let the idle
    // contribution loop (which keeps the pool busy) starve fraud detection —
    // only back off when the queue is genuinely backed up.
    if (this.pool.queue.length > 3) return;

    const batches = await this.store.allBatches();
    const candidates = batches.filter((b) => {
      const key = b.key ?? batchKey(b);
      return b.contributor !== this.identity.pubHex   // own work is honest by construction
        && !this.isBanned?.(b.contributor)            // already discredited
        && !this.audited.has(key);
    });
    if (!candidates.length) return;

    const b = candidates[Math.floor(Math.random() * candidates.length)];
    const key = b.key ?? batchKey(b);
    const sheep = await this._lookup(b.sheepId);
    if (!sheep) return; // can't audit without the genome; anti-entropy re-offers it

    const spec = specForGen(sheep.gen); // the sheep's render spec (by birth gen)
    const reply = await this.pool.submit({
      type: 'batch-hash', genomeJson: sheep.genome, sheepId: b.sheepId,
      frame: b.frame, idx: b.idx,
      w: spec.width, h: spec.height, ss: spec.ss, spp: spec.spp, nFrames: spec.nFrames,
    }).done;
    if (reply.type !== 'done') return;

    this.stats.audits++;
    this.audited.add(key);
    if (reply.hash !== b.hash) {
      this.stats.frauds++;
      const fraud = {
        v: PROTOCOL_VERSION,
        batchKey: key, expected: reply.hash, reporter: this.identity.pubHex,
      };
      fraud.sig = await this.sign(this.identity.pair, fraudSignBytes(fraud));
      await this.publishFraud(fraud);
    }
    this.onUpdate?.(this.stats);
  }
}
