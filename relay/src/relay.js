// The swarm's one piece of infrastructure: a libp2p node that
//   1. accepts WebSocket connections from browsers (they can't accept inbound),
//   2. acts as a circuit-relay-v2 server so browsers can WebRTC to each other,
//   3. joins the gossipsub topic so messages flow even before any direct
//      browser-to-browser link exists.
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
import { TOPIC } from './common.js';

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
  services: {
    identify: identify(),
    relay: circuitRelayServer({
      reservations: { maxReservations: 256 },
    }),
    pubsub: gossipsub({ allowPublishToZeroTopicPeers: true }),
  },
});

node.services.pubsub.subscribe(TOPIC);

console.log('relay peer id:', node.peerId.toString());
for (const addr of node.getMultiaddrs()) console.log('listening:', addr.toString());
console.log(`browsers should bootstrap to: <public-addr>/p2p/${node.peerId.toString()}`);
