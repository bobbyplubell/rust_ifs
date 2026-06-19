// identity.js — the user's Ed25519 identity. THE KEY IS THE LOGIN.
//
// The keypair *is* the identity: there is no account, no password, no server
// row to recover. Whoever holds the private key is, cryptographically, that
// user (see API.md "Identity & auth" — every mutating request is signed with
// it). So the key must be (a) persisted across visits and (b) exportable, so a
// user can back it up and carry it between devices ("log in as this key").
//
// STORAGE — localStorage, as a portable PKCS#8 blob.
//   WebCrypto can only export a key marked `extractable`, and to back the key
//   up we MUST be able to export it — so the private key is generated
//   extractable. We serialize it as PKCS#8 (the standard DER encoding
//   WebCrypto round-trips for Ed25519; it carries the raw 32-byte seed) and
//   store it base64 in localStorage. localStorage (not IndexedDB) so the value
//   is a plain string the export/import flow can hand around as text.
//
// The identity is shared across gateways (all the same Pages origin → same
// localStorage), and there is one shared swarm, so your standing
// (credits/reputation/tiles) is the same whichever gateway you use. See
// config.js / me.html.

import { hex, unhex } from './hash.js';

// `?peer=N` namespaces the stored identity so several same-origin tabs can act
// as DISTINCT users (a local multi-user simulator). Without it all tabs share
// one identity — the correct production behavior (one visitor = one key).
export const PEER_NS = new URLSearchParams(location.search).get('peer') || '0';

const ALG = { name: 'Ed25519' };
const STORE_KEY = `sheep-id-${PEER_NS}`; // localStorage slot for the PKCS#8 blob

// ---- base64 <-> bytes (PKCS#8 is binary; localStorage holds text) -----------

function b64encode(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return btoa(s);
}

function b64decode(str) {
  const bin = atob(str.trim());
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

// ---- key (de)serialization --------------------------------------------------

// Import a stored PKCS#8 private key, then DERIVE the public key from it so we
// hold a full { privateKey, publicKey } pair. WebCrypto can't export a public
// key out of a PKCS#8 private import directly, so we round-trip the private key
// to JWK, strip the private scalar `d`, and re-import the remaining x/y as the
// public key — a standard Ed25519 maneuver that works in browsers.
async function pairFromPkcs8(pkcs8) {
  const privateKey = await crypto.subtle.importKey('pkcs8', pkcs8, ALG, true, ['sign']);
  const jwk = await crypto.subtle.exportKey('jwk', privateKey);
  const pubJwk = { kty: jwk.kty, crv: jwk.crv, x: jwk.x, ext: true, key_ops: ['verify'] };
  const publicKey = await crypto.subtle.importKey('jwk', pubJwk, ALG, true, ['verify']);
  return { privateKey, publicKey };
}

async function pubHexOf(publicKey) {
  const raw = new Uint8Array(await crypto.subtle.exportKey('raw', publicKey));
  return hex(raw);
}

/** The portable backup string for a pair: base64 of its PKCS#8 private key. */
async function serialize(pair) {
  const pkcs8 = new Uint8Array(await crypto.subtle.exportKey('pkcs8', pair.privateKey));
  return b64encode(pkcs8);
}

// ---- public surface ---------------------------------------------------------

/** Load (or mint on first visit) this user's keypair from localStorage. */
export async function loadIdentity() {
  let stored = localStorage.getItem(STORE_KEY);
  let pair;
  if (stored) {
    pair = await pairFromPkcs8(b64decode(stored));
  } else {
    // Generate EXTRACTABLE so the user can later export/back-up the key.
    pair = await crypto.subtle.generateKey(ALG, true, ['sign', 'verify']);
    localStorage.setItem(STORE_KEY, await serialize(pair));
  }
  return { pair, pubHex: await pubHexOf(pair.publicKey) };
}

/**
 * Serialize the CURRENT identity to a portable backup string (base64 PKCS#8).
 * This IS the secret — anyone holding it is this user. Round-trips through
 * importKey() back to the identical pubHex.
 */
export async function exportKey() {
  const stored = localStorage.getItem(STORE_KEY);
  if (stored) return stored; // already the portable form
  // No stored key yet (e.g. exportKey called before loadIdentity): mint one.
  const id = await loadIdentity();
  return serialize(id.pair);
}

/**
 * Replace the stored identity with a pasted backup string ("log in as this
 * key"). Validates by importing + deriving the pubkey before overwriting, so a
 * malformed paste can't wipe the existing key. Returns the new pubHex; the
 * caller is expected to reload the page so every module picks up the new key.
 */
export async function importKey(str) {
  const pkcs8 = b64decode(str);
  const pair = await pairFromPkcs8(pkcs8);          // throws on bad input
  const pubHex = await pubHexOf(pair.publicKey);
  // Re-serialize from the imported pair (normalizes the stored blob).
  localStorage.setItem(STORE_KEY, await serialize(pair));
  return pubHex;
}

export async function sign(pair, bytes) {
  return hex(new Uint8Array(await crypto.subtle.sign(ALG, pair.privateKey, bytes)));
}

export async function verify(pubHex, sigHex, bytes) {
  try {
    const key = await crypto.subtle.importKey('raw', unhex(pubHex), ALG, true, ['verify']);
    return await crypto.subtle.verify(ALG, key, unhex(sigHex), bytes);
  } catch {
    return false;
  }
}
