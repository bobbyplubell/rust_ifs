// identity.js — per-peer Ed25519 identity via WebCrypto, stored in IndexedDB.
//
// `?peer=N` namespaces the identity (and the store, see store.js) so several
// same-origin tabs can act as DISTINCT peers on the shared BroadcastChannel —
// the local two-tab network simulator. Without the param all tabs share one
// identity, which is the correct production behavior (one visitor = one peer).

import { hex, unhex } from './hash.js';

export const PEER_NS = new URLSearchParams(location.search).get('peer') || '0';

const ALG = { name: 'Ed25519' };

function idb(name) {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(name, 1);
    req.onupgradeneeded = () => req.result.createObjectStore('kv');
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function kvGet(db, key) {
  return new Promise((resolve, reject) => {
    const req = db.transaction('kv').objectStore('kv').get(key);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function kvPut(db, key, value) {
  return new Promise((resolve, reject) => {
    const req = db.transaction('kv', 'readwrite').objectStore('kv').put(value, key);
    req.onsuccess = () => resolve();
    req.onerror = () => reject(req.error);
  });
}

/** Load (or create on first visit) this peer's keypair. The private key is
 *  non-extractable; CryptoKeys are structured-cloneable so they store as-is. */
export async function loadIdentity() {
  const db = await idb(`sheep-id-${PEER_NS}`);
  let pair = await kvGet(db, 'keypair');
  if (!pair) {
    pair = await crypto.subtle.generateKey(ALG, false, ['sign', 'verify']);
    await kvPut(db, 'keypair', pair);
  }
  const raw = new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey));
  db.close();
  return { pair, pubHex: hex(raw) };
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
