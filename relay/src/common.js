// Shared pieces of the libp2p setup. One gossipsub topic carries all wire
// messages (net.js multiplexes by msg.kind, same as the BroadcastChannel
// transport).

export const TOPIC = 'sheep/v2';

// A separate, tiny "discovery" topic. Peers broadcast their own (circuit-relay)
// multiaddrs on it every few seconds; the relay forwards ONLY this topic, so
// browsers learn each other's addresses and autodial into DIRECT WebRTC links.
// Bulk sheep data then flows over those direct links on TOPIC, never the relay.
export const DISCOVERY_TOPIC = 'sheep/disc/v1';

export const enc = (msg) => new TextEncoder().encode(JSON.stringify(msg));
export const dec = (bytes) => JSON.parse(new TextDecoder().decode(bytes));
