//! `sheep-proto` — the proof-of-sheep v3 wire-protocol contract (ARCHITECTURE
//! v3 §10 / §10.1). Pure types + serde, **no networking**.
//!
//! What lives here:
//! - [`Envelope`] + [`Envelope::canonical`]: the swarm-wide signed message,
//!   with canonicalization byte-identical to `coordinator/src/auth.rs`'s
//!   `canonical_message` (which is itself byte-matched to `web/js/api.js`'s
//!   `canonicalMessage`). This is the single signing/canonicalization rule
//!   shared by the libp2p face and the HTTP (browser) face.
//! - [`proto`]: the versioned libp2p protocol-ID constants (§10).
//! - [`msg`]: every gossip / req-resp message body type.
//! - [`derive`]: the deterministic `derive` rules ([`derive::derive_minted`],
//!   [`derive::derive_bred`]) that pin the valid genome space — same inputs
//!   reproduce the same genome (hence the same `sheep_id`) on any target.
//! - [`ResolutionTier`] + [`sheep_identity`]: resolution is bound into the
//!   sheep's identity (§2.1) so genome + tier together define the sheep.
//! - per-key sequence numbers + [`Equivocation`] (§7): the one consistency
//!   primitive — two signed messages at the same `(from, seq)` are proof of
//!   cheating.

pub mod canonical;
pub mod derive;
pub mod envelope;
pub mod equivocation;
pub mod identity;
pub mod msg;
pub mod proto;

pub use canonical::canonicalize_value;
pub use derive::{derive_bred, derive_minted, MINT_TRANSFORMS};
pub use envelope::{Envelope, ENVELOPE_VERSION};
pub use equivocation::{is_equivocation, Equivocation, Seq};
pub use identity::{sheep_identity, sheep_identity_hex, ResolutionTier};
