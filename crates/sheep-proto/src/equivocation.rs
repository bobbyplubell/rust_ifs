//! Per-key sequence numbers + equivocation proofs (ARCHITECTURE v3 §7).
//!
//! The one consistency primitive: every spend/claim carries an incrementing
//! per-key sequence number ([`Seq`]). Two *different* signed messages at the
//! same `(key, seq)` are cryptographic proof of cheating — a double-spend or a
//! double-claim — handled without consensus: optimistically apply, and when the
//! equivocation propagates everyone rejects both and slashes the key.
//!
//! [`Equivocation`] is that proof: a pair of [`crate::Envelope`]s. It is valid
//! ([`is_equivocation`]) iff both verify, share the same signer (`from`) and
//! the same `seq`, but differ in content.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::Envelope;

/// A per-key sequence wrapper: pairs a body with its sequence number (§7).
/// The `seq` is what the equivocation check keys on; carry it in any
/// spend/claim message (e.g. [`crate::msg::Claim::seq`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Seq<T> {
    pub seq: u64,
    pub body: T,
}

impl<T> Seq<T> {
    pub fn new(seq: u64, body: T) -> Self {
        Seq { seq, body }
    }
}

/// Extract the sequence number an envelope's body carries, if any.
///
/// Looks for a top-level integer `seq` field in the body (the convention for
/// spend/claim messages — see [`crate::msg::Claim`] and [`Seq`]).
pub fn body_seq(env: &Envelope) -> Option<u64> {
    env.body.get("seq").and_then(Value::as_u64)
}

/// A proof of equivocation: two signed messages from the same key at the same
/// sequence number with different content (§7). A slashing condition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Equivocation {
    pub a: Envelope,
    pub b: Envelope,
}

impl Equivocation {
    pub fn new(a: Envelope, b: Envelope) -> Self {
        Equivocation { a, b }
    }

    /// Whether this pair is a valid equivocation proof.
    pub fn is_valid(&self) -> bool {
        is_equivocation(&self.a, &self.b)
    }
}

/// True iff `a` and `b` are a valid equivocation: both signatures verify, same
/// signer (`from`), same `seq`, but different signed content.
///
/// "Different content" is decided by comparing the canonical signed bytes
/// ([`Envelope::canonical`]) — two messages with identical canonical bytes are
/// the same message (a duplicate), not an equivocation.
pub fn is_equivocation(a: &Envelope, b: &Envelope) -> bool {
    if !a.verify() || !b.verify() {
        return false;
    }
    if a.from != b.from {
        return false;
    }
    let (Some(sa), Some(sb)) = (body_seq(a), body_seq(b)) else {
        return false;
    };
    if sa != sb {
        return false;
    }
    // Same signer + same seq: an equivocation iff the signed content differs.
    a.canonical() != b.canonical()
}
