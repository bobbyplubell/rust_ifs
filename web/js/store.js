// store.js — the append-only fact sets (ARCHITECTURE.md "state is a set of
// signed, immutable facts"), persisted in IndexedDB. Grow-only: records are
// added if absent, never updated or removed. Namespaced per ?peer=N so dev
// tabs have independent state.

import { PEER_NS } from './identity.js';

export async function openStore() {
  const db = await new Promise((resolve, reject) => {
    const req = indexedDB.open(`sheep-store-v6-${PEER_NS}`, 1);
    req.onupgradeneeded = () => {
      req.result.createObjectStore('sheep', { keyPath: 'id' });
      req.result.createObjectStore('votes', { keyPath: 'key' });
      req.result.createObjectStore('fraud', { keyPath: 'key' });
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

  return {
    addSheep: (rec) => addIfAbsent('sheep', rec),
    addVote: (rec) => addIfAbsent('votes', rec),
    addFraud: (rec) => addIfAbsent('fraud', rec),
    allSheep: () => getAll('sheep'),
    allVotes: () => getAll('votes'),
    allFraud: () => getAll('fraud'),
  };
}
