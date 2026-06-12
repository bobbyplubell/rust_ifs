// The swarm's one piece of infrastructure: a libp2p node that
//   1. accepts WebSocket connections from browsers (they can't accept inbound),
//   2. acts as a circuit-relay-v2 server so browsers can WebRTC to each other,
//   3. joins the gossipsub topic so messages flow even before any direct
//      browser-to-browser link exists.
// It holds NO authority: it forwards signed facts it cannot forge.
//
// Env: PORT (ws listen, default 4001), ANNOUNCE (comma-separated public
// multiaddrs to advertise, e.g. /dns4/relay.example.com/tcp/443/wss).

import { createLibp2p } from 'libp2p';
import { webSockets } from '@libp2p/websockets';
import { circuitRelayServer } from '@libp2p/circuit-relay-v2';
import { noise } from '@chainsafe/libp2p-noise';
import { yamux } from '@chainsafe/libp2p-yamux';
import { gossipsub } from '@chainsafe/libp2p-gossipsub';
import { identify } from '@libp2p/identify';
import { TOPIC } from './common.js';

const port = process.env.PORT || 4001;
const announce = (process.env.ANNOUNCE || '').split(',').filter(Boolean);

const node = await createLibp2p({
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
