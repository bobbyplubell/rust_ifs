// The swarm's one piece of infrastructure: a libp2p node that
//   1. accepts WebSocket connections from browsers (they can't accept inbound),
//   2. acts as a circuit-relay-v2 server so browsers can WebRTC to each other,
//   3. introduces peers (forwards the discovery topic) so they form DIRECT
//      WebRTC links — most sheep data flows browser-to-browser over those,
//   4. ALSO subscribes to the data topic, so it's a gossip BACKBONE/FALLBACK:
//      a peer that can't establish WebRTC to anyone (hard NAT / cellular CGNAT,
//      which can still reach the relay over wss) stays in the swarm by gossiping
//      with the relay. Direct links offload the well-connected majority, so the
//      relay carries the tail, not everything. (Hybrid: inclusivity over the
//      pure discovery-only design — needed for a public launch reaching phones.)
// It holds NO authority: it forwards signed facts it cannot forge, sees only
// ciphertext, and can withhold but never corrupt.
//
// Env: PORT (ws listen, default 4001), ANNOUNCE (comma-separated public
// multiaddrs to advertise, e.g. /dns4/relay.example.com/tcp/443/wss).

import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { dirname } from 'node:path';
import { createLibp2p } from 'libp2p';
import { generateKeyPair, privateKeyToProtobuf, privateKeyFromProtobuf } from '@libp2p/crypto/keys';
import { webSockets } from '@libp2p/websockets';
import { circuitRelayServer } from '@libp2p/circuit-relay-v2';
import { noise } from '@chainsafe/libp2p-noise';
import { yamux } from '@chainsafe/libp2p-yamux';
import { gossipsub } from '@chainsafe/libp2p-gossipsub';
import { identify } from '@libp2p/identify';
import { pubsubPeerDiscovery } from '@libp2p/pubsub-peer-discovery';
import { TOPIC, DISCOVERY_TOPIC, RELAY_TOPIC } from './common.js';

const port = process.env.PORT || 4001;
const announce = (process.env.ANNOUNCE || '').split(',').filter(Boolean);

// Persist the peer key: the relay's peer id is baked into every client's
// config.js, so it must survive restarts (compose mounts /app/keys).
const keyFile = process.env.KEY_FILE || 'keys/relay.key';
let privateKey;
try {
  privateKey = privateKeyFromProtobuf(new Uint8Array(await readFile(keyFile)));
  console.log('loaded peer key from', keyFile);
} catch {
  privateKey = await generateKeyPair('Ed25519');
  await mkdir(dirname(keyFile), { recursive: true });
  await writeFile(keyFile, privateKeyToProtobuf(privateKey));
  console.log('generated new peer key ->', keyFile);
}

const node = await createLibp2p({
  privateKey,
  addresses: {
    listen: [`/ip4/0.0.0.0/tcp/${port}/ws`],
    ...(announce.length ? { announce } : {}),
  },
  transports: [webSockets()],
  connectionEncrypters: [noise()],
  streamMuxers: [yamux()],
  // Detect + drop genuinely-dead browser links, but tolerate a momentarily-busy
  // browser (saturated render / throttled background tab). The default 5s ping
  // timeout false-positives on those and aborts a live link; raise the floor to
  // 30s so only real dead links (sleep / drop) get aborted (~30-60s), then the
  // dead peers stop crowding the mesh without evicting busy-but-alive ones.
  connectionMonitor: {
    pingInterval: 20_000,
    pingTimeout: { minTimeout: 30_000, maxTimeout: 60_000 },
    abortConnectionOnPingFailure: true,
  },
  // Forward the discovery topic only (listenOnly: don't advertise the relay
  // itself — its address is already baked into every client's config).
  peerDiscovery: [pubsubPeerDiscovery({ listenOnly: true, topics: [DISCOVERY_TOPIC] })],
  services: {
    identify: identify(),
    relay: circuitRelayServer({
      // A reservation is what makes a browser dialable for inbound WebRTC, so
      // this caps how many peers can be fully connectable at once. 4096 is a
      // ceiling for a launch crowd; it costs nothing until that many peers
      // actually connect (RAM on the relay host is the real limit by then).
      reservations: { maxReservations: 4096 },
    }),
    // High mesh degree: the relay is a BACKBONE and should mesh with everyone
    // it's connected to, so a live peer is never crowded out of the mesh (and
    // thus never starved of forwarded data). Defaults (D=6) are tuned for a
    // flat p2p swarm, not a hub.
    pubsub: gossipsub({
      allowPublishToZeroTopicPeers: true,
      D: 64, Dlo: 32, Dhi: 128, Dscore: 48, Dout: 16,
    }),
  },
});

// Subscribe to the data topic: the relay is a gossip backbone/fallback so
// hard-NAT peers (which can reach it over wss but not WebRTC anyone) stay in
// the swarm. Direct browser-to-browser links offload the rest. (See header.)
node.services.pubsub.subscribe(TOPIC);

// Trustless relay discovery: gossip our own (self-certifying) public multiaddr
// so browsers learn about community relays beyond their bootstrap list. Also
// subscribed, so we forward OTHER relays' ads across the swarm.
node.services.pubsub.subscribe(RELAY_TOPIC);
{
  const selfAddr = announce.length
    ? `${announce[0]}/p2p/${node.peerId.toString()}`
    : node.getMultiaddrs().map((a) => a.toString()).find((a) => a.includes('/p2p/'));
  const advertise = () => {
    if (!selfAddr) return;
    node.services.pubsub.publish(RELAY_TOPIC, new TextEncoder().encode(selfAddr)).catch(() => {});
  };
  setTimeout(advertise, 3_000);     // initial, once a browser is likely connected
  setInterval(advertise, 30_000);   // refresh
}

console.log('relay peer id:', node.peerId.toString());
for (const addr of node.getMultiaddrs()) console.log('listening:', addr.toString());
console.log(`browsers should bootstrap to: <public-addr>/p2p/${node.peerId.toString()}`);

// ---- Observability -------------------------------------------------------
// libp2p's own debug logs are off, so we had ZERO visibility into why peers
// drop. Log every connection open/close and a periodic health line so we can
// actually SEE churn (does a peer drop? does it leave the gossip mesh? does
// the subscriber set shrink?) instead of guessing and restarting.
const shortId = (p) => '…' + p.toString().slice(-8);
const health = () => {
  const conns = node.getConnections();
  const subs = node.services.pubsub.getSubscribers(TOPIC).length;
  let mesh = -1;
  try { mesh = node.services.pubsub.getMeshPeers(TOPIC).length; } catch { /* api shape */ }
  return `conns=${conns.length} subscribers=${subs} mesh=${mesh}`;
};
node.addEventListener('connection:open', (evt) => {
  console.log(`[+] open  ${shortId(evt.detail.remotePeer)}  ${health()}`);
});
const everSubbed = new Set(); // connection ids that have been a TOPIC subscriber
node.addEventListener('connection:close', (evt) => {
  everSubbed.delete(evt.detail.id);
  console.log(`[-] close ${shortId(evt.detail.remotePeer)}  ${health()}`);
});
setInterval(() => console.log(`[stat] ${health()}`), 20_000);

// ---- Self-heal: reap "connected but never subscribed" zombies ------------
// A flaky client link can open a connection that never finishes the gossipsub
// handshake — the link dies mid-subscribe — leaving it as a "connected but
// subscribers=0" zombie. The connection-monitor ping only reaps these after
// ~30-60s (a long floor we keep so busy/backgrounded tabs aren't false-evicted),
// and on a churny link they pile up faster than that, degrading the relay
// (subscribers=0, mesh=0) until a manual restart. But every REAL peer in this
// swarm subscribes to the data topic within ~1-2s of connecting, so we can reap
// zombies directly: sweep periodically and close any connection older than
// SUBSCRIBE_GRACE that has NEVER been a TOPIC subscriber. The "never" is key —
// a flaky link can subscribe then briefly drop the subscription, and we must
// NOT churn such an established connection over a transient blip (that would
// also disrupt healthy peers on a momentary gossipsub hiccup); those are left
// to the gentler connection-monitor ping (~30-60s). We only kill connections
// that came up and never participated at all. Keyed per-PEER for the live
// check, per-CONNECTION for "ever subscribed". Circuit-relay + WebRTC signaling
// ride a browser's existing subscribed connection, so they're never targeted.
// No-op on a healthy swarm.
const SUBSCRIBE_GRACE = 20_000;
setInterval(() => {
  let subs;
  try { subs = new Set(node.services.pubsub.getSubscribers(TOPIC).map((p) => p.toString())); }
  catch { return; }
  const now = Date.now();
  for (const c of node.getConnections()) {
    if (subs.has(c.remotePeer.toString())) { everSubbed.add(c.id); continue; } // healthy now
    const opened = c.timeline?.open ?? now;
    if (now - opened < SUBSCRIBE_GRACE) continue;        // still within grace
    if (everSubbed.has(c.id)) continue;                  // subscribed before — leave blips to the ping
    console.log(`[reap] ${shortId(c.remotePeer)} never subscribed in ${Math.round((now - opened) / 1000)}s — closing`);
    c.close().catch(() => {});
  }
}, 10_000);
