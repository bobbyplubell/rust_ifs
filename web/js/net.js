// net.js — gossip + anti-entropy over a pluggable transport.
//
// The protocol logic is transport-agnostic (see ARCHITECTURE.md). The dev
// transport is BroadcastChannel: every same-origin browsing context joins the
// bus, so two tabs (?peer=1 / ?peer=2) form a real two-peer network with no
// server. The js-libp2p transport slots in behind the same two methods later.
//
// Wire messages:
//   {kind:'sheep', record}                          new sheep (signed)
//   {kind:'vote',  record}                          new vote  (signed render proof)
//   {kind:'inv',   from, sheep:[ids], votes:[keys]} periodic inventory
//   {kind:'data',  to, sheep:[recs], votes:[recs]}  anti-entropy fill, addressed
//
// Ingest validation (drop, don't propagate, on failure): shape, sizes,
// signature over the canonical sign-bytes. Sheep ids are additionally checked
// against the canonical genome via wasm (caller-provided checkSheepId).

import { utf8, sha256Hex } from './hash.js';
import { verify } from './identity.js';

/** Generation length: 5 minutes, dev and deployed. */
export const GEN_MS = 300_000;
/** The network's first generation (absolute); displayed numbers are relative to it. */
export const GENESIS_GEN = Math.floor(Date.parse('2026-06-12T00:00:00Z') / GEN_MS);
/** Current generation (absolute number; clock-derived so peers agree without consensus). */
export const gen = () => Math.floor(Date.now() / GEN_MS);
/** Survivors per generation — fixes the automatic-breeding output regardless of peer count. */
export const SURVIVORS_K = 4;
/** Deterministic per-author submissions counted per generation (lowest sheep ids win). */
export const AUTHOR_GEN_CAP = 3;

/** Loop proof: the proof's 64 units are FRAMES of the sheep's animation loop
 *  (frame i = phase i/64, 2 temporal sub-steps of motion blur), so proving =
 *  watching one full loop, and the frames replay as a cached animation
 *  afterward. One tier, full quality (~41M samples at 384px): every vote is
 *  worth 1 and rendered beautifully. The tier field stays in the wire format
 *  for future flexibility; only 'std' is currently valid. */
export const PROOF_TIERS = {
  std: { width: 384, height: 384, ss: 1, nFrames: 64, samplesPerFrame: 640_000, temporal: 2, weight: 1 },
};
export const PROOF_SPEC = PROOF_TIERS.std;
export const voteWeight = (v) => PROOF_TIERS[v.tier]?.weight ?? 1;

export const HEX64 = /^[0-9a-f]{64}$/;

// Canonical bytes each signature covers. The genome itself is covered via the
// sheep id (sha-256 of canonical genome JSON). Releases carry a render proof
// (same chunk-hash scheme as votes, challenge H(id|author|gen)) — you cannot
// release a sheep you haven't rendered, which prices submission spam in CPU.
export const sheepSignBytes = (r) =>
  utf8(`sheep|${r.id}|${(r.parents || []).join(',')}|${r.gen}|${r.author}|${r.chunkHashes.join(',')}`);
export const voteSignBytes = (v) =>
  utf8(`vote|${v.sheepId}|${v.gen}|${v.voter}|${v.tier}|${v.sumHash}|${v.chunkHashes.join(',')}`);

export const voteKey = (v) => `${v.voter}:${v.sheepId}:${v.gen}`;

/** Self-certifying vote challenge (v3 = loop proofs). */
export const voteChallenge = (sheepId, voterHex, g) =>
  sha256Hex(utf8(`v3|${sheepId}|${voterHex}|${g}`));

// Fraud proof: a self-contained, objectively checkable accusation — "frame
// `frame` of this signed vote should hash to `expected`, not what was signed".
// Anyone verifies it by re-rendering that one frame (1/64 of a proof).
export const fraudKey = (f) => `${f.voteKey}:${f.frame}`;
export const fraudSignBytes = (f) =>
  utf8(`fraud|${f.voteKey}|${f.frame}|${f.expected}|${f.reporter}`);

/** BroadcastChannel bus name — bumped on wire-format breaks. */
export const CHANNEL = 'sheep-net-v9';

export class BroadcastTransport {
  constructor(channel = CHANNEL) {
    this.ch = new BroadcastChannel(channel);
  }
  send(msg) {
    this.ch.postMessage(msg);
  }
  onMessage(fn) {
    this.ch.onmessage = (e) => fn(e.data);
  }
}

/** Fan messages out over several transports at once (tabs via BroadcastChannel
 *  AND the internet via libp2p). Duplicate deliveries are harmless: ingest is
 *  idempotent (grow-only store). */
export class CompositeTransport {
  constructor(transports) {
    this.transports = transports;
  }
  send(msg) {
    for (const t of this.transports) t.send(msg);
  }
  onMessage(fn) {
    for (const t of this.transports) t.onMessage(fn);
  }
}

export class Net {
  /**
   * @param transport   {send, onMessage}
   * @param store       store.js instance
   * @param pubHex      our peer id (used to address anti-entropy fills)
   * @param checkSheepId  async (genomeJson) => sheep_id hex, via the wasm worker
   * @param checkFrameHash async (genomeJson, challengeHex, frameIdx, tier) => hash —
   *                       re-renders one proof frame; used to verify incoming
   *                       fraud claims before believing them
   * @param onSheep/onVote/onFraud callbacks fired when a NEW record is accepted
   */
  constructor({ transport, store, pubHex, checkSheepId, checkFrameHash,
                lookupSheep, onSheep, onVote, onFraud, onSumData }) {
    Object.assign(this, {
      transport, store, pubHex, checkSheepId, checkFrameHash, lookupSheep,
      onSheep, onVote, onFraud, onSumData,
    });
    this.peers = new Map(); // pubHex -> last inv timestamp
    // Wire telemetry (stress testing / swarm page): message counts + bytes.
    this.counts = { sent: {}, recv: {}, sentBytes: 0, recvBytes: 0 };
  }

  _send(msg) {
    this.counts.sent[msg.kind] = (this.counts.sent[msg.kind] ?? 0) + 1;
    try { this.counts.sentBytes += JSON.stringify(msg).length; } catch { /* buffers */ }
    this.transport.send(msg);
  }

  async start() {
    this.transport.onMessage((msg) => {
      this.counts.recv[msg.kind] = (this.counts.recv[msg.kind] ?? 0) + 1;
      this._recv(msg).catch(console.error);
    });
    await this._sendInv();
    this._invTimer = setInterval(() => this._sendInv().catch(console.error), 5000);
  }

  /** Peers heard from in the last 15s (not counting ourselves). */
  peerCount() {
    const now = Date.now();
    let n = 0;
    for (const t of this.peers.values()) if (now - t < 15_000) n++;
    return n;
  }

  // -- publishing (records must already be signed) ---------------------------

  async publishSheep(record) {
    if (await this.store.addSheep(record)) {
      this._send({ kind: 'sheep', record });
      this.onSheep?.(record);
    }
  }

  async publishVote(record) {
    record.key = voteKey(record);
    if (await this.store.addVote(record)) {
      this._send({ kind: 'vote', record });
      this.onVote?.(record);
    }
  }

  requestSum(voteKey) {
    this._send({ kind: 'want-sum', from: this.pubHex, voteKey });
  }

  async publishFraud(record) {
    record.key = fraudKey(record);
    if (await this.store.addFraud(record)) {
      this._send({ kind: 'fraud', record });
      this.onFraud?.(record);
    }
  }

  // -- receiving --------------------------------------------------------------

  async _recv(msg) {
    switch (msg.kind) {
      case 'sheep': return this._ingestSheep(msg.record);
      case 'vote': return this._ingestVote(msg.record);
      case 'fraud': return this._ingestFraud(msg.record);
      // Cross-peer accumulation: anyone may ask for the summed histogram
      // behind a vote; holders reply addressed. The requester verifies the
      // bytes against the vote's signed sumHash, so holders need no trust.
      case 'want-sum': {
        if (!msg.voteKey || msg.from === this.pubHex) return;
        const buf = await this.store.getSum(msg.voteKey);
        if (buf) {
          this._send({ kind: 'sum-data', to: msg.from, voteKey: msg.voteKey, buf });
        }
        return;
      }
      case 'sum-data': {
        if (msg.to !== this.pubHex || !msg.voteKey || !msg.buf) return;
        this.onSumData?.(msg.voteKey, msg.buf);
        return;
      }
      case 'inv': return this._onInv(msg);
      case 'data': {
        if (msg.to !== this.pubHex) return;
        for (const r of msg.sheep || []) await this._ingestSheep(r);
        for (const r of msg.votes || []) await this._ingestVote(r);
        for (const r of msg.fraud || []) await this._ingestFraud(r);
        return;
      }
    }
  }

  async _ingestSheep(r) {
    if (!r || !HEX64.test(r.id) || typeof r.genome !== 'string') return;
    if (!Number.isInteger(r.gen) || !HEX64.test(r.author ?? '')) return;
    if (r.parents && !(Array.isArray(r.parents) && r.parents.every((p) => HEX64.test(p)))) return;
    if (!Array.isArray(r.chunkHashes) || r.chunkHashes.length !== PROOF_SPEC.nFrames) return;
    if (!r.chunkHashes.every((h) => HEX64.test(h))) return;
    // Storage sanity bound per (author, gen) — the *selection* cap
    // (AUTHOR_GEN_CAP, deterministic lowest-ids) is applied in gens.js.
    const byAuthor = (await this.store.allSheep())
      .filter((s) => s.author === r.author && s.gen === r.gen).length;
    if (byAuthor >= AUTHOR_GEN_CAP * 3) return;
    if (!(await verify(r.author, r.sig, sheepSignBytes(r)))) return;
    if ((await this.checkSheepId(r.genome)) !== r.id) return; // forged id or non-canonical genome
    if (await this.store.addSheep(r)) this.onSheep?.(r);
  }

  async _ingestVote(v) {
    if (!v || !HEX64.test(v.sheepId) || !HEX64.test(v.voter) || !Number.isInteger(v.gen)) return;
    if (!HEX64.test(v.sumHash ?? '')) return; // commitment to the summed histogram
    const tier = PROOF_TIERS[v.tier];
    if (!tier) return;
    if (!Array.isArray(v.chunkHashes) || v.chunkHashes.length !== tier.nFrames) return;
    if (!v.chunkHashes.every((h) => HEX64.test(h))) return;
    if (!(await verify(v.voter, v.sig, voteSignBytes(v)))) return;
    v.key = voteKey(v);
    if (await this.store.addVote(v)) this.onVote?.(v);
  }

  /** A fraud claim is believed only after we verify it OURSELVES: check both
   *  signatures, then re-render the disputed frame and confirm the claimed
   *  hash. Cost: 1/64 of a proof. Requires the sheep's genome — if we don't
   *  hold it yet, drop; anti-entropy re-offers the claim later. */
  async _ingestFraud(f) {
    if (!f || !f.vote || !Number.isInteger(f.frame) || !HEX64.test(f.expected ?? '')) return;
    if (!HEX64.test(f.reporter ?? '')) return;
    const v = f.vote;
    if (!PROOF_TIERS[v.tier]) return;
    if (f.frame < 0 || f.frame >= PROOF_TIERS[v.tier].nFrames) return;
    if (f.voteKey !== voteKey(v)) return;
    if (!(await verify(f.reporter, f.sig, fraudSignBytes(f)))) return;
    // The embedded vote must be genuinely signed by the accused...
    if (!(await verify(v.voter, v.sig, voteSignBytes(v)))) return;
    // ...and the claim must actually dispute it.
    if (v.chunkHashes[f.frame] === f.expected) return;
    const sheep = await this.lookupSheep(v.sheepId);
    if (!sheep) return; // can't verify yet
    const challenge = await voteChallenge(v.sheepId, v.voter, v.gen);
    const actual = await this.checkFrameHash(sheep.genome, challenge, f.frame, v.tier);
    if (actual !== f.expected || actual === v.chunkHashes[f.frame]) return; // false accusation
    f.key = fraudKey(f);
    if (await this.store.addFraud(f)) this.onFraud?.(f);
  }

  // -- anti-entropy ------------------------------------------------------------

  async _sendInv() {
    const sheep = (await this.store.allSheep()).filter((r) => !r.baked).map((r) => r.id);
    const votes = (await this.store.allVotes()).map((v) => v.key);
    const fraud = (await this.store.allFraud()).map((f) => f.key);
    this._send({ kind: 'inv', from: this.pubHex, sheep, votes, fraud });
  }

  async _onInv(msg) {
    if (!msg.from || msg.from === this.pubHex) return;
    this.peers.set(msg.from, Date.now());
    const theirSheep = new Set(msg.sheep || []);
    const theirVotes = new Set(msg.votes || []);
    const theirFraud = new Set(msg.fraud || []);
    const sheep = (await this.store.allSheep()).filter((r) => !r.baked && !theirSheep.has(r.id));
    const votes = (await this.store.allVotes()).filter((v) => !theirVotes.has(v.key));
    const fraud = (await this.store.allFraud()).filter((f) => !theirFraud.has(f.key));
    if (sheep.length || votes.length || fraud.length) {
      this._send({ kind: 'data', to: msg.from, sheep, votes, fraud });
    }
  }
}
