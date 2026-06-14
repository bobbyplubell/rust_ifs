// Browser side of the libp2p transport — bundled by esbuild into
// web/js/vendor/libp2p.js (see build.sh). Exposes the same two-method
// transport interface net.js expects, so it composes with BroadcastChannel.

import { createLibp2p } from 'libp2p';
import { webSockets } from '@libp2p/websockets';
import { webRTC } from '@libp2p/webrtc';
import { circuitRelayTransport } from '@libp2p/circuit-relay-v2';
import { noise } from '@chainsafe/libp2p-noise';
import { yamux } from '@chainsafe/libp2p-yamux';
import { gossipsub } from '@chainsafe/libp2p-gossipsub';
import { identify } from '@libp2p/identify';
import { ping } from '@libp2p/ping';
import { bootstrap } from '@libp2p/bootstrap';
import { pubsubPeerDiscovery } from '@libp2p/pubsub-peer-discovery';
import { multiaddr } from '@multiformats/multiaddr';
import { TOPIC, DISCOVERY_TOPIC, RELAY_TOPIC, enc, dec } from './common.js';

// Public STUN servers (used unless the caller passes its own). STUN is a cheap,
// stateless service — it just echoes back the address it sees — so leaning on
// free public ones is fine; data never flows through them.
const DEFAULT_STUN = [
  'stun:stun.l.google.com:19302',
  'stun:stun.cloudflare.com:3478',
];

/**
 * @param relays  array of relay multiaddrs incl. /p2p/<peer-id>, e.g.
 *                ['/dns4/relay.example.com/tcp/443/wss/p2p/12D3...']
 * @param stun    optional array of STUN urls (defaults to DEFAULT_STUN)
 * @returns {send, onMessage, peerCount, node}
 */
export async function createLibp2pTransport({ relays, stun }) {
  const stunUrls = (stun && stun.length) ? stun : DEFAULT_STUN;
  const node = await createLibp2p({
    // Listen via WebRTC, signaled through a relay reservation: this is what
    // lets two browsers talk directly after the relay introduces them.
    // Explicitly listen on each known relay's /p2p-circuit so we actively
    // RESERVE a slot (circuit-relay-v2 v3 won't auto-reserve on a merely
    // connected peer) — the reservation is what gives us a dialable
    // <relay>/p2p-circuit/webrtc address to advertise on the discovery topic.
    addresses: {
      listen: ['/webrtc', ...relays.map((r) => `${r}/p2p-circuit`)],
    },
    transports: [
      webSockets(),
      // STUN lets peers discover their public (server-reflexive) address so two
      // browsers behind different NATs can hole-punch a DIRECT connection. With
      // no iceServers a browser only has host candidates — fine on one LAN, but
      // it can't reach a peer across the internet. (No TURN: the ~10-20% of pairs
      // behind symmetric NAT won't connect — acceptable for now, add coturn for
      // full coverage.) Override via ?stun=<url,url>.
      webRTC({
        rtcConfiguration: {
          iceServers: [{ urls: stunUrls }],
        },
      }),
      circuitRelayTransport(),
    ],
    connectionEncrypters: [noise()],
    streamMuxers: [yamux()],
    // Detect a genuinely dead link (so the watchdog re-dials), but DON'T mistake
    // a busy or backgrounded tab for dead. The ping monitor's default 5s timeout
    // floor fires whenever the main thread stalls past 5s — which a saturated
    // render pump (44 tiles/s bursts) or a throttled background tab does
    // routinely — and with abort-on-failure it kills a LIVE connection (the
    // self-eviction churn: relay shows subscribers=0). Raise the floor to 30s: a
    // real dead link still aborts within ~30-60s and ensureRelays recovers it.
    connectionMonitor: {
      pingInterval: 20_000,
      pingTimeout: { minTimeout: 30_000, maxTimeout: 60_000 },
      abortConnectionOnPingFailure: true,
    },
    peerDiscovery: [
      ...(relays.length ? [bootstrap({ list: relays })] : []),
      // Broadcast our own circuit address on the discovery topic and dial peers
      // we hear about — this is what forms the direct browser-to-browser mesh.
      // 2s beacon: time-to-first-connection (and thus first sync) is gated by
      // this, so keep it brisk for small swarms.
      pubsubPeerDiscovery({ interval: 2_000, topics: [DISCOVERY_TOPIC] }),
    ],
    services: {
      identify: identify(),
      // Real @libp2p/ping: gives the connection-monitor a correct ping protocol
      // handler so it can verify peer liveness and DROP dead links (e.g. a stale
      // WebRTC peer that lingers in the gossip mesh and crowds out the relay).
      // Without it the mesh drains to zero over hours. The package handles the
      // protocol correctly (proper RTT + adaptive timeout) — do NOT hand-roll it.
      ping: ping(),
      pubsub: gossipsub({ allowPublishToZeroTopicPeers: true }),
    },
  });

  node.services.pubsub.subscribe(TOPIC);

  // Trustless relay discovery: collect gossiped relay multiaddrs and persist
  // them — the app reads localStorage.relays as extra relays on the next load,
  // so the relay set grows from a couple of bootstraps to the whole community.
  // Each ad is self-certifying (ends in /p2p/<id>), so a bad one just fails to
  // dial; no operator vetting needed (relays hold no authority).
  node.services.pubsub.subscribe(RELAY_TOPIC);
  node.services.pubsub.addEventListener('message', (evt) => {
    if (evt.detail.topic !== RELAY_TOPIC) return;
    try {
      const maddr = new TextDecoder().decode(evt.detail.data).trim();
      if (!/\/p2p\/[A-Za-z0-9]+$/.test(maddr)) return;
      const cur = (localStorage.getItem('relays') || '').split(',').map((s) => s.trim()).filter(Boolean);
      if (cur.includes(maddr)) return;
      localStorage.setItem('relays', [...new Set([...cur, maddr])].slice(-12).join(','));
    } catch { /* ignore malformed ad */ }
  });

  // Dial discovered peers to form the direct browser-to-browser mesh. The relay
  // forwards discovery beacons but NOT data, and libp2p's autodial won't dial
  // discovered peers on its own — so we dial them here. Capped at DIRECT_TARGET
  // direct links: gossipsub only needs a handful of mesh neighbours, and a
  // ~k-regular random graph stays connected, so data still reaches everyone
  // without every node connecting to every other (which wouldn't scale).
  const DIRECT_TARGET = 8;
  const relayIds = new Set(relays.map((r) => r.split('/p2p/').pop()).filter(Boolean));
  const directConns = () =>
    node.getConnections().filter((c) => !relayIds.has(c.remotePeer.toString()));
  const dialPeer = (id) => {
    if (relayIds.has(id.toString())) return;
    if (directConns().length >= DIRECT_TARGET) return;
    if (node.getConnections(id).length > 0) return; // already linked
    node.dial(id).catch(() => { /* unreachable peer: fine, others will relay gossip */ });
  };
  // Dial on first sight for a fast initial mesh...
  node.addEventListener('peer:discovery', (evt) => dialPeer(evt.detail.id));
  // ...and heal periodically: a dropped link or a missed/failed initial dial
  // would otherwise leave a peer stranded (the discovery event won't repeat for
  // an already-known peer). This re-dials known, reachable, unconnected peers
  // until we're back up to DIRECT_TARGET.
  setInterval(async () => {
    if (directConns().length >= DIRECT_TARGET) return;
    let peers = [];
    try { peers = await node.peerStore.all(); } catch { return; }
    for (const p of peers) {
      if (directConns().length >= DIRECT_TARGET) break;
      if (relayIds.has(p.id.toString())) continue;
      if (node.getConnections(p.id).length > 0) continue;
      // Only peers that have advertised a dialable webrtc address.
      if (!p.addresses.some((a) => a.multiaddr.toString().includes('webrtc'))) continue;
      dialPeer(p.id);
    }
  }, 6000 + Math.floor(Math.random() * 2000));

  // Reconnect watchdog: browsers drop the relay link on sleep / network change /
  // long idle, and libp2p's own recovery can stall (background timers throttle).
  // Re-dial any bootstrap relay we've lost — periodically and immediately when
  // the tab regains focus — so the swarm reconnects after a break.
  const ensureRelays = () => {
    for (const r of relays) {
      const id = r.split('/p2p/').pop();
      if (!id) continue;
      if (node.getConnections().some((c) => c.remotePeer.toString() === id)) continue;
      node.dial(multiaddr(r)).catch(() => { /* will retry next tick */ });
    }
  };
  setInterval(ensureRelays, 10_000);
  if (typeof document !== 'undefined') {
    document.addEventListener('visibilitychange', () => { if (!document.hidden) ensureRelays(); });
  }

  let handler = null;
  node.services.pubsub.addEventListener('message', (evt) => {
    if (evt.detail.topic !== TOPIC || !handler) return;
    try {
      handler(dec(evt.detail.data));
    } catch {
      /* malformed message: drop */
    }
  });

  return {
    node,
    peerId: node.peerId.toString(),
    send: (msg) => {
      node.services.pubsub.publish(TOPIC, enc(msg)).catch(() => {
        /* no peers yet: fine, anti-entropy will catch them up */
      });
    },
    onMessage: (fn) => {
      handler = fn;
    },
    peerCount: () => node.services.pubsub.getSubscribers(TOPIC).length,
  };
}
