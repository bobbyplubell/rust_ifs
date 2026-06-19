//! The signed envelope (§10) — the swarm-wide message wrapper.
//!
//! Every gossip / req-resp message is an [`Envelope`]: a small header
//! (`v`/`t`/`from`/`ts`) plus an arbitrary `body` (one of the [`crate::msg`]
//! types as a `serde_json::Value`) plus a detached Ed25519 `sig`.
//!
//! The signed bytes are [`Envelope::canonical`]: the envelope serialized as a
//! JSON object with the `sig` field stripped, keys sorted recursively, compact
//! — the exact same rule as `coordinator/src/auth.rs::canonical_message` and
//! `web/js/api.js::canonicalize`. A native peer and a browser sign byte-
//! identical bytes for the same message.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::canonical::canonicalize_value;

/// Current envelope/protocol version carried in [`Envelope::v`].
pub const ENVELOPE_VERSION: u16 = 1;

/// A signed protocol message.
///
/// - `v`: envelope version.
/// - `t`: message type tag (a `/sheep/...`-style short name or topic key).
/// - `from`: the sender's Ed25519 public key, lowercase hex (64 chars).
/// - `ts`: sender's timestamp (ms or micros, per the message type).
/// - `body`: the message payload (a [`crate::msg`] type, serialized).
/// - `sig`: Ed25519 signature over [`Envelope::canonical`], lowercase hex.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u16,
    pub t: String,
    /// Sender public key, lowercase hex.
    pub from: String,
    pub ts: u64,
    pub body: Value,
    /// Ed25519 signature over `canonical()`, lowercase hex. Empty until signed.
    #[serde(default)]
    pub sig: String,
}

impl Envelope {
    /// Build an unsigned envelope (`sig` empty). `from` is filled from the
    /// signing key by [`Envelope::sign`]; pass the key's pub hex if you want it
    /// set before signing (it must match the signer or [`Envelope::verify`]
    /// will fail).
    pub fn new(t: impl Into<String>, from: impl Into<String>, ts: u64, body: Value) -> Self {
        Envelope {
            v: ENVELOPE_VERSION,
            t: t.into(),
            from: from.into(),
            ts,
            body,
            sig: String::new(),
        }
    }

    /// The canonical signed bytes: this envelope as a JSON object with `sig`
    /// stripped, keys sorted recursively, compact. Byte-identical to
    /// `auth.rs::canonical_message` for the same field set.
    pub fn canonical(&self) -> String {
        // serde_json::to_value of a struct never fails (no maps with non-string
        // keys, no NaN unless a body deliberately holds one — bodies here are
        // serde structs without raw floats in the header).
        let value = serde_json::to_value(self).expect("envelope -> Value cannot fail");
        canonicalize_value(&value)
    }

    /// Sign this envelope in place: set `from` to the signing key's public hex
    /// and `sig` to the signature over [`Envelope::canonical`].
    pub fn sign(&mut self, key: &SigningKey) {
        self.from = hex_lower(&key.verifying_key().to_bytes());
        // sig is empty here, so it is stripped by canonical() either way; set
        // it after computing the bytes.
        self.sig = String::new();
        let msg = self.canonical();
        let sig: Signature = key.sign(msg.as_bytes());
        self.sig = hex_lower(&sig.to_bytes());
    }

    /// Verify `sig` against `from` over [`Envelope::canonical`]. Returns false
    /// on any decode/parse/verify failure.
    pub fn verify(&self) -> bool {
        let Some(pub_bytes) = decode_hex_32(&self.from) else {
            return false;
        };
        let Some(sig_bytes) = decode_hex_64(&self.sig) else {
            return false;
        };
        let Ok(key) = VerifyingKey::from_bytes(&pub_bytes) else {
            return false;
        };
        let sig = Signature::from_bytes(&sig_bytes);
        let msg = self.canonical();
        key.verify(msg.as_bytes(), &sig).is_ok()
    }
}

/// Lowercase hex (matches `flame_core::canonical::to_hex` / `hex::encode`).
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        let hi = nibble(bytes[i])?;
        let lo = nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn decode_hex_32(s: &str) -> Option<[u8; 32]> {
    decode_hex(s)?.try_into().ok()
}

fn decode_hex_64(s: &str) -> Option<[u8; 64]> {
    decode_hex(s)?.try_into().ok()
}
