// api.js — the coordinator HTTP client (v2).
//
// One function per endpoint in API.md. Plain `fetch`; JSON over HTTPS; no
// WebSocket / SSE. Live state is obtained by POLLING getFlock() / getMe() on a
// timer in the page code — this module is just the request layer.
//
// THE INTEGRATION CONTRACT lives in `canonicalMessage()` below: every mutating
// request is signed with the caller's Ed25519 key (identity.js), and the bytes
// that get signed MUST be byte-identical to what the Rust coordinator
// reconstructs and verifies. Get that wrong and every signed call is rejected.

import { COORDINATOR } from '../config.js';
import { sign } from './identity.js';
import { utf8 } from './hash.js';

const BASE = `${COORDINATOR.replace(/\/+$/, '')}/api`;

// ---- canonical signed-request scheme (per API.md "Identity & auth") ---------
//
// Canonical message = the request's JSON body with `sig` omitted, keys sorted,
// compact (no whitespace), UTF-8. We canonicalize RECURSIVELY (sorted keys at
// every nesting level) so nested objects — e.g. each Result inside `results` —
// serialize deterministically too; a serde_json::to_string over a value with
// sorted map keys on the Rust side reproduces exactly these bytes.
//
//   - objects: keys sorted lexicographically (by UTF-16 code unit, JS default),
//     emitted as {"k":v,...} with no spaces.
//   - arrays: order preserved, elements canonicalized in place.
//   - strings/numbers/booleans/null: standard JSON.stringify (compact).
//   - `undefined` keys are dropped (JSON has no undefined).
//
// `sig` is excluded by construction: we canonicalize the body BEFORE adding it.
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

/** The exact UTF-8 bytes signed for a request body (sig omitted). */
export function canonicalMessage(body) {
  const { sig: _sig, ...rest } = body;
  return canonicalize(rest);
}

// Build a signed body: take the caller's payload, stamp `pub` + `nonce`,
// canonicalize (sig omitted), sign those bytes, attach `sig`. Returns the full
// body object ready to POST as JSON.
async function signBody(identity, payload) {
  const body = {
    ...payload,
    pub: identity.pubHex,
    // nonce = a monotonically increasing client counter (timestamp ms is a
    // simple, replay-resistant choice the coordinator accepts as "last-seen").
    nonce: Date.now(),
  };
  body.sig = await sign(identity.pair, utf8(canonicalMessage(body)));
  return body;
}

// ---- low-level fetch helpers ------------------------------------------------

async function getJSON(path) {
  const res = await fetch(`${BASE}${path}`, { headers: { accept: 'application/json' } });
  if (!res.ok) throw await httpError(res);
  return res.json();
}

async function postSigned(path, identity, payload) {
  const body = await signBody(identity, payload);
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
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

// ---- endpoints (one per API.md route) ---------------------------------------

/** GET /api/flock — current living sheep + gen + countdown. */
export function getFlock() {
  return getJSON('/flock');
}

/** GET /api/sheep/:id — full detail (flock entry + genome/coverage/stats). */
export function getSheep(id) {
  return getJSON(`/sheep/${encodeURIComponent(id)}`);
}

/** GET /api/hall — Hall of Fame (same entry shape as /flock). */
export function getHall() {
  return getJSON('/hall');
}

/** GET /api/me?pub=HEX — this key's credits / reputation / tallies. */
export function getMe(pubHex) {
  return getJSON(`/me?pub=${encodeURIComponent(pubHex)}`);
}

/** The merged-loop video URL for a sheep (used directly as a <video> src). */
export function videoUrl(id) {
  return `${BASE}/video/${encodeURIComponent(id)}`;
}

/** POST /api/assign (signed) — pull a bundle of WorkUnits (+ optional audits). */
export function assign(identity, sheepId) {
  const payload = {};
  if (sheepId) payload.sheepId = sheepId;
  return postSigned('/assign', identity, payload);
}

/** POST /api/submit (signed) — return rendered Results (+ optional audit reports). */
export function submit(identity, results, auditReports) {
  const payload = { results };
  if (auditReports && auditReports.length) payload.audit_reports = auditReports;
  return postSigned('/submit', identity, payload);
}

/** POST /api/vote (signed) — spend 1 credit to back a sheep this gen. */
export function vote(identity, sheepId) {
  return postSigned('/vote', identity, { sheepId });
}

/** POST /api/breed (signed) — propose a pairing; coordinator breeds + adds child. */
export function breed(identity, parentA, parentB) {
  return postSigned('/breed', identity, { parentA, parentB });
}
