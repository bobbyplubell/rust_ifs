// Shared pieces of the libp2p setup. One gossipsub topic carries all wire
// messages (net.js multiplexes by msg.kind, same as the BroadcastChannel
// transport).

export const TOPIC = 'sheep/v2';

export const enc = (msg) => new TextEncoder().encode(JSON.stringify(msg));
export const dec = (bytes) => JSON.parse(new TextDecoder().decode(bytes));
