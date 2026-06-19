// Deployment config (v3 — node HTTP API).
//
// The static client (GitHub Pages) talks to a sheep-node over plain HTTPS: it
// polls GET /api/flock for live state, GET /api/assign for work, and POSTs
// signed Envelopes to /api/msg (renders / votes / births). No WebSocket, no
// SSE, no libp2p in the browser (the node bridges writes into the swarm).
//
// ONE SWARM, MANY GATEWAYS ----------------------------------------------------
// There is exactly ONE swarm (one shared flock). The GATEWAYS below are NOT
// separate worlds — they are redundant GATEWAY NODES into that single swarm.
// relay1 and relay2 are cross-bootstrapped peers that converge on the same
// flock (via gossip + flock-sync), so whichever you talk to shows the same
// sheep, and your identity / credits / reputation / tiles are the same through
// any of them (same Pages origin → same localStorage key → same Ed25519 key).
// The list is just an ordered set of entry points for transparent failover.
//
// COORDINATOR is the resolved API base URL — everything in API.md hangs off
// `${COORDINATOR}/api/...`. It resolves, in order:
//   1. `?world=<url>` query param — a HIDDEN debug override (not surfaced in the
//      UI); written through to localStorage so it sticks across reloads,
//   2. localStorage (`sheep-world`) — a previously-set debug override,
//   3. localhost gateway when the page itself is served from localhost (dev),
//   4. the first GATEWAYS entry — the default primary.

// Ordered gateway endpoints into the one shared swarm. First is primary; the
// rest are mirrors for failover (same flock behind all of them). The localhost
// gateway is only used when the page is itself served from localhost (./dev.sh).
export const GATEWAYS = [
  'https://relay.proof-of-sheep.com',   // primary gateway — the one shared swarm
  'https://relay2.proof-of-sheep.com',  // mirror gateway — same swarm, same flock
];

// Dev gateway: auto-selected only when the page is served from localhost.
const LOCAL_GATEWAY = 'http://localhost:8080';

const WORLD_LS_KEY = 'sheep-world';

function normalize(url) {
  return String(url || '').trim().replace(/\/+$/, '');
}

function onLocalhost() {
  const h = location.hostname;
  return h === 'localhost' || h === '127.0.0.1' || h === '';
}

/**
 * Resolve the active gateway URL into the one shared swarm:
 *   ?world= (hidden debug override) → localStorage → localhost (dev) → primary.
 * A `?world=` value is written through to localStorage so it persists.
 */
function resolveGateway() {
  const param = normalize(new URLSearchParams(location.search).get('world'));
  if (param) {
    try { localStorage.setItem(WORLD_LS_KEY, param); } catch { /* private mode */ }
    return param;
  }
  let stored = null;
  try { stored = normalize(localStorage.getItem(WORLD_LS_KEY)); } catch { /* ignore */ }
  if (stored) return stored;
  // A page served from localhost (./dev.sh) defaults to the local gateway; the
  // deployed Pages client defaults to the primary gateway.
  if (onLocalhost()) return normalize(LOCAL_GATEWAY);
  return normalize(GATEWAYS[0]);
}

// The resolved API base URL for this page load.
export const COORDINATOR = resolveGateway();
