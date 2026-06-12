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

/** Current generation = UTC day number. */
export const gen = () => Math.floor(Date.now() / 86_400_000);

/** Protocol render spec for vote proofs (PLAN.md tuning constants). */
export const PROOF_SPEC = { width: 256, height: 256, ss: 1, nChunks: 64, samplesPerChunk: 20_000 };

export const HEX64 = /^[0-9a-f]{64}$/;

// Canonical bytes each signature covers. The genome itself is covered via the
// sheep id (sha-256 of canonical genome JSON).
export const sheepSignBytes = (r) =>
  utf8(`sheep|${r.id}|${(r.parents || []).join(',')}|${r.gen}|${r.author}`);
export const voteSignBytes = (v) =>
  utf8(`vote|${v.sheepId}|${v.gen}|${v.voter}|${v.chunkHashes.join(',')}`);

export const voteKey = (v) => `${v.voter}:${v.sheepId}:${v.gen}`;

/** Self-certifying vote challenge: H(sheep_id ‖ voter ‖ gen). */
export const voteChallenge = (sheepId, voterHex, g) =>
  sha256Hex(utf8(`${sheepId}|${voterHex}|${g}`));

export class BroadcastTransport {
  constructor(channel = 'sheep-net-v1') {
    this.ch = new BroadcastChannel(channel);
  }
  send(msg) {
    this.ch.postMessage(msg);
  }
  onMessage(fn) {
    this.ch.onmessage = (e) => fn(e.data);
  }
}

export class Net {
  /**
   * @param transport   {send, onMessage}
   * @param store       store.js instance
   * @param pubHex      our peer id (used to address anti-entropy fills)
   * @param checkSheepId async (genomeJson) => sheep_id hex, via the wasm worker
   * @param onSheep/onVote  callbacks fired when a NEW record is accepted
   */
  constructor({ transport, store, pubHex, checkSheepId, onSheep, onVote }) {
    Object.assign(this, { transport, store, pubHex, checkSheepId, onSheep, onVote });
    this.peers = new Map(); // pubHex -> last inv timestamp
  }

  async start() {
    this.transport.onMessage((msg) => this._recv(msg).catch(console.error));
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
      this.transport.send({ kind: 'sheep', record });
      this.onSheep?.(record);
    }
  }

  async publishVote(record) {
    record.key = voteKey(record);
    if (await this.store.addVote(record)) {
      this.transport.send({ kind: 'vote', record });
      this.onVote?.(record);
    }
  }

  // -- receiving --------------------------------------------------------------

  async _recv(msg) {
    switch (msg.kind) {
      case 'sheep': return this._ingestSheep(msg.record);
      case 'vote': return this._ingestVote(msg.record);
      case 'inv': return this._onInv(msg);
      case 'data': {
        if (msg.to !== this.pubHex) return;
        for (const r of msg.sheep || []) await this._ingestSheep(r);
        for (const r of msg.votes || []) await this._ingestVote(r);
        return;
      }
    }
  }

  async _ingestSheep(r) {
    if (!r || !HEX64.test(r.id) || typeof r.genome !== 'string') return;
    if (!Number.isInteger(r.gen) || !HEX64.test(r.author ?? '')) return;
    if (r.parents && !(Array.isArray(r.parents) && r.parents.every((p) => HEX64.test(p)))) return;
    if (!(await verify(r.author, r.sig, sheepSignBytes(r)))) return;
    if ((await this.checkSheepId(r.genome)) !== r.id) return; // forged id or non-canonical genome
    if (await this.store.addSheep(r)) this.onSheep?.(r);
  }

  async _ingestVote(v) {
    if (!v || !HEX64.test(v.sheepId) || !HEX64.test(v.voter) || !Number.isInteger(v.gen)) return;
    if (!Array.isArray(v.chunkHashes) || v.chunkHashes.length !== PROOF_SPEC.nChunks) return;
    if (!v.chunkHashes.every((h) => HEX64.test(h))) return;
    if (!(await verify(v.voter, v.sig, voteSignBytes(v)))) return;
    v.key = voteKey(v);
    if (await this.store.addVote(v)) this.onVote?.(v);
  }

  // -- anti-entropy ------------------------------------------------------------

  async _sendInv() {
    const sheep = (await this.store.allSheep()).filter((r) => !r.baked).map((r) => r.id);
    const votes = (await this.store.allVotes()).map((v) => v.key);
    this.transport.send({ kind: 'inv', from: this.pubHex, sheep, votes });
  }

  async _onInv(msg) {
    if (!msg.from || msg.from === this.pubHex) return;
    this.peers.set(msg.from, Date.now());
    const theirSheep = new Set(msg.sheep || []);
    const theirVotes = new Set(msg.votes || []);
    const sheep = (await this.store.allSheep()).filter((r) => !r.baked && !theirSheep.has(r.id));
    const votes = (await this.store.allVotes()).filter((v) => !theirVotes.has(v.key));
    if (sheep.length || votes.length) {
      this.transport.send({ kind: 'data', to: msg.from, sheep, votes });
    }
  }
}
