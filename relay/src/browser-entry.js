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
import { bootstrap } from '@libp2p/bootstrap';
import { TOPIC, enc, dec } from './common.js';

/**
 * @param relays  array of relay multiaddrs incl. /p2p/<peer-id>, e.g.
 *                ['/dns4/relay.example.com/tcp/443/wss/p2p/12D3...']
 * @returns {send, onMessage, peerCount, node}
 */
export async function createLibp2pTransport({ relays }) {
  const node = await createLibp2p({
    // Listen via WebRTC, signaled through a relay reservation: this is what
    // lets two browsers talk directly after the relay introduces them.
    addresses: { listen: ['/webrtc'] },
    transports: [
      webSockets(),
      webRTC(),
      circuitRelayTransport(),
    ],
    connectionEncrypters: [noise()],
    streamMuxers: [yamux()],
    peerDiscovery: relays.length ? [bootstrap({ list: relays })] : [],
    services: {
      identify: identify(),
      pubsub: gossipsub({ allowPublishToZeroTopicPeers: true }),
    },
  });

  node.services.pubsub.subscribe(TOPIC);

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
