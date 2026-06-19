//! Versioned libp2p protocol-ID constants (ARCHITECTURE v3 §10).
//!
//! Nodes advertise exactly what they speak; a mismatch is caught at connect,
//! never by garbled behavior. Gossip topics carry [`crate::Envelope`]s; req/resp
//! protocols are request→response.
//!
//! Note on the doc's list: §10 names the *topics* (births/flock, votes,
//! attestations, claims+heartbeats, progress/coverage, reputation/bans) and the
//! *req/resp* (piece upload, advisory work hand-out). The brief enumerated
//! `/sheep/id/1.0.0`, `/sheep/flock/1.0.0`, `/sheep/votes`, `/sheep/attest`,
//! `/sheep/claims`, `/sheep/progress`, `/sheep/rep`, `/sheep/piece`,
//! `/sheep/assign`. We pin all of those. Where the brief gave a bare
//! `/sheep/votes` (no semver) we keep it verbatim as requested, and ALSO expose
//! a `*_V1` `/sheep/.../1.0.0` form, because §10 mandates `/sheep/<x>/<semver>`
//! protocol IDs at the libp2p level. Use the `_V1` forms for actual libp2p
//! registration; the bare forms double as gossipsub topic names.

// ---- identity / capability advertisement + flock membership ----------------

/// `/sheep/id` — node identity + capability advertisement (§1.1).
pub const ID: &str = "/sheep/id/1.0.0";

/// `/sheep/flock` — births + flock membership gossip (§2.1, §2.3).
pub const FLOCK: &str = "/sheep/flock/1.0.0";

// ---- gossip topics (§10). Bare names double as gossipsub topic strings; the
// `*_V1` constants are the semver'd libp2p protocol IDs. -----------------------

/// `/sheep/votes` — vote gossip (§2.2 survival backing).
pub const VOTES: &str = "/sheep/votes";
/// Semver'd protocol ID for [`VOTES`].
pub const VOTES_V1: &str = "/sheep/votes/1.0.0";

/// `/sheep/attest` — audit attestations (§6 shared attestation log).
pub const ATTEST: &str = "/sheep/attest";
/// Semver'd protocol ID for [`ATTEST`].
pub const ATTEST_V1: &str = "/sheep/attest/1.0.0";

/// `/sheep/claims` — soft work claims + heartbeats (§4).
pub const CLAIMS: &str = "/sheep/claims";
/// Semver'd protocol ID for [`CLAIMS`].
pub const CLAIMS_V1: &str = "/sheep/claims/1.0.0";

/// `/sheep/progress` — coverage / `have` progress gossip (§4, §10).
pub const PROGRESS: &str = "/sheep/progress";
/// Semver'd protocol ID for [`PROGRESS`].
pub const PROGRESS_V1: &str = "/sheep/progress/1.0.0";

/// `/sheep/rep` — reputation deltas + bans (§6).
pub const REP: &str = "/sheep/rep";
/// Semver'd protocol ID for [`REP`].
pub const REP_V1: &str = "/sheep/rep/1.0.0";

// ---- req/resp (§10) ---------------------------------------------------------

/// `/sheep/piece` — histogram piece upload (peer → seed).
pub const PIECE: &str = "/sheep/piece/1.0.0";

/// `/sheep/assign` — advisory work hand-out (least-covered blocks + audit
/// assignments).
pub const ASSIGN: &str = "/sheep/assign/1.0.0";

/// `/sheep/flock-sync` — flock catch-up req/resp (§10 convergence). A freshly-
/// connected node requests a peer's full birth log (every accepted Mint/Breed +
/// Vote envelope); the responder returns them and the requester re-applies each
/// through `engine.apply` (re-verifying signature + genome derivation). Births +
/// votes are ONE-SHOT gossip (never re-emitted by the engine), so this is how a
/// LATE joiner — or a reconnecting peer — converges to the full current flock
/// rather than only the persistently-republished founding sheep. Trustless: the
/// responder can only forward legitimately-born sheep; a forged envelope fails
/// verification on the requester.
pub const FLOCK_SYNC: &str = "/sheep/flock-sync/1.0.0";

/// Every protocol ID a full node speaks, for connect-time advertisement.
pub const ALL: &[&str] = &[
    ID,
    FLOCK,
    VOTES_V1,
    ATTEST_V1,
    CLAIMS_V1,
    PROGRESS_V1,
    REP_V1,
    PIECE,
    ASSIGN,
    FLOCK_SYNC,
];
