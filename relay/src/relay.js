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
import { TOPIC, DISCOVERY_TOPIC } from './common.js';

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
    pubsub: gossipsub({ allowPublishToZeroTopicPeers: true }),
  },
});

// Subscribe to the data topic: the relay is a gossip backbone/fallback so
// hard-NAT peers (which can reach it over wss but not WebRTC anyone) stay in
// the swarm. Direct browser-to-browser links offload the rest. (See header.)
node.services.pubsub.subscribe(TOPIC);

console.log('relay peer id:', node.peerId.toString());
for (const addr of node.getMultiaddrs()) console.log('listening:', addr.toString());
console.log(`browsers should bootstrap to: <public-addr>/p2p/${node.peerId.toString()}`);
