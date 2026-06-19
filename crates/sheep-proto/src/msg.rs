//! Message body types (ARCHITECTURE v3 §2.1, §4, §6, §10).
//!
//! Each is the `body` of an [`crate::Envelope`]. They are pure serde structs:
//! no networking, no behavior. Field names are the wire keys; canonicalization
//! ([`crate::Envelope::canonical`]) sorts them, so declaration order here is
//! for readability only.
//!
//! Numeric care: `PieceUpload::count` is a **string**, not a number — confirmed
//! tile counts can exceed JS `Number.MAX_SAFE_INTEGER` (2^53), and the browser
//! face signs the same bytes a native peer does, so a large integer must
//! survive a JS round-trip losslessly. All other counters here stay well under
//! 2^53.

use serde::{Deserialize, Serialize};

use crate::identity::ResolutionTier;

// ---- births (§2.1) ----------------------------------------------------------

/// Mint a brand-new sheep: `genome = derive(hash(ts_micros ‖ minter_pub))`.
/// `resolution` is bound into the sheep identity (§2.1).
///
/// A mint is a **credit spend** (§3), so it carries a per-key sequence number
/// (§7): two different mints from one key at the same `seq` are equivocation
/// (a double-spend) and slash the signer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mint {
    pub ts_micros: u64,
    /// Minter public key, lowercase hex (the same key that signs the envelope).
    pub minter_pub: String,
    pub resolution: ResolutionTier,
    /// Per-key sequence number (§7): the double-spend defense for this spend.
    pub seq: u64,
}

/// Breed a sheep from two parents: `genome = derive(parent_a, parent_b, seed)`.
///
/// A breed is a **credit spend** (§3) and carries a per-key sequence number
/// (§7), same double-spend defense as [`Mint`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Breed {
    /// Parent A's sheep id, lowercase hex.
    pub parent_a: String,
    /// Parent B's sheep id, lowercase hex.
    pub parent_b: String,
    pub seed: u64,
    /// Breeder public key, lowercase hex.
    pub breeder_pub: String,
    pub resolution: ResolutionTier,
    /// Per-key sequence number (§7): the double-spend defense for this spend.
    pub seq: u64,
}

// ---- survival (§2.2) --------------------------------------------------------

/// Spend a credit to back a sheep's survival.
///
/// A vote is a **credit spend** (§3) and carries a per-key sequence number
/// (§7): two different votes from one key at the same `seq` are a double-spend
/// and slash the signer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vote {
    /// Target sheep id, lowercase hex.
    pub sheep_id: String,
    /// Per-key sequence number (§7): the double-spend defense for this spend.
    pub seq: u64,
}

// ---- audits / attestations (§6) ---------------------------------------------

/// "I re-rendered tile `(sheep_id, frame, idx, pass)` and got hash `hash`."
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attestation {
    /// Sheep id, lowercase hex.
    pub sheep_id: String,
    pub frame: u32,
    pub idx: u32,
    /// Sample-density pass (§4): the same `(frame, idx)` rendered again in a
    /// later pass is a DISTINCT unit of work that adds density, so a tile is
    /// identified by `(frame, idx, pass)`, not `(frame, idx)`.
    pub pass: u32,
    /// Content hash of the tile histogram, lowercase hex.
    pub hash: String,
}

// ---- work claims (§4) -------------------------------------------------------

/// A soft claim (hint, not a lock) on a work block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    /// Block id (a contiguous slice of a sheep's `(frame, pass, idx)`
    /// enumeration), lowercase hex or a structured string.
    pub block_id: String,
    /// Claim expiry (epoch ms); extended by [`Heartbeat`].
    pub expiry: u64,
    /// Claimant public key, lowercase hex.
    pub claimant: String,
    /// Per-key sequence number (§7): one live claim per key.
    pub seq: u64,
}

/// "Still on block K" — extends the claim's TTL (§4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Block id this heartbeat refreshes.
    pub block_id: String,
}

// ---- progress / coverage (§4, §10) ------------------------------------------

/// Coverage / `have` progress: "tile `(sheep_id, frame, idx)` is confirmed at
/// hash `hash`." (§4 coverage / §10 progress gossip.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    /// Sheep id, lowercase hex.
    pub sheep_id: String,
    pub frame: u32,
    pub idx: u32,
    /// Sample-density pass (§4): `(frame, idx)` in a later pass is fresh work
    /// that raises density, so coverage is keyed by `(frame, idx, pass)`.
    pub pass: u32,
    /// Content hash of the confirmed tile histogram, lowercase hex.
    pub hash: String,
}

/// Alias: §10 calls this `have`. Same shape as [`Coverage`].
pub type Have = Coverage;

// ---- reputation / bans (§6) -------------------------------------------------

/// A reputation delta for a peer, and/or a ban.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepDelta {
    /// Subject peer public key, lowercase hex.
    pub peer: String,
    /// Reputation delta (signed).
    pub rep: i64,
    /// Whether this marks the peer banned (e.g. on a proven equivocation).
    pub banned: bool,
}

// ---- piece upload (req/resp, peer → seed; §5) -------------------------------

/// Upload a rendered tile histogram (peer → seed). The heavy artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PieceUpload {
    /// Sheep id, lowercase hex.
    pub sheep_id: String,
    pub frame: u32,
    pub idx: u32,
    /// Sample-density pass (§4): a later pass over the same `(frame, idx)` is a
    /// distinct heavy artifact that adds density when accumulated.
    pub pass: u32,
    /// Content hash of the histogram, lowercase hex.
    pub hash: String,
    /// Total accumulated sample count for this tile. A **string** — it can
    /// exceed JS `Number.MAX_SAFE_INTEGER`, and the browser face signs the same
    /// bytes, so it must round-trip through JS losslessly.
    pub count: String,
    /// The histogram, deflate/zstd-compressed then base64-encoded.
    pub hist_b64: String,
}

// ---- advisory work hand-out (req/resp; §4, §10) -----------------------------

/// Request a bundle of advisory work (least-covered blocks + audit
/// assignments). An empty request asks for "whatever's least covered."
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssignReq {
    /// Optional sheep id to bias toward (advisory only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheep_id: Option<String>,
    /// How many blocks the requester wants (advisory; server caps it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub want: Option<u32>,
}

/// One advisory block to render: a `(sheep_id, frame, idx)` slice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssignBlock {
    /// Block id.
    pub block_id: String,
    /// Sheep id this block belongs to, lowercase hex.
    pub sheep_id: String,
    pub frame: u32,
    pub idx: u32,
}

/// Response to an [`AssignReq`]: advisory blocks + optional audit assignments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssignResp {
    pub blocks: Vec<AssignBlock>,
    /// Tiles this requester should audit (unpredictable, server-derived; §6).
    #[serde(default)]
    pub audits: Vec<Coverage>,
}
