// net.js — gossip + anti-entropy over a pluggable transport (batch era).
//
// The protocol logic is transport-agnostic (see ARCHITECTURE.md). The dev
// transport is BroadcastChannel: every same-origin browsing context joins the
// bus, so two tabs (?peer=1 / ?peer=2) form a real two-peer network with no
// server. The js-libp2p transport slots in behind the same two methods later.
//
// The unit of work/contribution/vote is a BATCH: a deterministic slice of one
// animation frame's sample stream. Small signed records gossip normally; the
// heavy accumulated histograms NEVER ride the bus — they are fetched
// point-to-point (want-render / render-data) and pass the Verification gate
// before anything is accepted or handed to the app.
//
// Wire messages:
//   {kind:'sheep',   record}                       new sheep (signed releases / derived)
//   {kind:'batch',   record}                       new batch contribution (signed)
//   {kind:'fraud',   record}                       confirmed-on-receipt fraud proof
//   {kind:'inv',     from, d}                       per-(kind,gen) digests
//   {kind:'cov',     from, c}                       per-sheep batch-key-set digests
//   {kind:'bucket',  to, what, gen, keys}           keys for one mismatched bucket
//   {kind:'covreq',  to, sheepId}                   ask for a sheep's batch keys
//   {kind:'covkeys', to, sheepId, keys}             reply: that sheep's batch keys
//   {kind:'want-items', to, what, keys}             pull specific records
//   {kind:'data',    to, sheep, batches, fraud}     anti-entropy fill, addressed
//   {kind:'want-render', from, sheepId, frame}      ask for a frame's merged hist
//   {kind:'render-data', to, sheepId, frame, buf, batchKeys}  the heavy hist (addressed)
//
// Ingest validation (drop, don't propagate, on failure): shape, sizes,
// signature over canonical sign-bytes. The heavy render-data additionally runs
// the Verification gate (injected verifyRender) before onRender.

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
/** Tiles a user must have contributed to BOTH parents before a bred child they
 *  release is admitted to the flock. Enforced deterministically in gens.js (a
 *  release lacking the evidence is excluded by every peer), so it's protocol,
 *  not a client courtesy. */
export const BREED_MIN_TILES = 4;

/** Render spec a batch animates against. One tier, full quality (no quality
 *  tiers in the batch era). The contributed `spp` is BATCH_SPP. */
export const BATCH_SPEC = { width: 384, height: 384, ss: 1, nFrames: 64 };
export const BATCH_SPP = 640_000;

export const HEX64 = /^[0-9a-f]{64}$/;

// -- batch contribution (the unit of work, contribution, and vote) -----------
//
// batchKey = sheepId:frame:idx. A batch's `hash` commits to the integer
// histogram of rendering (frame, idx); re-rendering and comparing is the audit
// primitive. Each accepted, distinct, verified batch from a non-banned key is
// one vote for batch.sheepId in batch.gen.

export const batchKey = (b) => `${b.sheepId}:${b.frame}:${b.idx}`;
export const batchSignBytes = (b) =>
  utf8('batch|' + [b.sheepId, b.frame, b.idx, b.hash, b.spp, b.gen].join('|'));
/** v1: one batch, one vote. Kept as a function so weighting can change later. */
export const voteValue = (_b) => 1;

// -- sheep --------------------------------------------------------------------
//
// origin in seed|release|pair|mutant|immigrant. Sheep are content-addressed
// (id = sheep_id(genome)). Only releases are signed; derived sheep
// (pair/mutant/immigrant) and seeds are unsigned and recomputable from public
// data, so every peer reconstructs identical records.
export const sheepSignBytes = (r) =>
  utf8(`sheep|${r.id}|${(r.parents || []).join(',')}|${r.gen}|${r.author}`);

// -- fraud proof --------------------------------------------------------------
//
// "Batch `batchKey` should hash to `expected`, not what the contributor signed."
// Objectively checkable by re-rendering that one batch. A confirmed proof bans
// the offending contributor everywhere (all their votes excluded).
export const fraudSignBytes = (f) =>
  utf8('fraud|' + [f.batchKey, f.expected, f.reporter].join('|'));

/** BroadcastChannel bus name — bumped on wire-format breaks. */
export const CHANNEL = 'sheep-net-v11';

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
   * @param transport      {send, onMessage}
   * @param store          store.js instance
   * @param identity       { pubHex, pair } — pair only needed if net signs;
   *                        records arrive pre-signed via publishX in practice.
   * @param sign           async (pair, bytes) => sigHex (from identity.js)
   * @param verify         async (pubHex, sigHex, bytes) => bool (defaults to identity.verify)
   * @param checkBatchHash async (genomeJson, sheepId, frame, idx) => hash —
   *                        re-renders one batch in the worker; used to confirm
   *                        a fraud claim before believing it.
   * @param verifyRender   async ({sheepId, frame, hist, batchKeys}) => bool —
   *                        the Verification gate: re-renders a random sample of
   *                        the claimed batches, checks each batch_hash against a
   *                        stored batch record AND subtract_check vs hist, and
   *                        total_count(hist) == sum of spp over claimed batches.
   * @param lookupSheep    async (id) => record|undefined (incl. baked) so net
   *                        can fetch a genome to verify.
   * @param onSheep/onBatch/onFraud  fired when a NEW record is accepted.
   * @param onRender       (sheepId, frame, hist) — verified histogram for the app.
   */
  constructor({ transport, store, identity, sign, verify: verifyFn,
                checkBatchHash, checkSheepId, verifyRender, lookupSheep,
                onSheep, onBatch, onFraud, onRender }) {
    this.transport = transport;
    this.store = store;
    this.identity = identity || {};
    this.pubHex = this.identity.pubHex;
    this.pair = this.identity.pair;
    this.sign = sign;
    this.verify = verifyFn || verify;
    this.checkBatchHash = checkBatchHash;
    this.checkSheepId = checkSheepId; // async (genomeJson) => sheep_id hex
    this.verifyRender = verifyRender;
    this.lookupSheep = lookupSheep;
    this.onSheep = onSheep;
    this.onBatch = onBatch;
    this.onFraud = onFraud;
    this.onRender = onRender;
    this.peers = new Map(); // pubHex -> last inv timestamp
    this.banned = new Set(); // contributors with a confirmed fraud proof
    // Wire telemetry (stress testing / swarm page): message counts + bytes.
    this.counts = { sent: {}, recv: {}, sentBytes: 0, recvBytes: 0 };
  }

  _send(msg) {
    this.counts.sent[msg.kind] = (this.counts.sent[msg.kind] ?? 0) + 1;
    try { this.counts.sentBytes += JSON.stringify(msg).length; } catch { /* buffers */ }
    this.transport.send(msg);
  }

  async start() {
    // Seed the ban set from any fraud proofs we already hold.
    for (const f of await this.store.allFraud()) {
      const b = await this._batchByKey(f.batchKey);
      if (b) this.banned.add(b.contributor);
      if (f.contributor) this.banned.add(f.contributor);
    }
    this.transport.onMessage((msg) => {
      this.counts.recv[msg.kind] = (this.counts.recv[msg.kind] ?? 0) + 1;
      try { this.counts.recvBytes += JSON.stringify(msg).length; } catch { /* buffers */ }
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

  isBanned(pubHex) {
    return this.banned.has(pubHex);
  }

  // -- publishing (records must already be signed where applicable) -----------

  async publishSheep(record) {
    if (await this.store.addSheep(record)) {
      this.markDirty();
      this._send({ kind: 'sheep', record });
      this.onSheep?.(record);
    }
  }

  async publishBatch(record) {
    record.key = batchKey(record);
    if (await this.store.addBatch(record)) {
      this.markDirty();
      this._send({ kind: 'batch', record });
      this.onBatch?.(record);
    }
  }

  async publishFraud(record) {
    record.key = record.batchKey;
    if (await this.store.addFraud(record)) {
      const off = await this._batchByKey(record.batchKey);
      if (off) this.banned.add(off.contributor);
      this.markDirty();
      this._send({ kind: 'fraud', record });
      this.onFraud?.(record);
    }
  }

  /** Ask the swarm for a sheep's verified accumulated histogram of one frame. */
  requestRender(sheepId, frame) {
    this._send({ kind: 'want-render', from: this.pubHex, sheepId, frame });
  }

  // -- receiving --------------------------------------------------------------

  async _recv(msg) {
    switch (msg.kind) {
      case 'sheep': return this._ingestSheep(msg.record);
      case 'batch': return this._ingestBatch(msg.record);
      case 'fraud': return this._ingestFraud(msg.record);
      case 'inv': return this._onInv(msg);
      case 'cov': return this._onCov(msg);
      case 'bucket': return this._onBucket(msg);
      case 'covreq': return this._onCovReq(msg);
      case 'covkeys': return this._onCovKeys(msg);
      case 'want-items': return this._onWantItems(msg);
      case 'data': {
        if (msg.to !== this.pubHex) return;
        for (const r of msg.sheep || []) await this._ingestSheep(r);
        for (const r of msg.batches || []) await this._ingestBatch(r);
        for (const r of msg.fraud || []) await this._ingestFraud(r);
        return;
      }
      case 'want-render': return this._onWantRender(msg);
      case 'render-data': return this._onRenderData(msg);
    }
  }

  async _ingestSheep(r) {
    if (!r || !HEX64.test(r.id) || typeof r.genome !== 'string') return;
    if (!Number.isInteger(r.gen)) return;
    const ORIGINS = ['seed', 'release', 'pair', 'mutant', 'immigrant'];
    if (!ORIGINS.includes(r.origin)) return;
    if (r.parents != null && !(Array.isArray(r.parents) && r.parents.every((p) => HEX64.test(p)))) return;
    // Releases are signed by their author; derived/seed sheep are unsigned.
    if (r.origin === 'release') {
      if (!HEX64.test(r.author ?? '')) return;
      // Storage sanity bound per (author, gen) — the *selection* cap
      // (AUTHOR_GEN_CAP, deterministic lowest-ids) is applied in gens.js.
      const byAuthor = (await this.store.allSheep())
        .filter((s) => s.author === r.author && s.gen === r.gen).length;
      if (byAuthor >= AUTHOR_GEN_CAP * 3) return;
      if (!(await this.verify(r.author, r.sig, sheepSignBytes(r)))) return;
    } else if (r.author != null && !HEX64.test(r.author)) {
      return; // derived sheep carry no author (or null)
    }
    // Validate the id is the canonical sheep_id of the genome (content-address).
    if (this.checkSheepId) {
      if ((await this.checkSheepId(r.genome)) !== r.id) return; // forged id / non-canonical
    }
    if (await this.store.addSheep(r)) { this.markDirty(); this.onSheep?.(r); }
  }

  async _ingestBatch(b) {
    if (!b || !HEX64.test(b.sheepId) || !HEX64.test(b.hash)) return;
    if (!Number.isInteger(b.frame) || b.frame < 0 || b.frame >= BATCH_SPEC.nFrames) return;
    if (!Number.isInteger(b.idx) || b.idx < 0) return;
    if (!Number.isInteger(b.spp) || b.spp <= 0) return;
    if (!Number.isInteger(b.gen)) return;
    if (!HEX64.test(b.contributor ?? '')) return;
    if (this.banned.has(b.contributor)) return; // discard everything from banned keys
    if (!(await this.verify(b.contributor, b.sig, batchSignBytes(b)))) return;
    // The sheep must be known (so its coverage/genome are addressable).
    const sheep = await this.lookupSheep?.(b.sheepId);
    if (!sheep) return;
    b.key = batchKey(b);
    if (await this.store.addBatch(b)) { this.markDirty(); this.onBatch?.(b); }
  }

  /** A fraud claim is believed only after we verify it OURSELVES: check the
   *  reporter signature, that we hold the disputed batch, then re-render that
   *  one batch and confirm the contributor signed a wrong hash. Cost: 1 batch.
   *  Requires the sheep's genome — if absent, drop; anti-entropy re-offers it. */
  async _ingestFraud(f) {
    if (!f || typeof f.batchKey !== 'string' || !HEX64.test(f.expected ?? '')) return;
    if (!HEX64.test(f.reporter ?? '')) return;
    if (!(await this.verify(f.reporter, f.sig, fraudSignBytes(f)))) return;
    const b = await this._batchByKey(f.batchKey);
    if (!b) return; // can't verify the accusation yet
    if (b.hash === f.expected) return; // not actually a dispute
    const sheep = await this.lookupSheep?.(b.sheepId);
    if (!sheep) return;
    const actual = await this.checkBatchHash(sheep.genome, b.sheepId, b.frame, b.idx);
    if (actual !== f.expected || actual === b.hash) return; // false accusation
    f.key = f.batchKey;
    f.contributor = b.contributor; // record who is banned (for ban replay on load)
    if (await this.store.addFraud(f)) {
      this.banned.add(b.contributor);
      this.markDirty();
      this.onFraud?.(f);
    }
  }

  // -- helpers -----------------------------------------------------------------

  async _batchByKey(key) {
    return (await this.store.allBatches()).find((b) => b.key === key || batchKey(b) === key);
  }

  // -- the Verification gate (want-render / render-data) -----------------------

  async _onWantRender(msg) {
    if (!msg.sheepId || !Number.isInteger(msg.frame) || msg.from === this.pubHex) return;
    const buf = await this.store.getRender(msg.sheepId, msg.frame);
    if (!buf) return;
    const batchKeys = (await this.store.batchesForSheep(msg.sheepId))
      .filter((b) => b.frame === msg.frame)
      .map((b) => b.key ?? batchKey(b));
    this._send({
      kind: 'render-data', to: msg.from,
      sheepId: msg.sheepId, frame: msg.frame, buf, batchKeys,
    });
  }

  async _onRenderData(msg) {
    if (msg.to !== this.pubHex || !msg.sheepId || !Number.isInteger(msg.frame)) return;
    if (!msg.buf || !Array.isArray(msg.batchKeys)) return;
    // The heavy histogram is NEVER trusted: rebuild a typed view and run the
    // injected gate (re-render sample, count conservation, subtract_check).
    const hist = new BigUint64Array(msg.buf);
    let ok = false;
    try {
      ok = await this.verifyRender?.({
        sheepId: msg.sheepId, frame: msg.frame, hist, batchKeys: msg.batchKeys,
      });
    } catch (e) { console.error(e); ok = false; }
    if (!ok) return; // discard; verifyRender publishes a fraud proof if provable
    await this.store.putRender(msg.sheepId, msg.frame, msg.buf);
    this.onRender?.(msg.sheepId, msg.frame, hist);
  }

  // -- anti-entropy ------------------------------------------------------------
  //
  // inv carries one short digest per (kind, generation) bucket for sheep /
  // batches / fraud; only mismatched buckets exchange keys, then records. cov
  // carries, per sheep, a digest of its batch-key set so peers can find missing
  // batches even within an otherwise-matching generation bucket.

  _genOf(kind, rec) {
    if (kind === 'sheep') return rec.gen ?? 0;
    if (kind === 'batches') return rec.gen ?? 0;
    // fraud has no own gen; bucket it by the offending batch's frame/idx-free
    // key is impossible, so bucket all fraud into gen 0 (small set).
    return 0;
  }

  async _buckets() {
    if (this._bucketCache && !this._dirty) return this._bucketCache;
    const out = { sheep: new Map(), batches: new Map(), fraud: new Map() };
    const cov = new Map(); // sheepId -> [batchKey]
    const add = (kind, g, key) => {
      if (!out[kind].has(g)) out[kind].set(g, []);
      out[kind].get(g).push(key);
    };
    for (const r of await this.store.allSheep()) {
      if (!r.baked) add('sheep', this._genOf('sheep', r), r.id);
    }
    for (const b of await this.store.allBatches()) {
      const k = b.key ?? batchKey(b);
      add('batches', this._genOf('batches', b), k);
      if (!cov.has(b.sheepId)) cov.set(b.sheepId, []);
      cov.get(b.sheepId).push(k);
    }
    for (const f of await this.store.allFraud()) {
      add('fraud', this._genOf('fraud', f), f.key ?? f.batchKey);
    }
    const digests = { sheep: {}, batches: {}, fraud: {} };
    for (const kind of ['sheep', 'batches', 'fraud']) {
      for (const [g, keys] of out[kind]) {
        keys.sort();
        digests[kind][g] = (await sha256Hex(utf8(keys.join(',')))).slice(0, 16);
      }
    }
    const covDigests = {};
    for (const [sheepId, keys] of cov) {
      keys.sort();
      covDigests[sheepId] = (await sha256Hex(utf8(keys.join(',')))).slice(0, 16);
    }
    this._bucketCache = { keys: out, digests, cov, covDigests };
    this._dirty = false;
    return this._bucketCache;
  }

  markDirty() {
    this._dirty = true;
  }

  async _sendInv() {
    const { digests, covDigests } = await this._buckets();
    this._send({ kind: 'inv', from: this.pubHex, d: digests });
    this._send({ kind: 'cov', from: this.pubHex, c: covDigests });
  }

  async _onInv(msg) {
    if (!msg.from || msg.from === this.pubHex || !msg.d) return;
    this.peers.set(msg.from, Date.now());
    const { keys, digests } = await this._buckets();
    let sent = 0;
    for (const kind of ['sheep', 'batches', 'fraud']) {
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

  /** Per-sheep coverage: where a peer's batch-key-set digest differs from ours,
   *  ask for that sheep's full batch-key list (small) so we can diff and fill. */
  async _onCov(msg) {
    if (!msg.from || msg.from === this.pubHex || !msg.c) return;
    this.peers.set(msg.from, Date.now());
    const { covDigests } = await this._buckets();
    let sent = 0;
    const sheepIds = new Set([...Object.keys(covDigests), ...Object.keys(msg.c)]);
    for (const sheepId of sheepIds) {
      if (sent >= 6) return;
      if (covDigests[sheepId] !== msg.c[sheepId]) {
        this._send({ kind: 'covreq', to: msg.from, sheepId });
        sent++;
      }
    }
  }

  async _onCovReq(msg) {
    if (msg.to !== this.pubHex || !msg.sheepId) return;
    const keys = (await this.store.batchesForSheep(msg.sheepId)).map((b) => b.key ?? batchKey(b));
    this._send({ kind: 'covkeys', to: msg.from, sheepId: msg.sheepId, keys });
  }

  async _onCovKeys(msg) {
    if (msg.to !== this.pubHex || !msg.sheepId || !Array.isArray(msg.keys)) return;
    const mine = new Set((await this.store.batchesForSheep(msg.sheepId)).map((b) => b.key ?? batchKey(b)));
    const want = msg.keys.filter((k) => !mine.has(k));
    if (want.length) {
      this._send({ kind: 'want-items', to: msg.from, what: 'batches', keys: want });
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
      this._send(await this._fillFor(msg.from, msg.what, new Set(lack)));
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
    const payload = await this._fillFor(msg.from, msg.what, wanted);
    if (payload.sheep.length || payload.batches.length || payload.fraud.length) {
      this._send(payload);
    }
  }

  async _fillFor(to, what, wantedSet) {
    const payload = { kind: 'data', to, sheep: [], batches: [], fraud: [] };
    if (what === 'sheep') {
      payload.sheep = (await this.store.allSheep()).filter((r) => wantedSet.has(r.id));
    } else if (what === 'batches') {
      payload.batches = (await this.store.allBatches())
        .filter((b) => wantedSet.has(b.key ?? batchKey(b)));
    } else {
      payload.fraud = (await this.store.allFraud()).filter((f) => wantedSet.has(f.key ?? f.batchKey));
    }
    return payload;
  }

  // -- selection tally ---------------------------------------------------------

  /** Tally a sheep in a generation: distinct verified batches whose record
   *  carries gen=g, from non-banned contributors. One batch, one vote. */
  async tally(sheepId, g) {
    const batches = await this.store.batchesForSheep(sheepId);
    let n = 0;
    const seen = new Set();
    for (const b of batches) {
      if (b.gen !== g) continue;
      if (this.banned.has(b.contributor)) continue;
      const k = b.key ?? batchKey(b);
      if (seen.has(k)) continue;
      seen.add(k);
      n += voteValue(b);
    }
    return n;
  }
}
