// hash.js — tiny WebCrypto helpers (SHA-256 everywhere, lowercase hex).

export function utf8(s) {
  return new TextEncoder().encode(s);
}

export async function sha256Hex(bytes) {
  const digest = await crypto.subtle.digest('SHA-256', bytes);
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, '0')).join('');
}
