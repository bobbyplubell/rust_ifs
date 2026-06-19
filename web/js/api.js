// api.js — the v3 node HTTP client.
//
// The v3 node (crates/sheep-node) serves a read-only "watch face" + a 1:1 REST
// skin over the gossip protocol (see crates/sheep-node/src/http.rs):
//
//   reads  : GET /api/flock, GET /api/sheep/:id, GET /api/video/:id, GET /api/hall
//   writes : POST /api/msg   — a signed Envelope (or array of Envelopes)
//   work   : GET  /api/assign?pub=<hex> — advisory blocks + audit tiles
//
// THE INTEGRATION CONTRACT lives in the Envelope signing below. Every write is a
// signed Envelope `{v,t,from,ts,body,sig}` whose `sig` is Ed25519 over
// `canonical(envelope-minus-sig)`, where canonical = recursive-sorted-keys,
// sig-excluded, compact UTF-8. Those bytes MUST be byte-identical to what
// `sheep-proto`'s `Envelope::canonical()` reconstructs — a mismatch and the node
// rejects every signed call with HTTP 400 "bad signature". `canonicalize()`
// below is the verified byte-match (it is the same algorithm sheep-proto's
// `canonical::canonicalize_value` runs: serde_json with recursively-sorted
// keys, compact, sig removed from the top-level object).

import { COORDINATOR } from '../config.js';
import { sign } from './identity.js';
import { utf8 } from './hash.js';

const BASE = `${COORDINATOR.replace(/\/+$/, '')}/api`;

// Envelope version — must match sheep_proto::envelope::ENVELOPE_VERSION (1).
export const ENVELOPE_VERSION = 1;

// Envelope `t` tags — the gossip topic / message-type keys the engine routes on
// (crates/sheep-proto/src/proto.rs; engine.rs `apply` matches on these). The
// node's /api/msg hands the envelope to the SAME apply path inbound gossip uses.
export const T = {
  FLOCK: '/sheep/flock/1.0.0',   // births: Mint / Breed
  VOTES: '/sheep/votes',         // Vote (survival backing)
  PROGRESS: '/sheep/progress',   // Coverage / have
  PIECE: '/sheep/piece/1.0.0',   // PieceUpload (the heavy histogram artifact)
  ATTEST: '/sheep/attest',       // Attestation (audit report)
};

// ---- canonical signed bytes (byte-match with sheep-proto) --------------------
//
// Recursively sorted keys at EVERY nesting depth, compact (no whitespace),
// standard JSON scalars, `undefined` keys dropped. The top-level `sig` is
// excluded by construction (we canonicalize the envelope before attaching sig).
export function canonicalize(value) {
  if (value === null || typeof value !== 'object') {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) {
    return `[${value.map(canonicalize).join(',')}]`;
  }
  const keys = Object.keys(value).filter((k) => value[k] !== undefined).sort();
  const parts = keys.map((k) => `${JSON.stringify(k)}:${canonicalize(value[k])}`);
  return `{${parts.join(',')}}`;
}

/** The exact UTF-8 bytes signed for an envelope (`sig` omitted). */
export function envelopeCanonical(env) {
  const { sig: _sig, ...rest } = env;
  return canonicalize(rest);
}

/**
 * Build + sign a v3 Envelope. Mirrors sheep_proto::Envelope::sign: set `from`
 * to the signer's pub hex, canonicalize with `sig` empty, sign those bytes,
 * attach the hex signature.
 *
 * @param identity { pubHex, pair } from identity.js
 * @param t        envelope type tag (one of T.*)
 * @param body     the message body object (a sheep-proto msg type)
 * @param ts       sender timestamp; defaults to Date.now() (ms). Mint uses micros.
 */
export async function signEnvelope(identity, t, body, ts = Date.now()) {
  const env = {
    v: ENVELOPE_VERSION,
    t,
    from: identity.pubHex,
    ts,
    body,
    sig: '',
  };
  env.sig = await sign(identity.pair, utf8(envelopeCanonical(env)));
  return env;
}

// ---- low-level fetch helpers ------------------------------------------------

async function getJSON(path) {
  const res = await fetch(`${BASE}${path}`, { headers: { accept: 'application/json' } });
  if (!res.ok) throw await httpError(res);
  return res.json();
}

async function httpError(res) {
  let msg = `HTTP ${res.status}`;
  try {
    const j = await res.json();
    if (j && j.error) msg = j.error;
  } catch { /* non-JSON error body */ }
  const err = new Error(msg);
  err.status = res.status;
  return err;
}

/**
 * POST one or more signed Envelopes to /api/msg. Returns the node's per-item
 * result(s): a single envelope yields a flat `{accepted, credits, ...}`; an
 * array yields `{results:[...]}`. Throws on non-2xx (e.g. 400 bad signature).
 */
export async function postMsg(envelopeOrArray) {
  const res = await fetch(`${BASE}/msg`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(envelopeOrArray),
  });
  if (!res.ok) throw await httpError(res);
  return res.json();
}

// ---- read endpoints ---------------------------------------------------------

/** GET /api/flock — live flock snapshot: now_ms, self, sheep[] (with genome). */
export function getFlock() {
  return getJSON('/flock');
}

/** GET /api/sheep/:id — per-sheep detail (entry + frames_coverage + hall flag). */
export function getSheep(id) {
  return getJSON(`/sheep/${encodeURIComponent(id)}`);
}

/** GET /api/hall — the Hall of Fame (enshrined dead sheep). */
export function getHall() {
  return getJSON('/hall');
}

/** The merged-loop WebM video URL for a sheep (used directly as a <video> src). */
export function videoUrl(id) {
  return `${BASE}/video/${encodeURIComponent(id)}`;
}

/**
 * Resolve a (possibly node-relative) url like "/api/video/:id" against the
 * active gateway's origin, so cross-origin dev (static page on :8000 talking to
 * a node on :8080) loads it correctly. Absolute urls pass through unchanged.
 */
export function absoluteUrl(url) {
  if (!url) return url;
  if (/^https?:\/\//.test(url)) return url;
  const origin = BASE.replace(/\/api$/, '');
  return `${origin}${url.startsWith('/') ? '' : '/'}${url}`;
}

/**
 * GET /api/assign?pub=<hex> — advisory work hand-out: `{ blocks:[{block_id,
 * sheep_id, frame, idx, pass}], audits:[{sheep_id, frame, idx, pass}] }`.
 */
export function assign(pubHex, want) {
  const q = want ? `&want=${want}` : '';
  return getJSON(`/assign?pub=${encodeURIComponent(pubHex)}${q}`);
}

// ---- write helpers (one per message type) -----------------------------------

/** Vote: spend a credit to back a sheep's survival (T.VOTES). */
export async function vote(identity, sheepId, seq) {
  const env = await signEnvelope(identity, T.VOTES, { sheep_id: sheepId, seq });
  return postMsg(env);
}

/** Mint: birth a brand-new sheep at a resolution tier (T.FLOCK). */
export async function mint(identity, resolution, seq, tsMicros = Date.now() * 1000) {
  const body = {
    ts_micros: tsMicros,
    minter_pub: identity.pubHex,
    resolution,
    seq,
  };
  // A Mint's `ts` is the same micros the genome is derived from (engine reads
  // the body's ts_micros, but we set the envelope ts to match for clarity).
  const env = await signEnvelope(identity, T.FLOCK, body, tsMicros);
  return postMsg(env);
}

/** Breed: birth a sheep from two parents (T.FLOCK). */
export async function breed(identity, parentA, parentB, resolution, seq, seed) {
  const body = {
    parent_a: parentA,
    parent_b: parentB,
    seed: seed ?? (Date.now() & 0xffffffff),
    breeder_pub: identity.pubHex,
    resolution,
    seq,
  };
  const env = await signEnvelope(identity, T.FLOCK, body);
  return postMsg(env);
}

/** Coverage / have: "tile (sheep,frame,idx,pass) is confirmed at hash" (T.PROGRESS). */
export async function coverage(identity, sheepId, frame, idx, pass, hash) {
  const body = { sheep_id: sheepId, frame, idx, pass, hash };
  const env = await signEnvelope(identity, T.PROGRESS, body);
  return postMsg(env);
}

/** PieceUpload: the rendered tile histogram (T.PIECE). `count` is a STRING. */
export async function pieceUpload(identity, sheepId, frame, idx, pass, hash, count, histB64) {
  const body = {
    sheep_id: sheepId,
    frame,
    idx,
    pass,
    hash,
    count: String(count),
    hist_b64: histB64,
  };
  const env = await signEnvelope(identity, T.PIECE, body);
  return postMsg(env);
}

/** Attestation: "I re-rendered (sheep,frame,idx,pass) and got hash" (T.ATTEST). */
export async function attestation(identity, sheepId, frame, idx, pass, hash) {
  const body = { sheep_id: sheepId, frame, idx, pass, hash };
  const env = await signEnvelope(identity, T.ATTEST, body);
  return postMsg(env);
}
