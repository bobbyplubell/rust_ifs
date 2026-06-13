// store.js — the append-only fact sets (ARCHITECTURE.md "the sheep is the shared
// object" / "Data model"), persisted in IndexedDB. Grow-only: records are added
// if absent, never updated or removed. Namespaced per ?peer=N so dev tabs have
// independent state.
//
// v11 (batch-contribution era). Object stores:
//   sheep   keyPath 'id'   — content-addressed sheep records
//   batches keyPath 'key'  — batch contributions, key = batchKey(sheepId:f:i)
//   fraud   keyPath 'key'  — fraud proofs, key = batchKey of the offender
//   renders kv            — `${sheepId}:${frame}` -> verified histogram bytes
//                            (ArrayBuffer), a cache/serving store for the gate.

import { PEER_NS } from './identity.js';

export async function openStore() {
  const db = await new Promise((resolve, reject) => {
    const req = indexedDB.open(`sheep-store-v14-${PEER_NS}`, 1);
    req.onupgradeneeded = () => {
      req.result.createObjectStore('sheep', { keyPath: 'id' });
      req.result.createObjectStore('batches', { keyPath: 'key' });
      req.result.createObjectStore('fraud', { keyPath: 'key' });
      req.result.createObjectStore('renders'); // `${sheepId}:${frame}` -> ArrayBuffer
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });

  // add() (not put) so an existing key rejects: returns true only if new.
  function addIfAbsent(storeName, record) {
    return new Promise((resolve, reject) => {
      const tx = db.transaction(storeName, 'readwrite');
      const req = tx.objectStore(storeName).add(record);
      req.onsuccess = () => resolve(true);
      req.onerror = (e) => {
        e.preventDefault(); // swallow ConstraintError, keep the tx alive
        resolve(false);
      };
      tx.onerror = () => reject(tx.error);
    });
  }

  function getAll(storeName) {
    return new Promise((resolve, reject) => {
      const req = db.transaction(storeName).objectStore(storeName).getAll();
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  }

  function kvPut(storeName, key, value) {
    return new Promise((resolve, reject) => {
      const req = db.transaction(storeName, 'readwrite').objectStore(storeName).put(value, key);
      req.onsuccess = () => resolve();
      req.onerror = () => reject(req.error);
    });
  }

  function kvGet(storeName, key) {
    return new Promise((resolve, reject) => {
      const req = db.transaction(storeName).objectStore(storeName).get(key);
      req.onsuccess = () => resolve(req.result ?? null);
      req.onerror = () => reject(req.error);
    });
  }

  function kvKeys(storeName) {
    return new Promise((resolve, reject) => {
      const req = db.transaction(storeName).objectStore(storeName).getAllKeys();
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  }

  return {
    // sheep
    addSheep: (rec) => addIfAbsent('sheep', rec),
    allSheep: () => getAll('sheep'),
    // batches — keyPath is `key`, set by net to batchKey(b)
    addBatch: (rec) => addIfAbsent('batches', rec),
    allBatches: () => getAll('batches'),
    batchesForSheep: async (sheepId) =>
      (await getAll('batches')).filter((b) => b.sheepId === sheepId),
    // fraud
    addFraud: (rec) => addIfAbsent('fraud', rec),
    allFraud: () => getAll('fraud'),
    // verified render cache: `${sheepId}:${frame}` -> { buf:ArrayBuffer, keys:[batchKey] }
    // keys are EXACTLY the tiles summed into buf, so a served render and its
    // claimed batchKeys stay consistent (the gate checks total_count == Σ counts).
    putRender: (sheepId, frame, buf, keys) =>
      kvPut('renders', `${sheepId}:${frame}`, { buf, keys }),
    getRender: (sheepId, frame) => kvGet('renders', `${sheepId}:${frame}`),
    allRenderKeys: () => kvKeys('renders'),
  };
}
