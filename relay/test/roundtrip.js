// Round-trip test, runnable in plain Node (Docker): start a relay and two
// websocket-only clients, bootstrap both to the relay, publish on the topic
// from A, require receipt at B. Exercises everything except the
// browser-specific WebRTC leg.

import { createLibp2p } from 'libp2p';
import { webSockets } from '@libp2p/websockets';
import { circuitRelayServer } from '@libp2p/circuit-relay-v2';
import { noise } from '@chainsafe/libp2p-noise';
import { yamux } from '@chainsafe/libp2p-yamux';
import { gossipsub } from '@chainsafe/libp2p-gossipsub';
import { identify } from '@libp2p/identify';
import { bootstrap } from '@libp2p/bootstrap';
import { TOPIC, enc, dec } from '../src/common.js';

const base = () => ({
  transports: [webSockets()],
  connectionEncrypters: [noise()],
  streamMuxers: [yamux()],
  services: {
    identify: identify(),
    pubsub: gossipsub({ allowPublishToZeroTopicPeers: true }),
  },
});

const relay = await createLibp2p({
  ...base(),
  addresses: { listen: ['/ip4/127.0.0.1/tcp/0/ws'] },
  services: { ...base().services, relay: circuitRelayServer() },
});
relay.services.pubsub.subscribe(TOPIC);
const relayAddr = relay.getMultiaddrs()[0].toString();
console.log('relay at', relayAddr);

const mkClient = () =>
  createLibp2p({
    ...base(),
    peerDiscovery: [bootstrap({ list: [relayAddr] })],
  });

const a = await mkClient();
const b = await mkClient();
a.services.pubsub.subscribe(TOPIC);
b.services.pubsub.subscribe(TOPIC);

const got = new Promise((resolve) => {
  b.services.pubsub.addEventListener('message', (evt) => {
    if (evt.detail.topic === TOPIC) resolve(dec(evt.detail.data));
  });
});

// Publish with retry until the mesh forms.
const payload = { kind: 'test', n: 42 };
const tick = setInterval(() => {
  a.services.pubsub.publish(TOPIC, enc(payload)).catch((e) => {
    if (!tick.logged) { console.log('publish error:', e.message); tick.logged = true; }
  });
}, 250);

const diag = setInterval(() => {
  console.log(
    `a conns=${a.getConnections().length} subs=${a.services.pubsub.getSubscribers(TOPIC).length}`,
    `b conns=${b.getConnections().length} subs=${b.services.pubsub.getSubscribers(TOPIC).length}`,
    `relay conns=${relay.getConnections().length}`,
  );
}, 2000);

const result = await Promise.race([
  got,
  new Promise((_, rej) => setTimeout(() => rej(new Error('timeout: no message after 20s')), 20_000)),
]);
clearInterval(tick);

if (result.n !== 42) throw new Error('payload mismatch: ' + JSON.stringify(result));
console.log('ROUNDTRIP OK:', JSON.stringify(result));
await Promise.all([a.stop(), b.stop(), relay.stop()]);
process.exit(0);
