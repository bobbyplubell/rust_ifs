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
export const SURVIVORS_K = 6;
/** Mutant clones of top survivors per active generation (variance injection). */
export const MUTANTS_PER_GEN = 2;
/** Fresh random immigrants per active generation (fresh blood forever). */
export const IMMIGRANTS_PER_GEN = 1;
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
/** Signed tally contribution: +weight or -weight. */
export const voteValue = (v) => (v.dir === -1 ? -1 : 1) * voteWeight(v);

export const HEX64 = /^[0-9a-f]{64}$/;

// Canonical bytes each signature covers. The genome itself is covered via the
// sheep id (sha-256 of canonical genome JSON). Releases carry a render proof
// (same chunk-hash scheme as votes, challenge H(id|author|gen)) — you cannot
// release a sheep you haven't rendered, which prices submission spam in CPU.
export const sheepSignBytes = (r) =>
  utf8(`sheep|${r.id}|${(r.parents || []).join(',')}|${r.gen}|${r.author}|${r.chunkHashes.join(',')}`);
// dir: +1 (keep alive) or -1 (cull). Both directions cost the full render
// proof — you must watch a sheep to condemn it. Net-negative sheep die.
export const voteSignBytes = (v) =>
  utf8(`vote|${v.sheepId}|${v.gen}|${v.voter}|${v.dir}|${v.tier}|${v.sumHash}|${v.chunkHashes.join(',')}`);

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
export const CHANNEL = 'sheep-net-v10';

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
    this._dirty = true;
    await this._sendInv();
    // Jittered interval: hundreds of peers must not beacon in lockstep.
    const tick = () => {
      this._invTimer = setTimeout(() => {
        this._sendInv().catch(console.error);
        tick();
      }, 4000 + Math.random() * 3000);
    };
    tick();
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
      this.markDirty();
      this._send({ kind: 'sheep', record });
      this.onSheep?.(record);
    }
  }

  async publishVote(record) {
    record.key = voteKey(record);
    if (await this.store.addVote(record)) {
      this.markDirty();
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
      this.markDirty();
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
      case 'bucket': return this._onBucket(msg);
      case 'want-items': return this._onWantItems(msg);
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
    if (await this.store.addSheep(r)) { this.markDirty(); this.onSheep?.(r); }
  }

  async _ingestVote(v) {
    if (!v || !HEX64.test(v.sheepId) || !HEX64.test(v.voter) || !Number.isInteger(v.gen)) return;
    if (!HEX64.test(v.sumHash ?? '')) return; // commitment to the summed histogram
    if (v.dir !== 1 && v.dir !== -1) return;
    const tier = PROOF_TIERS[v.tier];
    if (!tier) return;
    if (!Array.isArray(v.chunkHashes) || v.chunkHashes.length !== tier.nFrames) return;
    if (!v.chunkHashes.every((h) => HEX64.test(h))) return;
    if (!(await verify(v.voter, v.sig, voteSignBytes(v)))) return;
    v.key = voteKey(v);
    if (await this.store.addVote(v)) { this.markDirty(); this.onVote?.(v); }
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
    if (await this.store.addFraud(f)) { this.markDirty(); this.onFraud?.(f); }
  }

  // -- anti-entropy ------------------------------------------------------------

  // ---- digest-first anti-entropy --------------------------------------------
  //
  // Broadcasting full key lists is O(peers x records) and was projected to be
  // the first thing to drown a large swarm. Instead, inv carries one short
  // digest per (kind, generation) bucket; only mismatched buckets exchange
  // keys, and only missing records move.

  _genOf(kind, rec) {
    if (kind === 'sheep') return rec.gen ?? 0;
    if (kind === 'votes') return rec.gen ?? 0;
    return Number(rec.key?.split(':')[2] ?? 0); // fraud key = voter:sheep:gen:frame
  }

  async _buckets() {
    if (this._bucketCache && !this._dirty) return this._bucketCache;
    const out = { sheep: new Map(), votes: new Map(), fraud: new Map() };
    const add = (kind, gen, key) => {
      if (!out[kind].has(gen)) out[kind].set(gen, []);
      out[kind].get(gen).push(key);
    };
    for (const r of await this.store.allSheep()) {
      if (!r.baked) add('sheep', this._genOf('sheep', r), r.id);
    }
    for (const v of await this.store.allVotes()) add('votes', this._genOf('votes', v), v.key);
    for (const f of await this.store.allFraud()) add('fraud', this._genOf('fraud', f), f.key);
    const digests = { sheep: {}, votes: {}, fraud: {} };
    for (const kind of ['sheep', 'votes', 'fraud']) {
      for (const [g, keys] of out[kind]) {
        keys.sort();
        digests[kind][g] = (await sha256Hex(utf8(keys.join(',')))).slice(0, 16);
      }
    }
    this._bucketCache = { keys: out, digests };
    this._dirty = false;
    return this._bucketCache;
  }

  markDirty() {
    this._dirty = true;
  }

  async _sendInv() {
    const { digests } = await this._buckets();
    this._send({ kind: 'inv', from: this.pubHex, d: digests });
  }

  async _onInv(msg) {
    if (!msg.from || msg.from === this.pubHex || !msg.d) return;
    this.peers.set(msg.from, Date.now());
    const { keys, digests } = await this._buckets();
    // For every bucket where we differ (or they lack one we have), send our
    // keys for that bucket; the peer diffs and ships/requests records.
    let sent = 0;
    for (const kind of ['sheep', 'votes', 'fraud']) {
      const gens = new Set([
        ...Object.keys(digests[kind]),
        ...Object.keys(msg.d[kind] ?? {}),
      ]);
      for (const g of gens) {
        if (sent >= 4) return; // bound per-inv repair work
        if (digests[kind][g] !== (msg.d[kind] ?? {})[g]) {
          this._send({
            kind: 'bucket', to: msg.from, what: kind, gen: Number(g),
            keys: keys[kind].get(Number(g)) ?? [],
          });
          sent++;
        }
      }
    }
  }

  async _onBucket(msg) {
    if (msg.to !== this.pubHex) return;
    const { keys } = await this._buckets();
    const mine = new Set(keys[msg.what]?.get(msg.gen) ?? []);
    const theirs = new Set(msg.keys ?? []);
    // Ship records they lack.
    const lack = [...mine].filter((k) => !theirs.has(k));
    if (lack.length) {
      const lackSet = new Set(lack);
      const payload = { kind: 'data', to: msg.from, sheep: [], votes: [], fraud: [] };
      if (msg.what === 'sheep') {
        payload.sheep = (await this.store.allSheep()).filter((r) => lackSet.has(r.id));
      } else if (msg.what === 'votes') {
        payload.votes = (await this.store.allVotes()).filter((v) => lackSet.has(v.key));
      } else {
        payload.fraud = (await this.store.allFraud()).filter((f) => lackSet.has(f.key));
      }
      this._send(payload);
    }
    // Request records we lack (they answer with 'data').
    const want = [...theirs].filter((k) => !mine.has(k));
    if (want.length) {
      this._send({ kind: 'want-items', to: msg.from, what: msg.what, keys: want });
    }
  }

  async _onWantItems(msg) {
    if (msg.to !== this.pubHex) return;
    const wanted = new Set(msg.keys ?? []);
    const payload = { kind: 'data', to: msg.from, sheep: [], votes: [], fraud: [] };
    if (msg.what === 'sheep') {
      payload.sheep = (await this.store.allSheep()).filter((r) => wanted.has(r.id));
    } else if (msg.what === 'votes') {
      payload.votes = (await this.store.allVotes()).filter((v) => wanted.has(v.key));
    } else {
      payload.fraud = (await this.store.allFraud()).filter((f) => wanted.has(f.key));
    }
    if (payload.sheep.length || payload.votes.length || payload.fraud.length) {
      this._send(payload);
    }
  }
}
