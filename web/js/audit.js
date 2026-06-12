// audit.js — the background skeptic. Every few seconds (only when the worker
// pool is idle, so it never competes with the user's renders) it picks one
// vote it hasn't checked, re-renders ONE random frame of that vote's loop
// proof (1/64 of the proof's cost), and compares against the signed hash.
//
// A mismatch yields a fraud proof: a self-contained, signed, objectively
// verifiable accusation (any peer re-renders the same frame to confirm —
// see net.js _ingestFraud). All votes from a discredited key are excluded
// from tallies everywhere a verified fraud proof reaches.

import { PROOF_TIERS, voteChallenge, fraudSignBytes } from './net.js';
import { sign } from './identity.js';

export class Auditor {
  constructor({ pool, store, net, me, baked = [], onUpdate, intervalMs = 8000 }) {
    Object.assign(this, { pool, store, net, me, baked, onUpdate, intervalMs });
    this.audited = new Set(); // vote keys checked this session
    this.stats = { audits: 0, frauds: 0 };
  }

  start() {
    this.timer = setInterval(() => this.tick().catch(console.error), this.intervalMs);
  }

  async tick() {
    if (this.pool.queue.length > 0 || this.pool.running > 0) return; // stay out of the way

    const sheep = new Map(
      [...this.baked, ...(await this.store.allSheep())].map((s) => [s.id, s]));
    const banned = new Set((await this.store.allFraud()).map((f) => f.vote.voter));
    const candidates = (await this.store.allVotes()).filter((v) =>
      v.voter !== this.me.pubHex &&    // own work is honest by construction
      !banned.has(v.voter) &&          // already discredited
      !this.audited.has(v.key) &&
      sheep.has(v.sheepId));
    if (!candidates.length) return;

    const v = candidates[Math.floor(Math.random() * candidates.length)];
    const spec = PROOF_TIERS[v.tier];
    if (!spec) return;
    const idx = Math.floor(Math.random() * spec.nFrames);
    console.log('[audit] checking', v.key.slice(0, 16), 'tier', v.tier, 'frame', idx);
    const challengeHex = await voteChallenge(v.sheepId, v.voter, v.gen);
    const m = await this.pool.submit({
      type: 'audit-frame', genomeJson: sheep.get(v.sheepId).genome, challengeHex, idx,
      width: spec.width, height: spec.height, ss: spec.ss,
      samplesPerFrame: spec.samplesPerFrame,
      nFrames: spec.nFrames, temporal: spec.temporal,
    }).done;
    if (m.type !== 'done') return;

    console.log('[audit] result for', v.key.slice(0, 16), m.hash === v.chunkHashes[idx] ? 'ok' : 'MISMATCH');
    this.stats.audits++;
    this.audited.add(v.key);
    if (m.hash !== v.chunkHashes[idx]) {
      this.stats.frauds++;
      const f = { voteKey: v.key, vote: v, frame: idx, expected: m.hash, reporter: this.me.pubHex };
      f.sig = await sign(this.me.pair, fraudSignBytes(f));
      console.log('[audit] publishing fraud proof');
      await this.net.publishFraud(f);
      console.log('[audit] fraud proof published');
    }
    this.onUpdate?.(this.stats);
  }
}
