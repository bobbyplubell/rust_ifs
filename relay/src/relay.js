// The swarm's one piece of infrastructure: a libp2p node that
//   1. accepts WebSocket connections from browsers (they can't accept inbound),
//   2. acts as a circuit-relay-v2 server so browsers can WebRTC to each other,
//   3. runs gossipsub but does NOT subscribe to the data topic. It still relays
//      SUBSCRIBE control messages (so peers discover each other) and the circuit
//      signaling that lets them form DIRECT WebRTC links — but it is NOT a data
//      hop. Sheep data gossips browser-to-browser over WebRTC, so the relay
//      never becomes a bandwidth bottleneck as the swarm grows.
//      Tradeoff: a peer that can't establish WebRTC to anyone is isolated —
//      there's no data fallback through the relay. For a public deployment
//      behind hard NATs, add a TURN server rather than re-subscribing the relay.
// It holds NO authority: it forwards signed facts it cannot forge.
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
import { DISCOVERY_TOPIC } from './common.js';

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
      reservations: { maxReservations: 256 },
    }),
    pubsub: gossipsub({ allowPublishToZeroTopicPeers: true }),
  },
});

// NOTE: intentionally NOT subscribed to TOPIC — discovery/signaling only, see
// the header. node.services.pubsub.subscribe(TOPIC) would make it a data hop.

console.log('relay peer id:', node.peerId.toString());
for (const addr of node.getMultiaddrs()) console.log('listening:', addr.toString());
console.log(`browsers should bootstrap to: <public-addr>/p2p/${node.peerId.toString()}`);
