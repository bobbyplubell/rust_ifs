// hash.js — tiny WebCrypto helpers (SHA-256 everywhere, lowercase hex).

export function utf8(s) {
  return new TextEncoder().encode(s);
}

export async function sha256Hex(bytes) {
  const digest = await crypto.subtle.digest('SHA-256', bytes);
  return hex(new Uint8Array(digest));
}

export function hex(bytes) {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

export function unhex(s) {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(i * 2, i * 2 + 2), 16);
  return out;
}
