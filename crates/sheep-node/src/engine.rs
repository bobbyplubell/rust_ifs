//! The pure proof-of-sheep v3 node engine (ARCHITECTURE v3 §2–§7).
//!
//! A deterministic state machine: **no networking, no async, no wall-clock**.
//! The clock is injected (`now_ms` passed in) so the whole thing is replayable
//! and testable. The transport layer (next agent) wraps this in a libp2p swarm:
//! it feeds inbound [`Envelope`]s to [`Engine::apply`] and ships the outbound
//! [`Envelope`]s [`Engine::tick`] returns.
//!
//! State convergence (§9): births, coverage, claims, credits and reputation are
//! all *log-derived* and conflict-free, so two engines fed the same envelope set
//! converge. The one adversarial corner — a key trying to double-claim — is
//! handled by per-key sequence equivocation (§7): two live claims from one key
//! at the same `seq` are rejected and the key is flagged.

use std::collections::{HashMap, HashSet};

use ed25519_dalek::SigningKey;
use flame_core::chunked::{hist_hash_hex, render_batch};
use flame_core::genome::Genome;
use flame_core::render::Accum;
use sha2::{Digest, Sha256};
use serde_json::Value;
use sheep_proto::derive::{derive_bred, derive_minted};
use sheep_proto::identity::{sheep_identity, ResolutionTier};
use sheep_proto::msg::{
    Attestation, Breed, Claim, Coverage, Heartbeat, Mint, PieceUpload, RepDelta, Vote,
};
use sheep_proto::{proto, Envelope};

use crate::block::{block_units, BlockId, Unit};
use crate::spec::{
    resolution_cost_mult, BREED_COST, BUNDLE_SIZE, IDXS_PER_FRAME, MINT_COST, SPP,
    TILES_PER_CREDIT, VOTE_COST,
};

// ---- tunables (the engine's knobs; §4.1, §4) -------------------------------

/// Claim TTL granted on claim/heartbeat (ms). A crashed peer stops
/// heartbeating and its claim lapses after this. 30s mirrors the conservative
/// ping floor used elsewhere in the project.
pub const CLAIM_TTL_MS: u64 = 30_000;

/// §4.1 coverage cap tolerance: a sheep may run up to
/// `min_flock_coverage + COVERAGE_TOLERANCE` confirmed tiles before claims for
/// it are rejected.
pub const COVERAGE_TOLERANCE: u64 = BUNDLE_SIZE as u64;

/// §4.1 floor: don't enforce the per-sheep cap until total confirmed coverage
/// across the flock exceeds this — early on every sheep is near zero and the
/// tolerance band covers them, so nobody is rejected on first contributions.
pub const COVERAGE_FLOOR: u64 = 4 * BUNDLE_SIZE as u64;

// ---- §6 trust / anti-fraud tunables ----------------------------------------

/// §6 reputation-graduated sampling constant: `sample_rate(rep) =
/// max(SAMPLE_FLOOR, NEW_PEER_RATE * TRUST_REP / (TRUST_REP + rep))`. At
/// `rep == TRUST_REP` the sample rate is half the new-peer rate; trust only ever
/// *reduces* the rate, never to zero (the floor). Kept modest so a peer must
/// accrue a meaningful body of confirmed work before its audit rate drops
/// appreciably — trust can't be cheaply cashed out into a free pass.
pub const TRUST_REP: u64 = 64;

/// §6 **new-peer audit rate** — the audit rate applied to a zero-reputation
/// submitter (the ceiling of [`sample_rate`]). Auditing is a deterministic
/// re-render, so every audited tile is rendered *twice* by the swarm; auditing
/// 100% of fresh work halves throughput for the dominant new-browser population.
///
/// We don't need 100% to deter fraud, because detection is **statistical and
/// retroactive**, not exhaustive: a bad tile is audited with probability
/// `NEW_PEER_RATE`, so submitting `k` bad tiles is caught with probability
/// `1 - (1 - NEW_PEER_RATE)^k` (≈97% by the 5th at 0.5), and a single catch
/// slashes + bans the key — nuking ALL its credit. Honeypots (always re-rendered
/// by the assigned auditor) independently catch lazy/lying auditors. So fraud
/// stays strongly −EV while redundant rendering is roughly halved. This is the
/// single dial for the speed/fraud-latency tradeoff; raise toward 1.0 to audit
/// fresh work harder, lower toward [`SAMPLE_FLOOR`] to render less redundantly.
pub const NEW_PEER_RATE: f64 = 0.5;

/// §6 sampling floor: the audit rate never drops below this no matter how
/// trusted a submitter is (so reputation is never a full free pass).
pub const SAMPLE_FLOOR: f64 = 0.05;

/// §6 **reputation-anchored confirmation thresholds (the Sybil fix).** A tile is
/// confirmed by a *reputation-weighted* rule rather than the old "any one
/// attestation confirms", so a flood of disposable zero-rep keys can never
/// self-confirm a peer's own (possibly bogus) tile — only EARNED standing
/// counts toward confirmation.
///
/// A single attestor at or above this reputation confirms a tile alone (the
/// "trusted-attestor" path). The local node is always treated as trusted
/// regardless of this value, so the gateway/seed confirmation path is preserved.
/// Set at half of [`TRUST_REP`]: a peer must have a meaningful body of
/// log-derived useful work behind it before its lone word is load-bearing.
pub const TRUSTED_ATTESTOR_REP: u64 = 32;

/// §6 quorum size for the fallback "quorum" confirmation path: when no single
/// trusted attestor exists, this many DISTINCT valid attestors are required…
pub const CONFIRM_QUORUM: usize = 2;

/// §6 …and the SUM of those distinct attestors' reputations must reach this.
/// Because a fresh disposable key contributes `0` to the sum, a Sybil holding
/// any number `K` of zero-rep keys can never satisfy the quorum — the sum stays
/// `0`. Only keys that have earned standing (by honest attestation / confirmed
/// work) move the sum, so confirmation is anchored to real, log-derived effort.
pub const CONFIRM_QUORUM_REP_SUM: u64 = 24;

/// §6 reputation-graduated sample rate for a submitter of standing `rep`.
/// `max(SAMPLE_FLOOR, NEW_PEER_RATE * TRUST_REP / (TRUST_REP + rep))`. Pure;
/// anyone recomputes it from a peer's log-derived rep, so two nodes agree on how
/// heavily to audit. A zero-rep submitter is sampled at [`NEW_PEER_RATE`] (not
/// 100% — see that constant for why partial auditing still deters fraud), and
/// the rate decays toward [`SAMPLE_FLOOR`] as confirmed work accrues.
pub fn sample_rate(rep: u64) -> f64 {
    let raw = NEW_PEER_RATE * (TRUST_REP as f64 / (TRUST_REP as f64 + rep as f64));
    if raw < SAMPLE_FLOOR {
        SAMPLE_FLOOR
    } else {
        raw
    }
}

/// §6 **unpredictable, verifiable audit assignment.** A pure function: is the
/// auditor identified by `auditor_pub` (lowercase-hex ed25519 key) assigned to
/// audit `tile = (sheep_hex, frame, idx, pass)` for a submitter of standing
/// `submitter_rep`, under `round_salt`?
///
/// `assigned = sha256(auditor_pub ‖ sheep ‖ frame ‖ idx ‖ pass ‖ round_salt)
///             < threshold(sample_rate(submitter_rep))`,
/// where `threshold(p)` is the largest 64-bit prefix value such that a uniform
/// hash lands below it with probability `p`.
///
/// **Why it's unpredictable + unselectable:** the assignment hash binds the
/// auditor's OWN pubkey, the tile, and a `round_salt` the auditor does not
/// control. An auditor cannot pick which tiles fall below its threshold (that
/// would require grinding its keypair, which also throws away its reputation),
/// so it cannot preferentially "audit" a confederate's tiles. **Why it's
/// verifiable:** the inputs are all public log facts, so any node recomputes the
/// exact same boolean — an auditor that attests a tile it was not assigned, or
/// fails to when assigned, is detectable.
pub fn assigned_to_audit(
    auditor_pub: &str,
    tile: (&str, u32, u32, u32),
    submitter_rep: u64,
    round_salt: &[u8],
) -> bool {
    let (sheep, frame, idx, pass) = tile;
    let mut hasher = Sha256::new();
    hasher.update(auditor_pub.as_bytes());
    hasher.update(b"|");
    hasher.update(sheep.as_bytes());
    hasher.update(frame.to_le_bytes());
    hasher.update(idx.to_le_bytes());
    hasher.update(pass.to_le_bytes());
    hasher.update(b"|");
    hasher.update(round_salt);
    let digest = hasher.finalize();
    // Take the leading 8 bytes as a big-endian u64 draw in [0, 2^64).
    let mut draw_bytes = [0u8; 8];
    draw_bytes.copy_from_slice(&digest[..8]);
    let draw = u64::from_be_bytes(draw_bytes);
    draw < assign_threshold(sample_rate(submitter_rep))
}

/// §6 **pure audit-assignment over a snapshot of unaudited tiles.** Given a
/// worker and a list of currently-unaudited tiles each tagged with its
/// submitter's reputation `(sheep, frame, idx, pass, submitter_rep)`, return the
/// subset this worker is assigned to audit under `round_salt`, applying the SAME
/// [`assigned_to_audit`] rule the in-hand [`Engine::assign_for`] uses. Sharing
/// this one implementation keeps the live (engine-in-hand) and cached (engine
/// checked out) assign paths byte-identical (DRY).
///
/// Output is sorted by `(sheep, frame, idx, pass)` so two nodes hand out the same
/// advisory set for the same inputs (the caller's snapshot map order is unstable).
pub fn audits_for(
    worker_pub: &str,
    unaudited: &[(String, u32, u32, u32, u64)],
    round_salt: &[u8],
) -> Vec<(String, u32, u32, u32)> {
    let mut out: Vec<(String, u32, u32, u32)> = unaudited
        .iter()
        .filter(|(sheep, frame, idx, pass, submitter_rep)| {
            assigned_to_audit(worker_pub, (sheep.as_str(), *frame, *idx, *pass), *submitter_rep, round_salt)
        })
        .map(|(sheep, frame, idx, pass, _)| (sheep.clone(), *frame, *idx, *pass))
        .collect();
    out.sort_by(|a, b| {
        (a.0.as_str(), a.1, a.2, a.3).cmp(&(b.0.as_str(), b.1, b.2, b.3))
    });
    out
}

/// The `[0, 2^64)` threshold a uniform draw must fall below to be "assigned"
/// with probability `p`. `p<=0 → 0` (never), `p>=1 → u64::MAX` (always).
fn assign_threshold(p: f64) -> u64 {
    if p <= 0.0 {
        return 0;
    }
    if p >= 1.0 {
        return u64::MAX;
    }
    // 2^64 * p, clamped into u64.
    let scaled = p * (u64::MAX as f64);
    if scaled >= u64::MAX as f64 {
        u64::MAX
    } else {
        scaled as u64
    }
}

// ---- learned flock entry ----------------------------------------------------

/// A sheep the node has learned about from a `Mint`/`Breed` birth event, with
/// its genome reconstructed + verified against the recorded derivation (§2.1).
#[derive(Debug, Clone)]
pub struct FlockEntry {
    pub genome: Genome,
    pub resolution: ResolutionTier,
    /// Birth time (envelope `ts`), ms.
    pub birth_ms: u64,
    /// §2.4 attribution: the key that signed the birth (minter or breeder),
    /// lowercase hex. Cryptographic proof of authorship, not a claim.
    pub creator: String,
    /// §2.4 lineage: for a bred sheep, `Some((parent_a_id, parent_b_id))`; for a
    /// minted sheep, `None`. Enables family trees / recursive credit.
    pub parents: Option<(String, String)>,
}

/// A live claim learned from gossiped `Claim` + `Heartbeat` (§4).
#[derive(Debug, Clone)]
pub struct LiveClaim {
    pub claimant: String,
    pub expiry_ms: u64,
    pub seq: u64,
}

/// What the engine is currently rendering (its single live claim).
#[derive(Debug, Clone)]
struct ActiveWork {
    block: BlockId,
    /// Units not yet rendered+emitted this claim.
    remaining: Vec<Unit>,
}

/// The pure node engine.
pub struct Engine {
    signing_key: SigningKey,
    /// This node's own public key, lowercase hex (matches `Envelope.from`).
    self_pub: String,

    /// Learned flock: sheep identity hex -> entry (§2.3).
    flock: HashMap<String, FlockEntry>,

    /// Confirmed coverage (§4.1): sheep hex -> set of confirmed
    /// `(frame, idx, pass)`. A tile is confirmed once it carries >=1 valid
    /// attestation. Keyed by `pass` too (§4): a later pass over the same
    /// `(frame, idx)` is a DISTINCT confirmed unit, so coverage (density) grows
    /// past one pass instead of being capped at it.
    confirmed: HashMap<String, HashSet<(u32, u32, u32)>>,
    /// Submitted-but-unaudited (§6 ingest-trust): sheep hex ->
    /// `(frame, idx, pass)` seen via `Coverage`/`Have` but not yet attested.
    unaudited: HashMap<String, HashSet<(u32, u32, u32)>>,
    /// Per tile, the set of distinct keys that have attested a matching hash —
    /// confirmation depth (§6). Key: (sheep hex, frame, idx, pass) -> {attestor}.
    attestors: HashMap<(String, u32, u32, u32), HashSet<String>>,

    /// §6 dispute tracking — every hash claimed for a tile, by either a
    /// submitter (`Coverage`) or an attestor (`Attestation`), and which keys
    /// claimed it. Key: tile -> (claimed hash -> {keys}). Two distinct hashes
    /// each with support is a corroborated mismatch → a dispute re-render.
    tile_hashes: HashMap<(String, u32, u32, u32), HashMap<String, HashSet<String>>>,
    /// §6 the submitter of a tile (the key whose `Coverage` first carried it),
    /// so a dispute can slash + retract the right party's contribution.
    submitter: HashMap<(String, u32, u32, u32), String>,
    /// §6 content hashes proven fraudulent by a dispute re-render — handed to the
    /// accumulator (`apply_disputes` → `retract_hash`) to bar the fraudster's work
    /// from re-entry (the dispute path has only hashes, not the tile bytes, so
    /// exact subtraction is deferred to the Phase-2 frame rebuild).
    retracted_hashes: HashSet<String>,
    /// §6 tiles already settled by a dispute, so we re-render each at most once
    /// (a dispute is the *only* re-render and it happens once per tile).
    disputed_tiles: HashSet<(String, u32, u32, u32)>,

    /// Live claims learned from gossip: block wire id -> claim (§4).
    claims: HashMap<String, LiveClaim>,

    /// Per-key highest seq seen, and the canonical bytes signed at that seq —
    /// for equivocation detection on inbound claims (§7).
    seq_seen: HashMap<String, HashMap<u64, String>>,
    /// §7 the SPEND seq namespace (separate from claims): per key, the canonical
    /// bytes signed at each spend seq, for double-spend equivocation on
    /// Vote/Mint/Breed. Two different spends at one `(key, seq)` → slash.
    spend_seq_seen: HashMap<String, HashMap<u64, String>>,
    /// Keys proven to have equivocated (a slashing condition; §7).
    slashed: HashSet<String>,

    /// §3 earned credits source, per key: confirmed tiles each key submitted
    /// (log-derived). `earned_tiles[self_pub]` is this node's own earned count.
    /// A key's earned credits = `earned_tiles[key] / TILES_PER_CREDIT`.
    earned_tiles: HashMap<String, u64>,
    /// §3/§7 committed spends, per key: total credits a key has spent via
    /// accepted Vote/Mint/Breed, summed over their costs. A key's balance is
    /// `earned − spent`; an `apply`'d spend that would overspend is rejected.
    spent_credits: HashMap<String, u64>,
    /// §2.2 per-sheep backing tally: lifetime votes received (log-derived). A
    /// `Vote{sheep}` adds `1` here; vitality = backing − decay(age).
    backing: HashMap<String, u64>,
    /// §2.2 peak backing ever seen per sheep — recorded into the Hall on death.
    peak_backing: HashMap<String, u64>,
    /// §2.2 Hall of Fame: enshrined sheep, preserved after death.
    hall: Vec<HallEntry>,
    /// §2.2 ids already enshrined, so a sheep enters the Hall at most once.
    enshrined: HashSet<String>,
    /// §2.2 the per-world decay personality (age-escalation rate knob).
    decay: DecayParams,
    /// §2.2 the Hall enshrinement thresholds.
    hall_threshold: HallThreshold,
    /// §3 per-world credit sinks (the base costs at the R384 tier; scaled by
    /// `resolution_cost_mult` per spend). Default to the spec.rs constants;
    /// overridable per world via [`Engine::set_costs`] / [`WorldConfig`].
    vote_cost: u64,
    mint_cost: u64,
    breed_cost: u64,
    /// Confirmed tiles credited to THIS node (own work that landed confirmed),
    /// for the simple log-derived credit count (§3).
    own_confirmed_tiles: u64,
    /// Tiles THIS node has rendered (own work), keyed (sheep hex, frame, idx,
    /// pass); used to credit the node when its submitted tile is later
    /// confirmed (§3).
    own_rendered: HashSet<(String, u32, u32, u32)>,

    /// Envelope canonical bytes already applied — gossip dedup / idempotency.
    seen_envelopes: HashSet<String>,

    /// This node's own outbound claim sequence counter (§4 one-at-a-time).
    next_seq: u64,
    /// The node's single in-flight piece of work, if any (§4 one claim at a
    /// time, complete-before-next).
    active: Option<ActiveWork>,
    /// Blocks this node has already rendered to completion (its own work), by
    /// wire id — so the next claim advances to fresh work instead of redoing a
    /// block it already finished.
    completed_blocks: HashSet<String>,

    /// Audit queue: tiles this node has been asked to re-render + attest (§6).
    audit_queue: Vec<Coverage>,

    /// §6 reputation per peer key (lowercase hex) — log-derived "proof of useful
    /// work" standing. A submitter's confirmed audited tiles raise its rep; an
    /// auditor's correct attestations raise its. Read by [`sample_rate`] so
    /// trusted peers are audited lightly and new/zero-rep peers heavily.
    reputation: HashMap<String, u64>,
    /// §6 **explicit always-trusted attestor keys** (lowercase hex), seeded from
    /// [`WorldConfig::trusted_keys`]. A key here confirms a tile alone in
    /// [`is_confirmed_by`] exactly as the local node does (subject to the same
    /// slashed/banned exclusion), so two seeds that list each other confirm both
    /// their tiles immediately at cold-start — no rep warm-up between seeds.
    /// Empty by default (non-breaking: an engine built without trusted keys
    /// behaves exactly as before).
    trusted_keys: HashSet<String>,
    /// §6 keys banned by a propagated/observed slash (a superset of `slashed`;
    /// `slashed` is keys WE proved cheated, `banned` also includes ones learned
    /// via `RepDelta`). Both reject the key's traffic + retract its work.
    banned: HashSet<String>,

    /// §6 the round salt for audit assignment — bytes the auditor does NOT
    /// control (so it can't grind which tiles it is assigned). Fixed per engine;
    /// shared swarm-wide so assignments are mutually verifiable.
    round_salt: Vec<u8>,

    /// §6 honeypots this node (in a seed/accumulator role) has planted: tile ->
    /// the known-true content hash. An incoming attestation on a planted tile is
    /// graded against this; a contradicting hash proves the auditor didn't
    /// actually render → slash.
    honeypots: HashMap<(String, u32, u32, u32), String>,
    /// §6 keys caught lying on a honeypot (lazy auditors), for test/inspection.
    honeypot_caught: HashSet<String>,

    /// §6 reputation deltas this node has decided to broadcast (drained by
    /// `tick` into signed `RepDelta` envelopes so the swarm converges on
    /// standing + bans).
    pending_rep: Vec<RepDelta>,

    /// §3 keys grandfathered past the credit check — the bootstrap genesis
    /// minter (a fixed protocol-constant key, no private user). Its births apply
    /// on a fresh node that has earned nothing yet. Convergent: every node seeds
    /// the same constant, so the exemption is not a per-node authority.
    credit_exempt: HashSet<String>,

    /// This node's own outbound spend sequence counter (§7), shared across
    /// Vote/Mint/Breed so every spend from this key carries a unique seq.
    next_spend_seq: u64,

    /// **Flock-sync catch-up log (§10 convergence).** Every accepted FLOCK
    /// (Mint/Breed birth) and VOTES (backing) envelope, in arrival order, kept as
    /// its original signed bytes. Births + votes are ONE-SHOT gossip (the engine
    /// never re-emits them), so a node that joins after a birth happened would
    /// otherwise never converge. The transport's `/sheep/flock-sync` req/resp
    /// replays this log to a freshly-connected peer, who re-applies each through
    /// `apply` (re-verifying signature + genome derivation), so it is trustless:
    /// the responder cannot inject a sheep it didn't see legitimately born — a
    /// forged or tampered envelope fails `verify()`/derivation on the receiver.
    /// Deduped by canonical bytes (the same gate `seen_envelopes` uses).
    birth_log: Vec<Envelope>,
}

/// The default audit round salt (§6). A fixed swarm-wide constant so every node
/// computes mutually-verifiable assignments; in a fuller deployment this would
/// rotate (e.g. seed-signed per epoch), but it must never be auditor-chosen.
pub const DEFAULT_ROUND_SALT: &[u8] = b"sheep/audit/round/v1";

// ---- §2.2 age-escalating backing decay -------------------------------------

/// §2.2 the per-world decay "personality" knob: how fast a sheep's required
/// backing escalates with age. Vitality = total_backing − decay(age), and decay
/// **escalates with age** (polynomial → exponential) so a sheep needs ever-more
/// votes to persist — scarcity lives in the lifespan, not the birth.
///
/// `decay(age) = base + linear·t + quad·t² + (exp_scale·(2^(t/half_life) − 1))`
/// where `t = age_ms / TIME_UNIT_MS` (age in whole "decay units"). The leading
/// polynomial term gives a gentle early ramp; the exponential term is the
/// whale-proof tail (§2.2) — no fixed earning rate meets it forever. Tune the
/// rate via these fields: Sandbox = steep (small `half_life`, big `quad`),
/// Gallery = gentle (large `half_life`, small coefficients).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecayParams {
    /// One "decay unit" of wall time, ms. Age is measured in these units so the
    /// coefficients below are human-scale, not per-millisecond.
    pub time_unit_ms: u64,
    /// Constant floor subtracted from backing from birth (a brand-new sheep
    /// with zero votes is already at vitality `-base`, so even mints need a vote
    /// to survive their first decay unit).
    pub base: f64,
    /// Linear-in-age coefficient (gentle early cost).
    pub linear: f64,
    /// Quadratic-in-age coefficient (the polynomial escalation).
    pub quad: f64,
    /// Exponential tail scale (the whale-proof term's amplitude).
    pub exp_scale: f64,
    /// Exponential tail half-life in decay units: every `half_life` units the
    /// exponential term roughly doubles. Smaller = steeper world.
    pub half_life: f64,
}

impl DecayParams {
    /// The default ("Gallery"-ish, gentle) decay personality. Chosen so that:
    /// a freshly-minted sheep with a handful of votes comfortably survives its
    /// youth, but by old age the cost has escalated past any plausible backing.
    /// One decay unit = 10 minutes. With the coefficients below, a sheep with a
    /// handful of votes lives the intended (Gallery-ish ~tens-of-minutes-to-hour)
    /// span before the escalating tail ages it out: at the DEFAULT params
    /// `decay(age)` reaches a backing of 4 at ~27 min, 8 at ~44 min, 16 at ~67 min
    /// (then keeps escalating — no fixed backing survives forever, §2.2). The
    /// per-ms-age math that previously used `time_unit_ms=1000` made `t` count
    /// SECONDS, so `quad·t²` demanded ~25 votes by 10s and killed a 4-vote
    /// founding sheep in ~3s — the bug this recalibration fixes.
    pub const DEFAULT: DecayParams = DecayParams {
        time_unit_ms: 600_000,
        base: 0.5,
        linear: 0.5,
        quad: 0.25,
        exp_scale: 1.0,
        half_life: 8.0,
    };

    /// The decay (required backing) at `age_ms`. Monotonically increasing in age
    /// and escalating (the marginal cost per unit of age grows), per §2.2. Pure.
    pub fn decay(&self, age_ms: u64) -> f64 {
        let t = age_ms as f64 / self.time_unit_ms as f64;
        let poly = self.base + self.linear * t + self.quad * t * t;
        let exp = if self.exp_scale != 0.0 && self.half_life > 0.0 {
            self.exp_scale * ((2f64).powf(t / self.half_life) - 1.0)
        } else {
            0.0
        };
        poly + exp
    }
}

// ---- §2.2 Hall of Fame ------------------------------------------------------

/// §2.2 enshrinement: a sheep that died is recorded in the Hall of Fame if it
/// lived notably long OR was deeply loved (peak backing past a threshold).
/// Mortality *with legacy* — preserved after death.
#[derive(Debug, Clone)]
pub struct HallEntry {
    pub sheep_id: String,
    pub genome: Genome,
    pub resolution: ResolutionTier,
    pub birth_ms: u64,
    /// Wall time (injected `now_ms`) at which the engine observed the death.
    pub death_ms: u64,
    /// Lifespan in ms (`death_ms − birth_ms`).
    pub lifespan_ms: u64,
    /// Peak total backing the sheep ever accumulated.
    pub peak_backing: u64,
}

/// §2.2 enshrinement thresholds: a dead sheep is enshrined if it lived at least
/// `min_lifespan_ms` OR ever reached `min_peak_backing` votes. Tunable per world.
#[derive(Debug, Clone, Copy)]
pub struct HallThreshold {
    pub min_lifespan_ms: u64,
    pub min_peak_backing: u64,
}

impl HallThreshold {
    /// Sensible defaults: "long-lived" = at least 20 decay units of the default
    /// `time_unit_ms` (20 × 600_000ms = 200 min); "deeply loved" = at least 16
    /// lifetime votes.
    pub const DEFAULT: HallThreshold = HallThreshold {
        min_lifespan_ms: 12_000_000,
        min_peak_backing: 16,
    };
}

// ---- §2.2/§3 per-world config (the "personality" knobs) --------------------

/// The per-world tunables a deployment injects at [`Engine`] construction
/// (ARCHITECTURE v3 §2.2 decay personality + §3 credit sinks), plus the seed's
/// initial-flock bootstrap size. Built from env in `main.rs`/`net.rs` (the
/// wall-clock-reading layer); the engine itself stays pure — config is data.
///
/// `Sandbox` = steep decay + cheap births (churny); `Gallery` = gentle decay +
/// dearer births (curated). Sane defaults match the engine's own defaults so an
/// unset env is a no-op rather than a surprise.
#[derive(Debug, Clone)]
pub struct WorldConfig {
    /// §2.2 age-escalating backing-decay personality.
    pub decay: DecayParams,
    /// §2.2 Hall-of-Fame enshrinement thresholds.
    pub hall: HallThreshold,
    /// §3 base vote cost (R384 tier; scaled per spend).
    pub vote_cost: u64,
    /// §3 base mint cost (R384 tier; scaled per spend).
    pub mint_cost: u64,
    /// §3 base breed cost (R384 tier; scaled per spend).
    pub breed_cost: u64,
    /// How many LIVE genesis sheep a *seed* mints into the world at boot (0 =
    /// none). Only the serving/accumulator node acts on this.
    pub bootstrap_flock: usize,
    /// RAM budget (MB = 1e6 bytes) for the accumulator's resident merged-frame
    /// working set (§5). The merged CRDT sums spill to `data_dir/accum/` beyond
    /// this; the small per-frame metadata stays resident regardless. Memory is
    /// thus bounded INDEPENDENT of flock/frame count. Only the serving/accumulator
    /// node holds an accumulator, so a plain worker ignores this.
    pub accum_ram_mb: usize,
    /// §6 **explicit mutual-trust set** (lowercase-hex ed25519 pubkeys). Keys
    /// here are treated as always-trusted attestors (alongside the local node),
    /// so their lone attestation confirms a tile regardless of earned rep. The
    /// fix for cold-start divergence between two seeds: configure each seed with
    /// the OTHER seed's pubkey here (`SHEEP_TRUSTED_KEYS`) and BOTH seeds' tiles
    /// confirm immediately — no rep warm-up between seeds, so their coverage
    /// stays in lockstep instead of one seed confirming and the other lagging.
    /// Empty by default (no extra trust beyond the local node), so unset env is
    /// a no-op. Slashed/banned keys are still excluded by `is_confirmed_by`.
    pub trusted_keys: HashSet<String>,
}

impl WorldConfig {
    /// The default world: the engine's own defaults (gentle/Gallery-ish decay,
    /// spec.rs credit sinks) and a 4-sheep bootstrap flock, with an EMPTY
    /// `trusted_keys` set (no extra trust beyond the local node). Provided as a
    /// function rather than an associated `const` because `trusted_keys`
    /// (`HashSet`) has no const empty constructor on this toolchain — call sites
    /// use `WorldConfig::default()` (and `..WorldConfig::default()` for the
    /// struct-update form).
    pub fn default_config() -> WorldConfig {
        WorldConfig {
            decay: DecayParams::DEFAULT,
            hall: HallThreshold::DEFAULT,
            vote_cost: VOTE_COST,
            mint_cost: MINT_COST,
            breed_cost: BREED_COST,
            bootstrap_flock: 4,
            // 128 MB resident merged-frame working set by default — comfortably
            // holds a handful of full-frame R384 frames while bounding total RAM.
            accum_ram_mb: 128,
            trusted_keys: HashSet::new(),
        }
    }
}

impl Default for WorldConfig {
    fn default() -> Self {
        WorldConfig::default_config()
    }
}

impl Engine {
    /// Construct an engine bound to the node's signing key.
    pub fn new(signing_key: SigningKey) -> Self {
        let self_pub = hex_lower(&signing_key.verifying_key().to_bytes());
        Engine {
            signing_key,
            self_pub,
            flock: HashMap::new(),
            confirmed: HashMap::new(),
            unaudited: HashMap::new(),
            attestors: HashMap::new(),
            claims: HashMap::new(),
            seq_seen: HashMap::new(),
            spend_seq_seen: HashMap::new(),
            slashed: HashSet::new(),
            own_confirmed_tiles: 0,
            own_rendered: HashSet::new(),
            seen_envelopes: HashSet::new(),
            next_seq: 0,
            active: None,
            completed_blocks: HashSet::new(),
            audit_queue: Vec::new(),
            tile_hashes: HashMap::new(),
            submitter: HashMap::new(),
            retracted_hashes: HashSet::new(),
            disputed_tiles: HashSet::new(),
            reputation: HashMap::new(),
            trusted_keys: HashSet::new(),
            banned: HashSet::new(),
            round_salt: DEFAULT_ROUND_SALT.to_vec(),
            honeypots: HashMap::new(),
            honeypot_caught: HashSet::new(),
            pending_rep: Vec::new(),
            earned_tiles: HashMap::new(),
            spent_credits: HashMap::new(),
            backing: HashMap::new(),
            peak_backing: HashMap::new(),
            hall: Vec::new(),
            enshrined: HashSet::new(),
            decay: DecayParams::DEFAULT,
            hall_threshold: HallThreshold::DEFAULT,
            vote_cost: VOTE_COST,
            mint_cost: MINT_COST,
            breed_cost: BREED_COST,
            credit_exempt: {
                let mut s = HashSet::new();
                // Grandfather the bootstrap genesis minter past the credit check.
                s.insert(crate::derive_minted_genesis::genesis_minter_pub());
                s
            },
            next_spend_seq: 0,
            birth_log: Vec::new(),
        }
    }

    /// Construct an engine and apply a per-world [`WorldConfig`] (decay, hall,
    /// credit sinks). The `bootstrap_flock` field is *not* consumed here — that
    /// is the transport's wall-clock-driven boot step ([`Engine::bootstrap_seed_flock`]).
    pub fn new_with_config(signing_key: SigningKey, cfg: &WorldConfig) -> Self {
        let mut e = Engine::new(signing_key);
        e.apply_world_config(cfg);
        e
    }

    /// Apply the §2.2/§3 personality knobs from a [`WorldConfig`] in place,
    /// including the §6 explicit `trusted_keys` mutual-trust set.
    pub fn apply_world_config(&mut self, cfg: &WorldConfig) {
        self.decay = cfg.decay;
        self.hall_threshold = cfg.hall;
        self.vote_cost = cfg.vote_cost;
        self.mint_cost = cfg.mint_cost;
        self.breed_cost = cfg.breed_cost;
        // §6 mutual trust: keys we will treat as always-trusted attestors (the
        // other seed's pubkey, typically) so both seeds confirm at cold-start.
        self.trusted_keys = cfg.trusted_keys.clone();
    }

    /// Add an explicit always-trusted attestor key (§6) at runtime / in tests —
    /// equivalent to listing it in [`WorldConfig::trusted_keys`]. Lowercase hex.
    pub fn add_trusted_key(&mut self, key: impl Into<String>) {
        self.trusted_keys.insert(key.into());
    }

    /// Override the §3 per-world credit sinks (base costs at the R384 tier).
    pub fn set_costs(&mut self, vote: u64, mint: u64, breed: u64) {
        self.vote_cost = vote;
        self.mint_cost = mint;
        self.breed_cost = breed;
    }

    /// The §3 credit sinks currently in force (base R384-tier costs).
    pub fn costs(&self) -> (u64, u64, u64) {
        (self.vote_cost, self.mint_cost, self.breed_cost)
    }

    /// Override the per-world decay personality (§2.2 age-escalation rate knob).
    pub fn set_decay_params(&mut self, params: DecayParams) {
        self.decay = params;
    }

    /// The decay personality currently in force (§2.2).
    pub fn decay_params(&self) -> DecayParams {
        self.decay
    }

    /// Override the Hall-of-Fame enshrinement thresholds (§2.2).
    pub fn set_hall_threshold(&mut self, t: HallThreshold) {
        self.hall_threshold = t;
    }

    /// Grandfather an extra key past the §3 credit check (e.g. a test fixture).
    pub fn exempt_credit(&mut self, key: impl Into<String>) {
        self.credit_exempt.insert(key.into());
    }

    /// Grant a key `tiles` earned render-credits directly (test / bootstrap
    /// support). Equivalent to the key having had `tiles` of its submitted tiles
    /// confirmed (§3), without re-running the render+attest loop — lets a node be
    /// funded for an initiated spend in a unit test without minutes of rendering.
    /// Production credit is always log-derived from real confirmations.
    pub fn grant_earned_tiles(&mut self, key: impl Into<String>, tiles: u64) {
        *self.earned_tiles.entry(key.into()).or_insert(0) += tiles;
    }

    /// Grant a key `rep` log-derived reputation directly (test / bootstrap
    /// support). Equivalent to the key having earned `rep` standing via honest
    /// attestation / confirmed work (§6), without re-running the render+attest
    /// loop — lets a test stand up a "trusted" or "established" attestor for the
    /// §6 confirmation rule without minutes of rendering. Banned keys never
    /// accrue standing (mirrors the internal `bump_rep`). Production rep is
    /// always log-derived from real attestations.
    pub fn grant_rep(&mut self, key: impl Into<String>, rep: u64) {
        self.bump_rep(&key.into(), rep);
    }

    /// Override the audit round salt (§6). The transport/seed sets this swarm-
    /// wide; tests use it to prove assignment changes with the salt. It must
    /// stay outside any single auditor's control to preserve unselectability.
    pub fn set_round_salt(&mut self, salt: impl Into<Vec<u8>>) {
        self.round_salt = salt.into();
    }

    /// The round salt currently in force (§6) — so the transport can compute the
    /// same `assigned_to_audit` the engine does when routing observed tiles.
    pub fn round_salt(&self) -> &[u8] {
        &self.round_salt
    }

    // ---- read accessors (the transport / UI layer calls these) ------------

    /// This node's public key, lowercase hex.
    pub fn self_pub(&self) -> &str {
        &self.self_pub
    }

    /// The learned flock (sheep identity hex -> entry).
    pub fn flock(&self) -> &HashMap<String, FlockEntry> {
        &self.flock
    }

    /// Confirmed coverage count for one sheep (§4.1; the cap's input).
    pub fn coverage(&self, sheep_hex: &str) -> u64 {
        self.confirmed.get(sheep_hex).map_or(0, |s| s.len() as u64)
    }

    /// Total confirmed coverage across the flock (the §4.1 floor's input).
    pub fn total_coverage(&self) -> u64 {
        self.confirmed.values().map(|s| s.len() as u64).sum()
    }

    /// Live (non-expired) claims, after pruning at `now_ms`.
    pub fn live_claims(&self) -> &HashMap<String, LiveClaim> {
        &self.claims
    }

    /// This node's spendable credits (§3): **earned − spent**. Earned =
    /// `own_confirmed_tiles / TILES_PER_CREDIT`; spent = the credits committed to
    /// accepted Vote/Mint/Breed by this key. Saturates at 0 (never goes
    /// negative — overspends are rejected before they commit).
    pub fn credits(&self) -> u64 {
        self.credits_of(&self.self_pub)
    }

    /// Any key's spendable credits (§3), log-derived: `earned − spent`. Pure +
    /// convergent — every node computes the same balance for a given key from
    /// the shared log. Saturating at 0.
    pub fn credits_of(&self, key: &str) -> u64 {
        let earned = self.earned_tiles.get(key).copied().unwrap_or(0) / TILES_PER_CREDIT;
        let spent = self.spent_credits.get(key).copied().unwrap_or(0);
        earned.saturating_sub(spent)
    }

    /// Credits this key has committed to accepted spends (§3 sinks).
    pub fn spent_of(&self, key: &str) -> u64 {
        self.spent_credits.get(key).copied().unwrap_or(0)
    }

    /// Raw count of this node's confirmed tiles (the earned-credit numerator).
    pub fn own_confirmed_tiles(&self) -> u64 {
        self.own_confirmed_tiles
    }

    /// A key's running total of **confirmed tiles** it submitted (§3, log-
    /// derived) — the numerator of that key's earned credits
    /// (`earned_tiles[key] / TILES_PER_CREDIT`). Convergent: every node computes
    /// the same per-key count from the shared log. `0` for a key we've seen earn
    /// nothing. The §10 write-face returns this so a browser contributor can show
    /// its accepted-tile total and its progress to the next credit.
    pub fn earned_tiles_for(&self, key: &str) -> u64 {
        self.earned_tiles.get(key).copied().unwrap_or(0)
    }

    // ---- §2.2 backing / vitality / live flock -----------------------------

    /// §2.2 a sheep's total backing — lifetime votes received (log-derived).
    pub fn backing(&self, sheep_hex: &str) -> u64 {
        self.backing.get(sheep_hex).copied().unwrap_or(0)
    }

    /// §2.2 a sheep's **vitality** at `now_ms`:
    /// `total_backing − decay(age)`, where `age = now_ms − birth_ms` and decay
    /// escalates with age. `> 0` = alive, `<= 0` = dormant/dead. Pure (reads the
    /// injected clock, never wall-clock). Unknown sheep → `None`.
    pub fn vitality(&self, sheep_hex: &str, now_ms: u64) -> Option<f64> {
        let entry = self.flock.get(sheep_hex)?;
        let age = now_ms.saturating_sub(entry.birth_ms);
        let backing = self.backing(sheep_hex) as f64;
        Some(backing - self.decay.decay(age))
    }

    /// §2.2/§2.3 is a sheep alive (vitality > 0) at `now_ms`?
    pub fn is_alive(&self, sheep_hex: &str, now_ms: u64) -> bool {
        self.vitality(sheep_hex, now_ms).is_some_and(|v| v > 0.0)
    }

    /// §2.3 the **live flock** at `now_ms`: every sheep whose vitality is still
    /// positive. Dormant/dead sheep are excluded here but remain in [`flock`]
    /// (history) and, if enshrined, in [`hall`]. Pure — driven by injected clock.
    pub fn live_flock(&self, now_ms: u64) -> HashMap<String, FlockEntry> {
        self.flock
            .iter()
            .filter(|(id, _)| self.is_alive(id, now_ms))
            .map(|(id, e)| (id.clone(), e.clone()))
            .collect()
    }

    /// §2.2 the Hall of Fame: enshrined (notably long-lived or deeply-loved)
    /// dead sheep, preserved after death. Driven by [`reap`] / [`tick`].
    pub fn hall(&self) -> &[HallEntry] {
        &self.hall
    }

    /// Keys proven to have equivocated (§7) or caught cheating an audit/honeypot
    /// (§6). The transport reads this to drop a slashed key's traffic.
    pub fn slashed(&self) -> &HashSet<String> {
        &self.slashed
    }

    /// All banned keys (§6): the union of locally-proven slashing and bans
    /// learned over the wire via `RepDelta`. A banned key's traffic is rejected
    /// and its merged work is retractable.
    pub fn banned(&self) -> &HashSet<String> {
        &self.banned
    }

    /// A peer's log-derived reputation (§6), 0 if unknown. Feeds [`sample_rate`].
    pub fn reputation_of(&self, peer: &str) -> u64 {
        self.reputation.get(peer).copied().unwrap_or(0)
    }

    /// Content hashes proven fraudulent by a dispute re-render (§6). The
    /// accumulator bars these from re-entry (`Accumulator::apply_disputes` →
    /// `retract_hash`); exact subtraction is deferred to the Phase-2 rebuild.
    pub fn retracted_hashes(&self) -> &HashSet<String> {
        &self.retracted_hashes
    }

    /// Keys caught lying on a planted honeypot (§6) — lazy auditors.
    pub fn honeypot_caught(&self) -> &HashSet<String> {
        &self.honeypot_caught
    }

    /// Is `auditor_pub` assigned (under this engine's round salt + the
    /// submitter's current rep) to audit `tile`? The pure §6 assignment, with
    /// this engine's salt + log-derived rep filled in — the transport calls this
    /// to decide which observed tiles to enqueue, and it is re-verifiable by any
    /// node with the same log.
    pub fn is_assigned(
        &self,
        auditor_pub: &str,
        tile: (&str, u32, u32, u32),
        submitter: &str,
    ) -> bool {
        assigned_to_audit(
            auditor_pub,
            tile,
            self.reputation_of(submitter),
            &self.round_salt,
        )
    }

    /// Push an audit assignment (the transport layer routes assigned observed
    /// tiles here; §6). Each will be re-rendered + attested on the next `tick`.
    pub fn enqueue_audit(&mut self, tile: Coverage) {
        self.audit_queue.push(tile);
    }

    // ---- §10 advisory work hand-out (the assign req/resp + REST face) -------

    /// §4/§4.1/§6 **advisory work hand-out** for `worker_pub` (lowercase hex):
    /// the same block-selection + coverage-cap + audit-assignment logic the
    /// libp2p `tick`/audit path uses, surfaced as a pure read so a browser
    /// contributor (or the §10 `/sheep/assign` req/resp) learns what to render
    /// next. Returns up to `want` least-covered, uncapped, currently-unclaimed
    /// blocks (skipping ones another peer holds, §4) plus any audit tiles
    /// `worker_pub` is assigned under [`assigned_to_audit`] (§6) among the
    /// tiles this node has observed but not yet confirmed.
    ///
    /// Advisory + read-only (§10): it never mutates engine state, and the
    /// neutral-claim protocol means a race just yields a harmless duplicate
    /// render that determinism dedups — so handing the same block to two workers
    /// is safe.
    pub fn assign_for(
        &self,
        worker_pub: &str,
        want: u32,
        now_ms: u64,
    ) -> (Vec<crate::block::BlockId>, Vec<Coverage>) {
        let blocks = self.pick_blocks_for(worker_pub, want.max(1) as usize, now_ms);

        // Audit assignments (§6): among observed-but-unconfirmed tiles, the ones
        // THIS worker is assigned to (unpredictable, unselectable, verifiable).
        // Delegate to the ONE canonical rule ([`audits_for`]) over the SAME cheap
        // snapshot the cache path in `net` feeds it (`audit_inputs`), so both
        // answers are byte-identical (DRY). `round_salt` is this engine's salt —
        // a public, log-derived swarm-wide value, NOT random.
        let audits = audits_for(worker_pub, &self.audit_inputs(), &self.round_salt)
            .into_iter()
            .map(|(sheep_id, frame, idx, pass)| Coverage {
                sheep_id,
                frame,
                idx,
                pass,
                hash: String::new(),
            })
            .collect();
        (blocks, audits)
    }

    /// §6 **cheap snapshot of the currently-unaudited tiles** as
    /// `(sheep, frame, idx, pass, submitter_rep)` — the inputs [`audits_for`]
    /// needs to compute a worker's audit assignments WITHOUT the live engine. The
    /// in-hand [`assign_for`] and the cache path in `net` both run `audits_for`
    /// over this, so the cached assign answer is byte-identical to the in-hand one.
    ///
    /// **Capped at [`Self::AUDIT_INPUTS_CAP`] tiles** to keep the cache cheap to
    /// clone every refresh: a node may observe many unconfirmed tiles, but the
    /// advisory hand-out only needs a representative working set (the missing tail
    /// is picked up on a later refresh as the head confirms + drains). Tiles are
    /// taken in a deterministic order (sorted by `(sheep, frame, idx, pass)`) so
    /// the cap selects the same subset on every node, preserving convergence.
    pub fn audit_inputs(&self) -> Vec<(String, u32, u32, u32, u64)> {
        let mut all: Vec<(String, u32, u32, u32, u64)> = Vec::new();
        for (sheep, set) in &self.unaudited {
            for &(frame, idx, pass) in set {
                let submitter_rep = self
                    .submitter
                    .get(&(sheep.clone(), frame, idx, pass))
                    .map(|s| self.reputation_of(s))
                    .unwrap_or(0);
                all.push((sheep.clone(), frame, idx, pass, submitter_rep));
            }
        }
        // Deterministic order so the cap below selects the same subset everywhere.
        all.sort_by(|a, b| (a.0.as_str(), a.1, a.2, a.3).cmp(&(b.0.as_str(), b.1, b.2, b.3)));
        all.truncate(Self::AUDIT_INPUTS_CAP);
        all
    }

    /// Cap on [`Self::audit_inputs`] — the most-relevant unaudited tiles cached
    /// for the §10 assign hand-out (keeps the per-refresh clone cheap).
    pub const AUDIT_INPUTS_CAP: usize = 256;

    /// Pick up to `want` least-covered, uncapped, unclaimed-by-others blocks for
    /// `worker_pub` — the multi-block generalization of [`pick_block`] used by
    /// [`assign_for`]. Pure (no state mutation); skips blocks another live peer
    /// holds (§4 soft claims) and ones this worker already holds.
    fn pick_blocks_for(&self, worker_pub: &str, want: usize, now_ms: u64) -> Vec<BlockId> {
        // Delegate to the ONE canonical selection rule ([`crate::block::pick_blocks`]),
        // passing the engine-derived inputs it needs (the live cache path in `net`
        // feeds it the SAME shapes, so both answers are byte-identical). Cheap: the
        // flock is a handful of sheep and the claim set is the live-claim count.
        crate::block::pick_blocks(
            &self.assign_inputs(),
            self.total_coverage(),
            &self.claim_inputs(),
            &self.submitted_inputs(),
            worker_pub,
            want,
            now_ms,
        )
    }

    /// Per-sheep **submitted** units (`confirmed ∪ unaudited`) as
    /// `(sheep_hex, {(frame, idx, pass)})` — the frontier-advance input to
    /// [`crate::block::pick_blocks`] (Fix 1). A block whose every unit is in this
    /// set has already been SUBMITTED for that sheep (even if not yet confirmed),
    /// so `pick_blocks` skips it and hands the worker the next fresh block instead
    /// of having it redundantly re-render a pending tile. Cloned cheaply (per-sheep
    /// sets of the flock's currently-pending tiles; the flock is small); the run
    /// loop caches this for the §10 assign render-window fallback so the cache path
    /// stays byte-identical to the in-hand one.
    pub fn submitted_inputs(&self) -> Vec<(String, HashSet<(u32, u32, u32)>)> {
        let mut out: HashMap<String, HashSet<(u32, u32, u32)>> = HashMap::new();
        for (sheep, set) in &self.confirmed {
            out.entry(sheep.clone()).or_default().extend(set.iter().copied());
        }
        for (sheep, set) in &self.unaudited {
            out.entry(sheep.clone()).or_default().extend(set.iter().copied());
        }
        out.into_iter().collect()
    }

    /// `(sheep_hex, confirmed_coverage)` for every known sheep — the flock-coverage
    /// input to [`crate::block::pick_blocks`]. Cloned cheaply (a handful of sheep);
    /// the run loop caches this for the §10 assign render-window fallback.
    pub fn assign_inputs(&self) -> Vec<(String, u64)> {
        self.flock
            .keys()
            .map(|s| (s.clone(), self.coverage(s)))
            .collect()
    }

    /// Live soft claims as `(block_wire_id, expiry_ms, claimant_pub)` — the claim
    /// input to [`crate::block::pick_blocks`]. Cloned cheaply (the live-claim set);
    /// the run loop caches this for the §10 assign render-window fallback.
    pub fn claim_inputs(&self) -> Vec<(String, u64, String)> {
        self.claims
            .iter()
            .map(|(wire, c)| (wire.clone(), c.expiry_ms, c.claimant.clone()))
            .collect()
    }

    /// §6.1 **gateway ingest-audit re-render check.** Re-render the tile a
    /// browser `Coverage`/`PieceUpload` claims and compare against its asserted
    /// content hash, returning `true` iff they match. This is the public node's
    /// "verify-before-vouch" gate for disposable browser identities (§6.1): the
    /// SAME deterministic re-render the dispute/honeypot paths use, exposed so
    /// the HTTP write-face can sample-audit ingress before injecting + co-signing.
    /// `None` (treated as "cannot verify") if the sheep's genome is unknown.
    pub fn verify_tile_hash(
        &self,
        sheep_id: &str,
        frame: u32,
        idx: u32,
        pass: u32,
        claimed_hash: &str,
    ) -> Option<bool> {
        let entry = self.flock.get(sheep_id)?;
        let identity = decode_hex_32(sheep_id)?;
        let accum = self.render_tile(&entry.genome, &identity, entry.resolution, frame, idx, pass);
        Some(hist_hash_hex(&accum) == claimed_hash)
    }

    // ---- §6 honeypots (seed / accumulator role) ----------------------------

    /// Plant a known-answer honeypot tile (§6). The node computes the true hash
    /// itself (rendering the tile) and remembers it; a later attestation on this
    /// tile is graded against the truth — a contradicting hash proves the
    /// auditor didn't really render. Returns the planted truth hash. `None` if
    /// the sheep is unknown.
    pub fn plant_honeypot(&mut self, sheep_id: &str, frame: u32, idx: u32, pass: u32) -> Option<String> {
        let entry = self.flock.get(sheep_id)?;
        let identity = decode_hex_32(sheep_id)?;
        let accum = self.render_tile(&entry.genome, &identity, entry.resolution, frame, idx, pass);
        let truth = hist_hash_hex(&accum);
        self.honeypots
            .insert((sheep_id.to_string(), frame, idx, pass), truth.clone());
        Some(truth)
    }

    /// Grade an attestation against any planted honeypot (§6). If the tile is a
    /// honeypot and the attested hash contradicts the planted truth, the attestor
    /// is slashed (proved it didn't render) and a banning `RepDelta` is queued.
    /// Returns `true` if a liar was caught.
    fn grade_honeypot(&mut self, att: &Attestation, attestor: &str) -> bool {
        let key = (att.sheep_id.clone(), att.frame, att.idx, att.pass);
        let Some(truth) = self.honeypots.get(&key) else {
            return false;
        };
        if &att.hash != truth {
            self.slash_key(attestor);
            self.honeypot_caught.insert(attestor.to_string());
            true
        } else {
            // Honest re-render of a honeypot earns the auditor standing.
            self.bump_rep(attestor, 1);
            false
        }
    }

    // ---- ingest (§3 apply) -------------------------------------------------

    /// Ingest an inbound protocol message: verify its signature (and, for
    /// births, its genome derivation), then update state convergently.
    /// Idempotent under replay (gossip dedup). Returns `true` if the message
    /// was accepted and changed state, `false` if rejected or a duplicate.
    pub fn apply(&mut self, env: &Envelope, now_ms: u64) -> bool {
        // Signature gate — everything in the swarm is signed (§10).
        if !env.verify() {
            return false;
        }
        // Slashed/banned keys are rejected wholesale (§6/§7): their submissions
        // are dropped and their merged work is retractable.
        if self.slashed.contains(&env.from) || self.banned.contains(&env.from) {
            return false;
        }
        // Idempotency / gossip dedup: identical signed bytes apply once.
        let canon = env.canonical();
        if self.seen_envelopes.contains(&canon) {
            return false;
        }

        let accepted = match env.t.as_str() {
            t if t == proto::FLOCK => self.apply_birth(env),
            t if t == proto::CLAIMS => self.apply_claim_or_heartbeat(env, now_ms),
            t if t == proto::PROGRESS => self.apply_coverage(env),
            t if t == proto::ATTEST => self.apply_attestation(env),
            t if t == proto::REP || t == proto::REP_V1 => self.apply_rep_delta(env),
            t if t == proto::VOTES || t == proto::VOTES_V1 => self.apply_vote(env),
            _ => false,
        };

        if accepted {
            self.seen_envelopes.insert(canon);
            // §10 convergence: stash accepted births (FLOCK) + backing (VOTES) so
            // the flock-sync catch-up can replay them to a late-joining peer.
            // These are the only envelope types the engine never re-emits, so a
            // node that connects after they were gossiped can't otherwise learn
            // them. (Spends/claims/coverage/attestations are re-emitted or derived
            // from re-rendering, so they self-heal; births/votes do not.)
            if env.t == proto::FLOCK
                || env.t == proto::VOTES
                || env.t == proto::VOTES_V1
            {
                self.birth_log.push(env.clone());
            }
        }
        accepted
    }

    /// §10 convergence — the **flock catch-up log**: every accepted birth
    /// (Mint/Breed) + backing (Vote) envelope, in arrival order, as original
    /// signed bytes. The transport replays this to a freshly-connected peer over
    /// the `/sheep/flock-sync` req/resp so a late joiner converges to the full
    /// current flock (births/votes are one-shot gossip the engine never re-emits).
    /// Trustless: the receiver re-applies each through [`Engine::apply`], which
    /// re-verifies the signature + re-derives the genome — the responder cannot
    /// inject a sheep, only forward legitimately-born ones it already accepted.
    pub fn birth_log(&self) -> &[Envelope] {
        &self.birth_log
    }

    /// A `Mint`/`Breed` birth (§2.1): reconstruct the genome from the recorded
    /// derivation and VERIFY it matches; reject on mismatch. The envelope `t`
    /// is `FLOCK`; the body discriminates mint vs. breed by its fields. A birth
    /// is a credit spend (§2.1/§3): the signer's balance is checked and the seq
    /// is policed for double-spend equivocation (§7).
    fn apply_birth(&mut self, env: &Envelope) -> bool {
        // Try Mint first (has `ts_micros`/`minter_pub`), else Breed.
        if let Ok(mint) = serde_json::from_value::<Mint>(env.body.clone()) {
            return self.apply_mint(env, &mint);
        }
        if let Ok(breed) = serde_json::from_value::<Breed>(env.body.clone()) {
            return self.apply_breed(env, &breed);
        }
        false
    }

    fn apply_mint(&mut self, env: &Envelope, mint: &Mint) -> bool {
        let Some(minter) = decode_hex_32(&mint.minter_pub) else {
            return false;
        };
        // §3 mint is a spend: cost scales with the chosen resolution (§2.1).
        let cost = self.mint_cost * resolution_cost_mult(mint.resolution);
        // Commit the spend (equivocation + overspend gate). Genesis/exempt keys
        // skip the balance check but still get seq-policed.
        if !self.commit_spend(env, mint.seq, cost) {
            return false;
        }
        // Re-derive the genome from the recorded (ts_micros, minter_pub) and
        // bind the resolution — anyone can verify, nobody trusts the minter.
        let genome = derive_minted(mint.ts_micros, &minter);
        let identity = sheep_identity(&genome, mint.resolution);
        let identity_hex = hex_lower(&identity);
        // birth_ms: the mint timestamp is micros; the flock clock is ms.
        let birth_ms = mint.ts_micros / 1000;
        self.flock.entry(identity_hex).or_insert(FlockEntry {
            genome,
            resolution: mint.resolution,
            birth_ms,
            creator: env.from.clone(),
            parents: None,
        });
        true
    }

    fn apply_breed(&mut self, env: &Envelope, breed: &Breed) -> bool {
        // Both parents must already be in the flock to re-derive the child.
        let (Some(a), Some(b)) = (
            self.flock.get(&breed.parent_a).map(|e| e.genome.clone()),
            self.flock.get(&breed.parent_b).map(|e| e.genome.clone()),
        ) else {
            return false;
        };
        // §3 breed is the costliest spend; cost scales with resolution (§2.1).
        let cost = self.breed_cost * resolution_cost_mult(breed.resolution);
        if !self.commit_spend(env, breed.seq, cost) {
            return false;
        }
        let genome = derive_bred(&a, &b, breed.seed);
        let identity = sheep_identity(&genome, breed.resolution);
        let identity_hex = hex_lower(&identity);
        self.flock.entry(identity_hex).or_insert(FlockEntry {
            genome,
            resolution: breed.resolution,
            // Breed bodies carry no timestamp; use the envelope ts (the breed
            // time) as birth_ms so decay/vitality has an age anchor.
            birth_ms: env.ts,
            creator: env.from.clone(),
            // §2.4 lineage: record both parents (recursively creditable).
            parents: Some((breed.parent_a.clone(), breed.parent_b.clone())),
        });
        true
    }

    /// A `Vote` (§2.2/§3): a credit spend that adds to a sheep's backing tally.
    /// Policed for double-spend equivocation + overspend (§7).
    fn apply_vote(&mut self, env: &Envelope) -> bool {
        let Ok(vote) = serde_json::from_value::<Vote>(env.body.clone()) else {
            return false;
        };
        // Vote only counts for a sheep we know (its genome verified via birth).
        if !self.flock.contains_key(&vote.sheep_id) {
            return false;
        }
        if !self.commit_spend(env, vote.seq, self.vote_cost) {
            return false;
        }
        let b = self.backing.entry(vote.sheep_id.clone()).or_insert(0);
        *b += 1;
        let peak = self.peak_backing.entry(vote.sheep_id.clone()).or_insert(0);
        if *b > *peak {
            *peak = *b;
        }
        true
    }

    /// §3/§7 **commit a credit spend.** Shared by Vote/Mint/Breed:
    /// 1. **Equivocation (§7):** a *different* signed spend from the same key at
    ///    the same `seq` is a double-spend → slash the signer, reject. An
    ///    identical re-send is a harmless duplicate (gossip dedup also catches
    ///    it). A *new* `(key, seq)` is recorded.
    /// 2. **Overspend (§3):** reject if the spend would push the signer below
    ///    zero balance (`earned − already_spent < cost`). Genesis/exempt keys
    ///    skip the balance check (bootstrap). On accept, the cost is committed to
    ///    `spent_credits`.
    /// Returns `true` if the spend is committed (caller proceeds), `false` if
    /// rejected (equivocation, overspend, or a stale dup).
    fn commit_spend(&mut self, env: &Envelope, seq: u64, cost: u64) -> bool {
        let canon = env.canonical();
        // 1) seq / equivocation in the SPEND namespace.
        let per_key = self.spend_seq_seen.entry(env.from.clone()).or_default();
        match per_key.get(&seq) {
            Some(prev) if prev != &canon => {
                // Two DIFFERENT spends at one (key, seq): self-evident double-
                // spend → slash (reuse the §6/§7 slashing path).
                let who = env.from.clone();
                self.slash_key(&who);
                return false;
            }
            Some(_) => {
                // Identical re-send already committed at this seq: a duplicate.
                // The outer `seen_envelopes` dedup means we shouldn't normally
                // reach here twice for byte-identical bytes, but be safe and do
                // not double-charge.
                return false;
            }
            None => {
                per_key.insert(seq, canon);
            }
        }

        // 2) overspend gate (§3). Skipped for grandfathered bootstrap keys.
        //
        // Tolerance for gossip lag (§3.1): enforce only when this node actually
        // has a *balance record* for the signer — i.e. it has seen the signer
        // earn credits (`earned_tiles`) or spend before, or it is our own key.
        // A totally-unknown key (no observed earnings yet) is applied
        // optimistically; its spends are re-evaluated as the log converges,
        // rather than rejected on a balance we simply haven't learned. This
        // matches §7's "optimistically apply" + §3.1's "smoothed/windowed
        // measure with tolerance, so nodes agree despite gossip lag." The HARD
        // local guarantee (a node can't spend more than IT earned) is enforced
        // by the `initiate_*` methods, which always have the full self balance.
        // "Observable" = we have seen this key EARN credits (an `earned_tiles`
        // record) or it is our own key. A key we have only ever seen *spend*
        // (no earnings in our view) stays optimistic — otherwise its first
        // optimistic spend would falsely mark it broke for the next one.
        let observable = env.from == self.self_pub || self.earned_tiles.contains_key(&env.from);
        if observable && !self.credit_exempt.contains(&env.from) {
            let balance = self.credits_of(&env.from);
            if balance < cost {
                // Reject the spend's EFFECT (no birth/backing). The seq is now
                // recorded, so a later equivocating spend at this seq is still
                // caught. Not a slashing condition — overspend is not provable
                // cheating (it may be gossip lag), just not applied.
                return false;
            }
        }
        // Commit the cost.
        *self.spent_credits.entry(env.from.clone()).or_insert(0) += cost;
        true
    }

    /// A `Claim` (§4) or `Heartbeat` (§4) on the claims topic. Both refresh a
    /// claim's TTL; a `Claim` additionally checks per-key equivocation (§7).
    fn apply_claim_or_heartbeat(&mut self, env: &Envelope, now_ms: u64) -> bool {
        if let Ok(claim) = serde_json::from_value::<Claim>(env.body.clone()) {
            return self.apply_claim(env, &claim, now_ms);
        }
        if let Ok(hb) = serde_json::from_value::<Heartbeat>(env.body.clone()) {
            return self.apply_heartbeat(&hb, now_ms);
        }
        false
    }

    fn apply_claim(&mut self, env: &Envelope, claim: &Claim, now_ms: u64) -> bool {
        // Equivocation (§7): a different signed message from the same key at
        // the same seq is proof of cheating — reject this one and slash.
        let canon = env.canonical();
        let per_key = self.seq_seen.entry(env.from.clone()).or_default();
        match per_key.get(&claim.seq) {
            Some(prev) if prev != &canon => {
                // Two live claims at one (key, seq): self-evident double-claim.
                let who = env.from.clone();
                self.slash_key(&who);
                return false;
            }
            Some(_) => {} // identical re-send — fine (dedup also catches it).
            None => {
                per_key.insert(claim.seq, canon);
            }
        }

        let Some(block) = BlockId::from_wire(&claim.block_id) else {
            return false;
        };
        self.prune_claims(now_ms);
        self.claims.insert(
            block.to_wire(),
            LiveClaim {
                claimant: claim.claimant.clone(),
                expiry_ms: claim.expiry.max(now_ms + CLAIM_TTL_MS),
                seq: claim.seq,
            },
        );
        true
    }

    fn apply_heartbeat(&mut self, hb: &Heartbeat, now_ms: u64) -> bool {
        self.prune_claims(now_ms);
        if let Some(c) = self.claims.get_mut(&hb.block_id) {
            c.expiry_ms = now_ms + CLAIM_TTL_MS;
            true
        } else {
            false
        }
    }

    /// A `Coverage`/`Have` (§4): a tile was submitted (ingest-trust, §6). It is
    /// tracked as unaudited until an attestation confirms it.
    fn apply_coverage(&mut self, env: &Envelope) -> bool {
        let Ok(cov) = serde_json::from_value::<Coverage>(env.body.clone()) else {
            return false;
        };
        if cov.frame >= crate::spec::N_FRAMES || cov.idx >= IDXS_PER_FRAME {
            return false;
        }
        // Only count progress on sheep we know (genome verified).
        if !self.flock.contains_key(&cov.sheep_id) {
            return false;
        }
        let tile = (cov.sheep_id.clone(), cov.frame, cov.idx, cov.pass);
        // §6 dispute tracking: record the submitter (first to gossip a Coverage
        // for this tile) and the hash they claimed, so a later conflicting
        // attestation triggers a dispute against the right party.
        self.submitter.entry(tile.clone()).or_insert(env.from.clone());
        if !cov.hash.is_empty() {
            self.tile_hashes
                .entry(tile.clone())
                .or_default()
                .entry(cov.hash.clone())
                .or_default()
                .insert(env.from.clone());
        }

        if self
            .confirmed
            .get(&cov.sheep_id)
            .is_some_and(|s| s.contains(&(cov.frame, cov.idx, cov.pass)))
        {
            return true; // already confirmed; nothing to add to unaudited.
        }
        self.unaudited
            .entry(cov.sheep_id.clone())
            .or_default()
            .insert((cov.frame, cov.idx, cov.pass));
        true
    }

    /// An `Attestation` (§6): "I re-rendered tile T and got hash H." Records the
    /// attestor; a tile with >=1 valid attestation becomes *confirmed*, moving
    /// it from unaudited into the coverage count. Credits the renderer (this
    /// node) when its own submitted tile is confirmed (§3).
    fn apply_attestation(&mut self, env: &Envelope) -> bool {
        let Ok(att) = serde_json::from_value::<Attestation>(env.body.clone()) else {
            return false;
        };
        if att.frame >= crate::spec::N_FRAMES || att.idx >= IDXS_PER_FRAME {
            return false;
        }
        if !self.flock.contains_key(&att.sheep_id) {
            return false;
        }
        let key = (att.sheep_id.clone(), att.frame, att.idx, att.pass);

        // §6 honeypot grading: if THIS node planted this tile, grade the
        // attestation against the known truth. A contradiction proves the
        // attestor never rendered → it is slashed and its attestation rejected.
        if self.grade_honeypot(&att, &env.from) {
            return true; // caught a liar; state changed (slash).
        }

        let set = self.attestors.entry(key.clone()).or_default();
        let newly = set.insert(env.from.clone());

        // §6 dispute tracking: record the attested hash under the attestor.
        if !att.hash.is_empty() {
            self.tile_hashes
                .entry(key.clone())
                .or_default()
                .entry(att.hash.clone())
                .or_default()
                .insert(env.from.clone());
        }

        // §6 reputation ("proof of useful work"): an attestor that re-rendered
        // and corroborates the standing consensus hash earns standing; the
        // submitter whose tile is being attested earns standing too. Both are
        // log-derived, so every node computes the same rep. **Done BEFORE the
        // confirmation test** so this attestation's own rep bump (and that of
        // any prior attestors) is reflected when we evaluate `is_confirmed_by`:
        // an attestor that crosses [`TRUSTED_ATTESTOR_REP`] on THIS attestation
        // confirms the tile on the same event. Rep-earning is UNCHANGED from the
        // old model — this is the bootstrap by which honest auditors climb past
        // the trust bar even before any of their attestations confirm anything.
        if !att.hash.is_empty() {
            self.bump_rep(&env.from, 1);
            if let Some(sub) = self.submitter.get(&key).cloned() {
                if sub != env.from {
                    self.bump_rep(&sub, 1);
                }
            }
        }

        // §6 **reputation-anchored confirmation (the Sybil fix).** Confirm the
        // tile iff its set of valid (non-slashed/banned) distinct attestors now
        // satisfies the trusted-attestor OR quorum-rep-sum rule — recomputed from
        // current log-derived rep on every attestation, so it is deterministic
        // across nodes given the same log. CONVERGENCE NOTE: a tile attested only
        // by browsers who become trusted LATER confirms on their NEXT attestation
        // / a re-attest, NOT retroactively on the rep-change event — we do not
        // rescan all tiles when a key's rep changes (no expensive global rescan).
        // Acceptable for v1: a re-render under the audit lottery re-attests it.
        let confirm_now = self.is_confirmed_by(&key);
        let already = self
            .confirmed
            .get(&att.sheep_id)
            .is_some_and(|s| s.contains(&(att.frame, att.idx, att.pass)));
        let was_new = confirm_now && !already && {
            self.confirmed
                .entry(att.sheep_id.clone())
                .or_default()
                .insert((att.frame, att.idx, att.pass))
        };
        if was_new {
            // No longer merely unaudited.
            if let Some(u) = self.unaudited.get_mut(&att.sheep_id) {
                u.remove(&(att.frame, att.idx, att.pass));
            }
            // Credit this node if it was the renderer of this tile (we track
            // our own rendered tiles via `own_rendered`, below).
            if self
                .own_rendered
                .contains(&(att.sheep_id.clone(), att.frame, att.idx, att.pass))
            {
                self.own_confirmed_tiles += 1;
            }
            // §3 earned-credit source, per key: credit the tile's SUBMITTER (the
            // key whose Coverage first carried it). Log-derived, so every node
            // computes the same per-key earned balance. Falls back to crediting
            // our own key when the submitter is unknown but we rendered it
            // (the two-peer/genesis path where Coverage and Attestation race).
            let tile = (att.sheep_id.clone(), att.frame, att.idx, att.pass);
            let credited = if let Some(sub) = self.submitter.get(&tile).cloned() {
                Some(sub)
            } else if self.own_rendered.contains(&tile) {
                Some(self.self_pub.clone())
            } else {
                None
            };
            if let Some(k) = credited {
                *self.earned_tiles.entry(k).or_insert(0) += 1;
            }
        }

        newly || was_new
    }

    /// §6 **assignment-anchored, optimistic confirmation predicate (the scaling
    /// fix).** Is the tile `key = (sheep, frame, idx, pass)` confirmed by its
    /// current set of **valid distinct attestors** — those recorded in
    /// `attestors[key]` that are NOT slashed/banned? A slashed/banned attestor's
    /// word is worth nothing here, so a key caught lying (honeypot/dispute) is
    /// dropped from the tally even if it previously attested.
    ///
    /// Let `S = submitter[key]` (the key whose `Coverage` first carried the tile;
    /// possibly unknown). The tile is confirmed iff there exists a valid attestor
    /// `A` such that **`A != S`** (a submitter can NEVER confirm its own tile —
    /// the one absolute invariant, enforced even on the assigned path), AND EITHER:
    /// - **(a) trusted path:** `A` is the LOCAL node (`self.self_pub`, always
    ///   trusted — preserves the gateway/seed confirmation path), OR is in the
    ///   explicit `trusted_keys` mutual-trust set (§6, e.g. the other seed's
    ///   pubkey), OR has `reputation_of(A) >= TRUSTED_ATTESTOR_REP`; OR
    /// - **(b) assigned path (the scaling fix):** `A` is *assigned* to audit this
    ///   tile under the §6 audit lottery — `is_assigned(A, tile, S)` is true
    ///   (`sha256(A ‖ tile ‖ round_salt) < threshold(sample_rate(rep(S)))`). When
    ///   `S` is unknown its rep is taken as 0 (so an assigned non-self attestor
    ///   still confirms). OR
    /// - **(c) quorum path (kept, harmless):** `>= CONFIRM_QUORUM` distinct valid
    ///   attestors (each `!= S`) whose summed `reputation_of(...)` reaches
    ///   `CONFIRM_QUORUM_REP_SUM`.
    ///
    /// **Why the posture changed (rep-gating → assignment).** The old rule confirmed
    /// only via earned standing (rep `>= 32` or a rep-sum quorum), so a fresh
    /// browser's audits never confirmed anything until it ground out rep 32 — every
    /// confirmation funnelled through the seeds and the seeds became the bottleneck.
    /// The assigned path makes confirmation **scale 1:1 with participation**: any
    /// honest auditor the verifiable lottery assigned to a tile confirms it
    /// immediately, so the swarm confirms its own work and hand-out no longer
    /// stalls on slow confirmation. Fraud is now caught **optimistically** —
    /// honeypots (`grade_honeypot`) and disputes (conflicting-hash → re-render →
    /// slash) detect-and-slash bad attestations after the fact — rather than being
    /// *prevented* by rep-gating up front. Trade-off (accepted): because a rep-0
    /// submitter is audited at rate ~1.0, almost any assigned key confirms its
    /// tile, so a 2-key Sybil (one submits, one attests) can briefly self-confirm;
    /// that window is closed by the honeypot/dispute slash machinery, which the
    /// `A != S` invariant + the unselectable lottery keep from being gamed cheaply.
    ///
    /// Pure read over log-derived state (rep + slash sets + submitter + round salt)
    /// → deterministic across nodes given the same log (`is_assigned` is itself a
    /// hash of public log facts).
    fn is_confirmed_by(&self, key: &(String, u32, u32, u32)) -> bool {
        let Some(attestors) = self.attestors.get(key) else {
            return false;
        };
        let submitter = self.submitter.get(key); // None if the submitter is unknown.
        let (sheep, frame, idx, pass) = (key.0.as_str(), key.1, key.2, key.3);

        let mut distinct = 0usize;
        let mut rep_sum: u64 = 0;
        for a in attestors {
            // Only valid attestors count (slashed/banned keys are discarded).
            if self.slashed.contains(a) || self.banned.contains(a) {
                continue;
            }
            // The submitter can NEVER confirm its own tile — the one invariant that
            // survives the optimistic posture (even an assigned submitter is barred).
            if submitter.is_some_and(|s| s == a) {
                continue;
            }
            // (a) trusted path: the local node is always trusted (the gateway/seed
            // confirmation path), as is any explicitly-configured mutual-trust key
            // (§6 — the other seed), else a peer at/above the rep bar.
            if a == &self.self_pub
                || self.trusted_keys.contains(a)
                || self.reputation_of(a) >= TRUSTED_ATTESTOR_REP
            {
                return true;
            }
            // (b) assigned path (the scaling fix): the §6 audit lottery picked `A`
            // to audit THIS tile for THIS submitter. `is_assigned` reads the
            // submitter's rep (0 when the submitter is unknown — `reputation_of`
            // returns 0 for an empty/unknown key), so an assigned non-self attestor
            // confirms a fresh (rep-0) submitter's tile immediately — 1:1 with
            // participation. Verifiable + unselectable, so it can't be gamed without
            // grinding the auditor's own key (which throws away its standing).
            let submitter_key = submitter.map(|s| s.as_str()).unwrap_or("");
            if self.is_assigned(a, (sheep, frame, idx, pass), submitter_key) {
                return true;
            }
            distinct += 1;
            rep_sum = rep_sum.saturating_add(self.reputation_of(a));
        }
        // (c) quorum path (kept, harmless): enough distinct EARNED-rep attestors
        // (none of them the submitter) by count + summed rep.
        distinct >= CONFIRM_QUORUM && rep_sum >= CONFIRM_QUORUM_REP_SUM
    }

    /// An inbound `RepDelta` (§6): consume reputation/ban news so the swarm
    /// converges on standing + bans. A `banned` delta bans the subject (and
    /// retracts its work downstream); a non-ban delta nudges its rep. Note we
    /// do NOT let arbitrary peers inflate rep unboundedly here — a positive
    /// `rep` is treated as advisory and clamped; bans are the load-bearing case.
    fn apply_rep_delta(&mut self, env: &Envelope) -> bool {
        let Ok(rd) = serde_json::from_value::<RepDelta>(env.body.clone()) else {
            return false;
        };
        if rd.peer.is_empty() {
            return false;
        }
        if rd.banned {
            // Converge on the ban: mark slashed+banned (so the key's traffic is
            // dropped and its merged work retracted), idempotently.
            if !self.banned.contains(&rd.peer) {
                self.banned.insert(rd.peer.clone());
                self.slashed.insert(rd.peer.clone());
                self.claims.retain(|_, c| c.claimant != rd.peer);
                self.reputation.remove(&rd.peer);
            }
            return true;
        }
        // Advisory rep nudge (bounded): apply a positive delta, ignore negative
        // here (negative standing news arrives as a ban). Never lifts a banned
        // key.
        if rd.rep > 0 && !self.banned.contains(&rd.peer) {
            self.bump_rep(&rd.peer, rd.rep as u64);
            return true;
        }
        false
    }

    // ---- §2.1/§3 initiated actions (mint / breed / vote) ------------------

    /// §3 **initiate a vote**: spend `VOTE_COST` to back `sheep_id`'s survival
    /// and build the signed [`Vote`] envelope. Returns `None` if the node can't
    /// afford it or doesn't know the sheep. Applies locally too (so the node's
    /// own balance + the sheep's backing reflect the spend immediately, exactly
    /// as the swarm will once it gossips). The caller publishes the envelope.
    pub fn initiate_vote(&mut self, sheep_id: &str, now_ms: u64) -> Option<Envelope> {
        if !self.flock.contains_key(sheep_id) {
            return None;
        }
        if self.credits() < self.vote_cost {
            return None;
        }
        let seq = self.next_spend_seq;
        self.next_spend_seq += 1;
        let vote = Vote { sheep_id: sheep_id.to_string(), seq };
        let env = self.sign(proto::VOTES, to_value(&vote), now_ms);
        // Apply our own spend locally (idempotent w.r.t. the gossip echo: the
        // canonical bytes are recorded in `seen_envelopes` so the round-trip is
        // a no-op).
        self.apply(&env, now_ms);
        Some(env)
    }

    /// §2.1/§3 **initiate a mint**: spend the mint cost (scaled by `tier`) and
    /// build a signed [`Mint`] for a brand-new sheep whose genome is
    /// `derive_minted(ts_micros, self)`. Genome-injection stays blocked — the
    /// genome is derived from the recorded inputs, verified on apply. Returns
    /// `None` if the node can't afford the mint. The new sheep's identity hex is
    /// returned alongside the envelope.
    pub fn initiate_mint(
        &mut self,
        ts_micros: u64,
        tier: ResolutionTier,
        now_ms: u64,
    ) -> Option<(Envelope, String)> {
        let cost = self.mint_cost * resolution_cost_mult(tier);
        if self.credits() < cost {
            return None;
        }
        let seq = self.next_spend_seq;
        self.next_spend_seq += 1;
        let mint = Mint {
            ts_micros,
            minter_pub: self.self_pub.clone(),
            resolution: tier,
            seq,
        };
        let env = self.sign(proto::FLOCK, to_value(&mint), now_ms);
        // The resulting identity (re-derived exactly as apply does).
        let minter = decode_hex_32(&self.self_pub)?;
        let genome = derive_minted(ts_micros, &minter);
        let id_hex = hex_lower(&sheep_identity(&genome, tier));
        self.apply(&env, now_ms);
        Some((env, id_hex))
    }

    /// §2.1/§3 **initiate a breed**: spend the (costliest) breed cost and build
    /// a signed [`Breed`] from two parents already in the flock. The child genome
    /// is `derive_bred(a, b, seed)` (verified on apply); lineage (breeder + both
    /// parents) is recorded. Returns `None` if unaffordable or a parent is
    /// unknown. Returns the envelope + the child's identity hex.
    pub fn initiate_breed(
        &mut self,
        parent_a: &str,
        parent_b: &str,
        seed: u64,
        tier: ResolutionTier,
        now_ms: u64,
    ) -> Option<(Envelope, String)> {
        let (Some(ga), Some(gb)) = (
            self.flock.get(parent_a).map(|e| e.genome.clone()),
            self.flock.get(parent_b).map(|e| e.genome.clone()),
        ) else {
            return None;
        };
        let cost = self.breed_cost * resolution_cost_mult(tier);
        if self.credits() < cost {
            return None;
        }
        let seq = self.next_spend_seq;
        self.next_spend_seq += 1;
        let breed = Breed {
            parent_a: parent_a.to_string(),
            parent_b: parent_b.to_string(),
            seed,
            breeder_pub: self.self_pub.clone(),
            resolution: tier,
            seq,
        };
        let env = self.sign(proto::FLOCK, to_value(&breed), now_ms);
        let genome = derive_bred(&ga, &gb, seed);
        let id_hex = hex_lower(&sheep_identity(&genome, tier));
        self.apply(&env, now_ms);
        Some((env, id_hex))
    }

    // ---- world bootstrap (deploy: a seed mints a LIVE starter flock) -------

    /// **World bootstrap (deploy finalization).** Mint `count` LIVE genesis
    /// sheep into this (seed) engine at wall-clock `now_ms`, each backed enough
    /// to survive the world's decay, and return the signed envelopes (Mints +
    /// Votes) to publish into the swarm so `/api/flock` shows a living, watchable
    /// world from boot.
    ///
    /// Pure (clock injected): the caller passes the real `now_ms` from net.rs.
    ///
    /// **Bootstrap policy (a deploy choice, not fully specified by §2.1):**
    /// - **Minter:** the seed's OWN key, made **credit-exempt** for this call —
    ///   the operator seeds the world, and a fresh seed has earned nothing, so it
    ///   can't pay the §3 mint cost. This mirrors the genesis-minter grandfathering
    ///   (§3) but scopes it to the operator's own key. The exemption persists on
    ///   this engine (it is the operator's node) — re-running is harmless.
    /// - **Distinct genomes:** the mint seed (`ts_micros`) is varied per sheep
    ///   (`now_micros + i`), so `derive_minted` yields a distinct genome/identity
    ///   each. The mint `ts` is wall-clock `now_ms` → `birth_ms == now`, so the
    ///   age is ~0 and decay hasn't bitten (unlike the fixed-2023 demo genesis).
    /// - **Initial backing:** `initial_backing` self-votes per sheep (credit-
    ///   exempt) so `vitality = backing − decay(0) = backing − base > 0` under the
    ///   world's decay. `4` comfortably clears both worlds' `base` (0.5).
    /// - **Restart behavior:** **a restart RE-SEEDS** (NON-deterministic ids).
    ///   Because the mint seed is wall-clock-derived, a restarted seed mints a
    ///   FRESH starter flock with new identities rather than reviving the old one.
    ///   Chosen for simplicity + honesty: the old flock isn't lost (it's gossiped
    ///   + held by peers/the watch cache, and ages out naturally via §2.2), and a
    ///   seed restart is rare. The alternative (deterministic per-world ids stable
    ///   across restart) would need a persisted world-salt; deferred as it buys
    ///   little for a bootstrap-only flock that decay churns anyway.
    pub fn bootstrap_seed_flock(
        &mut self,
        count: usize,
        initial_backing: u64,
        now_ms: u64,
    ) -> Vec<Envelope> {
        let mut out = Vec::new();
        // The operator seeds the world: grandfather our own key past the §3
        // credit check for these births (a fresh seed has earned nothing).
        let me = self.self_pub.clone();
        self.credit_exempt.insert(me.clone());
        let Some(minter) = decode_hex_32(&me) else {
            return out;
        };
        let base_micros = now_ms.saturating_mul(1000);

        for i in 0..count {
            // Distinct genome per sheep: vary the mint seed (`ts_micros`).
            let ts_micros = base_micros + i as u64;
            let tier = ResolutionTier::R384; // cheapest tier — fast to render.
            let seq = self.next_spend_seq;
            self.next_spend_seq += 1;
            let mint = Mint {
                ts_micros,
                minter_pub: me.clone(),
                resolution: tier,
                seq,
            };
            // Sign with ts = now_ms so birth_ms (ts_micros/1000) ≈ now and the
            // sheep is freshly-born under wall-clock decay.
            let env = self.sign(proto::FLOCK, to_value(&mint), now_ms);
            if !self.apply(&env, now_ms) {
                continue; // (shouldn't happen for a fresh distinct mint)
            }
            out.push(env);

            // The resulting identity (re-derived exactly as `apply_mint` does).
            let genome = derive_minted(ts_micros, &minter);
            let id_hex = hex_lower(&sheep_identity(&genome, tier));

            // Initial backing: self-votes so vitality > 0 under the world's decay.
            for _ in 0..initial_backing {
                let seq = self.next_spend_seq;
                self.next_spend_seq += 1;
                let vote = Vote { sheep_id: id_hex.clone(), seq };
                let venv = self.sign(proto::VOTES, to_value(&vote), now_ms);
                if self.apply(&venv, now_ms) {
                    out.push(venv);
                }
            }
        }
        out
    }

    /// §2.x **population floor — anti-extinction replenishment.** Guarantee the
    /// LIVE flock never empties: if fewer than `floor` sheep are alive at
    /// `now_ms`, mint exactly enough FRESH founding sheep to bring the live count
    /// back up to `floor`, returning their signed Mint/Vote envelopes (ready to
    /// publish — they are applied locally here too).
    ///
    /// This does NOT override "loved or dead" (§2.2): individual sheep still
    /// decay and die on their own merits, and an over-floor flock is left
    /// untouched. The floor only protects the POPULATION — without it a fully
    /// decayed world spirals dead (no sheep → nothing to render → no
    /// contribution → no new mints), leaving an empty gallery. With it, a seed
    /// silently re-mints a fresh founding cohort the moment the live count dips
    /// below `floor`, so there is always something live to watch and build on.
    ///
    /// Pure-ish (clock injected): the caller passes the real `now_ms`. Reuses
    /// [`bootstrap_seed_flock`] for the actual minting, so the replenishment
    /// sheep are produced by exactly the same signed-Mint path as the boot flock
    /// — distinct genomes (the per-sheep `now_micros + i` seed) and, because the
    /// mint seed is wall-clock-derived, top-ups at different `now_ms` yield
    /// distinct identities (no duplicate genomes across replenishments).
    ///
    /// Caller contract (transport): only a FOUNDING seed
    /// (`world.bootstrap_flock > 0`) should call this, on a timer; a node that
    /// never seeds (`bootstrap_flock == 0`, e.g. a mirror seed or a plain worker)
    /// must NOT — otherwise two seeds double-mint. Returns `vec![]` (a no-op)
    /// when the live flock already meets or exceeds `floor`.
    pub fn maintain_floor(
        &mut self,
        floor: usize,
        initial_backing: u64,
        now_ms: u64,
    ) -> Vec<Envelope> {
        let live = self.live_flock(now_ms).len();
        let need = floor.saturating_sub(live);
        if need == 0 {
            return Vec::new();
        }
        // Reuse the boot-mint path: signed Mints with deterministically-derived,
        // per-sheep-distinct genomes, applied locally and returned for gossip.
        self.bootstrap_seed_flock(need, initial_backing, now_ms)
    }

    // ---- contribute loop (§4 tick) ----------------------------------------

    /// The contribute loop: prune expired claims, then either render the held
    /// claim's units (emitting coverage/piece/heartbeat) or — if idle — pick the
    /// least-covered eligible block and emit a `Claim`. Also drains the audit
    /// queue, emitting one `Attestation` per assigned tile. All outbound
    /// envelopes are signed with the node's key.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Envelope> {
        let mut out = Vec::new();
        self.prune_claims(now_ms);

        // 0) Disputes (§6): a single authoritative re-render of any tile that
        // has a corroborated hash mismatch → ground truth → slash + retract.
        self.resolve_disputes(now_ms);

        // 0.5) Reap the dead (§2.2): any sheep whose vitality has decayed to <=0
        // is dormant — enshrine it in the Hall if it earned legacy. (Pure: the
        // raw flock map is kept for history; `live_flock` filters the dead.)
        self.reap(now_ms);

        // 1) Audits first — they confirm others' work (§6).
        let audits: Vec<Coverage> = self.audit_queue.drain(..).collect();
        let busy_auditing = !audits.is_empty();
        for tile in audits {
            if let Some(env) = self.make_attestation(&tile, now_ms) {
                // Apply our OWN attestation to our OWN engine too. `make_attestation`
                // only BUILDS the signed envelope; without this the node confirms
                // others' work only in its PEERS' views (via gossip), never its own,
                // so the audits IT performs never update its `confirmed`/`earned_tiles`
                // — and that's exactly what /api/msg's `confirmed_tiles` reads back, so
                // a browser submitting to this gateway would see 0 confirmed forever.
                self.apply_attestation(&env);
                out.push(env);
            }
        }

        // 2) Work: render new tiles ONLY when NOT busy auditing. A seed's
        // highest-value job is CONFIRMING others' work (it is trusted), so while
        // contributors are submitting (audit queue non-empty) it audits instead of
        // competing for the same fresh tiles — otherwise the seed, being native-fast,
        // wins the render race and is recorded as the tile's submitter, so the
        // contributor's identical render earns NO credit. With no audit backlog
        // (e.g. an empty swarm at bootstrap) the seed renders to grow the flock.
        if !busy_auditing {
            if self.active.is_some() {
                self.render_active(now_ms, &mut out);
            } else if let Some(block) = self.pick_block(now_ms) {
                out.push(self.emit_claim(block, now_ms));
            }
        }

        // 3) Drain any reputation/ban news this node decided to broadcast (§6).
        let reps: Vec<RepDelta> = self.pending_rep.drain(..).collect();
        for rd in reps {
            out.push(self.sign(proto::REP, to_value(&rd), now_ms));
        }

        out
    }

    /// §6 **disputes — the only re-render.** Scan tracked tiles for a
    /// corroborated mismatch: a tile carrying two distinct hashes where one is
    /// the submitter's and at least one INDEPENDENT key (not the submitter)
    /// attested a different hash. For each, re-render THAT ONE tile for ground
    /// truth, then for every key whose claimed hash != truth: slash it and, if
    /// it is the submitter, retract its merged contribution (the fraudulent
    /// content hash → `retracted_hashes`, consumed by the accumulator). Single
    /// tile only, once per tile (never full replication).
    fn resolve_disputes(&mut self, _now_ms: u64) {
        // Collect candidate disputed tiles first (avoid borrow conflicts).
        let candidates: Vec<(String, u32, u32, u32)> = self
            .tile_hashes
            .iter()
            .filter(|(tile, by_hash)| {
                if self.disputed_tiles.contains(*tile) {
                    return false;
                }
                // Need >=2 distinct hashes, and the conflict must be corroborated
                // by >=1 key that is NOT the submitter (so a lone bad submission
                // with no independent witness doesn't trigger a re-render yet).
                if by_hash.len() < 2 {
                    return false;
                }
                let sub = self.submitter.get(*tile);
                by_hash
                    .iter()
                    .any(|(_h, keys)| keys.iter().any(|k| Some(k) != sub))
            })
            .map(|(tile, _)| tile.clone())
            .collect();

        for tile in candidates {
            self.disputed_tiles.insert(tile.clone());
            let (sheep_id, frame, idx, pass) = tile.clone();
            let Some(entry) = self.flock.get(&sheep_id).cloned() else {
                continue;
            };
            let Some(identity) = decode_hex_32(&sheep_id) else {
                continue;
            };
            // Ground truth: one authoritative re-render of this single tile.
            let accum =
                self.render_tile(&entry.genome, &identity, entry.resolution, frame, idx, pass);
            let truth = hist_hash_hex(&accum);

            // Anyone whose claimed hash != truth is a fraudster: slash, and if
            // they submitted the tile, retract that submission's content hash.
            let by_hash = self.tile_hashes.get(&tile).cloned().unwrap_or_default();
            let sub = self.submitter.get(&tile).cloned();
            for (hash, keys) in &by_hash {
                if hash == &truth {
                    continue;
                }
                // The submitter's fraudulent histogram hash must be retracted
                // from the accumulator (keyed CRDT removal, §6).
                if sub.as_ref().is_some_and(|s| keys.contains(s)) {
                    self.retracted_hashes.insert(hash.clone());
                }
                for k in keys {
                    self.slash_key(k);
                }
            }

            // The tile's confirmed status is rebuilt from the truth: keep it
            // confirmed only if at least one honest key attested the truth.
            let truth_had_support = by_hash.get(&truth).is_some_and(|s| !s.is_empty());
            if !truth_had_support {
                if let Some(s) = self.confirmed.get_mut(&sheep_id) {
                    s.remove(&(frame, idx, pass));
                }
            }
        }
    }

    /// §2.2 **reap the dead + enshrine.** For every sheep whose vitality has
    /// decayed to `<= 0` at `now_ms` and which we have not yet enshrined: if it
    /// lived past the lifespan threshold OR ever reached the peak-backing
    /// threshold, record a [`HallEntry`] (mortality with legacy). The sheep
    /// stays in the raw `flock` map (history); [`live_flock`] excludes it.
    ///
    /// Pure + convergent: every node, fed the same births + votes + clock,
    /// enshrines the same set (nodes may differ by seconds on the death moment —
    /// soft and self-healing, not a fork, §2.2).
    fn reap(&mut self, now_ms: u64) {
        // Collect dead-and-not-yet-enshrined ids first (avoid borrow conflict).
        let dead: Vec<String> = self
            .flock
            .keys()
            .filter(|id| !self.enshrined.contains(*id))
            .filter(|id| !self.is_alive(id, now_ms))
            .cloned()
            .collect();

        for id in dead {
            // Mark observed-dead so we don't re-evaluate every tick.
            self.enshrined.insert(id.clone());
            let Some(entry) = self.flock.get(&id) else {
                continue;
            };
            let lifespan_ms = now_ms.saturating_sub(entry.birth_ms);
            let peak = self.peak_backing.get(&id).copied().unwrap_or(0);
            let worthy = lifespan_ms >= self.hall_threshold.min_lifespan_ms
                || peak >= self.hall_threshold.min_peak_backing;
            if worthy {
                self.hall.push(HallEntry {
                    sheep_id: id.clone(),
                    genome: entry.genome.clone(),
                    resolution: entry.resolution,
                    birth_ms: entry.birth_ms,
                    death_ms: now_ms,
                    lifespan_ms,
                    peak_backing: peak,
                });
            }
        }
    }

    /// Re-render an assigned audit tile and attest the observed hash (§6).
    /// Returns `None` if the sheep is unknown.
    fn make_attestation(&self, tile: &Coverage, now_ms: u64) -> Option<Envelope> {
        let entry = self.flock.get(&tile.sheep_id)?;
        let identity = decode_hex_32(&tile.sheep_id)?;
        let accum = self.render_tile(
            &entry.genome,
            &identity,
            entry.resolution,
            tile.frame,
            tile.idx,
            tile.pass,
        );
        let att = Attestation {
            sheep_id: tile.sheep_id.clone(),
            frame: tile.frame,
            idx: tile.idx,
            pass: tile.pass,
            hash: hist_hash_hex(&accum),
        };
        Some(self.sign(proto::ATTEST, to_value(&att), now_ms))
    }

    /// Render every remaining unit of the active claim, emitting `Coverage` +
    /// `PieceUpload` per unit and a `Heartbeat`; on completion free the claim.
    fn render_active(&mut self, now_ms: u64, out: &mut Vec<Envelope>) {
        let Some(active) = self.active.take() else {
            return;
        };
        let sheep_hex = active.block.sheep_hex();
        let Some(entry) = self.flock.get(&sheep_hex).cloned() else {
            // Sheep vanished from the flock; drop the claim.
            return;
        };

        // Heartbeat extends our own claim's TTL while working (§4).
        out.push(self.sign(
            proto::CLAIMS,
            to_value(&Heartbeat { block_id: active.block.to_wire() }),
            now_ms,
        ));

        for unit in &active.remaining {
            // Render the tile's accumulation once: its hash is the content
            // address (gossiped in Coverage/Attestation), and its compressed
            // bytes are the heavy PieceUpload artifact (§5).
            let accum = self.render_tile(
                &entry.genome,
                &active.block.sheep_identity,
                entry.resolution,
                unit.frame,
                unit.idx,
                unit.pass,
            );
            let hash = hist_hash_hex(&accum);
            // Mark as our own rendered tile so confirmation credits us (§3).
            self.own_rendered
                .insert((sheep_hex.clone(), unit.frame, unit.idx, unit.pass));
            // Coverage/`have` progress gossip (§4).
            out.push(self.sign(
                proto::PROGRESS,
                to_value(&Coverage {
                    sheep_id: sheep_hex.clone(),
                    frame: unit.frame,
                    idx: unit.idx,
                    pass: unit.pass,
                    hash: hash.clone(),
                }),
                now_ms,
            ));
            // Heavy piece upload, peer -> seed (§5). `count` is a String (§10);
            // `hist_b64` carries the compressed histogram in the SAME format
            // the coordinator/accumulator decodes (see crate::hist).
            out.push(self.sign(
                proto::PIECE,
                to_value(&PieceUpload {
                    sheep_id: sheep_hex.clone(),
                    frame: unit.frame,
                    idx: unit.idx,
                    pass: unit.pass,
                    hash,
                    count: flame_core::chunked::total_count(&accum).to_string(),
                    hist_b64: crate::hist::encode_accum(&accum),
                }),
                now_ms,
            ));
            // Locally track our own submission as unaudited until attested.
            self.unaudited
                .entry(sheep_hex.clone())
                .or_default()
                .insert((unit.frame, unit.idx, unit.pass));
        }

        // Block complete: record it and free the single-claim slot (§4
        // complete-before-next). `active` is already taken; leaving
        // `self.active = None` unlocks the next, and recording the block makes
        // the next claim advance to fresh work.
        self.completed_blocks.insert(active.block.to_wire());
    }

    /// Render one tile's accumulation buffer. Pass-aware (§4): a later `pass`
    /// over the same `(frame, idx)` draws a DISTINCT sample stream, so summing
    /// passes raises density instead of re-adding the same samples.
    ///
    /// flame-core's `render_batch` seeds from `batch_seed(seed_id, frame, idx)`
    /// only (no pass input). Rather than touch flame-core's determinism math, we
    /// mix `pass` into the *seed-id* bytes we hand it: `pass == 0` passes the
    /// bare identity through unchanged (so existing `(frame, idx, pass=0)` hashes
    /// and the golden render are untouched), and `pass > 0` derives a fresh
    /// 32-byte seed-id `sha256(identity ‖ "pass" ‖ le32(pass))`. The histogram
    /// dimensions and camera framing are identical across passes, so the buffers
    /// remain element-wise mergeable in the accumulator.
    fn render_tile(
        &self,
        genome: &Genome,
        identity: &[u8; 32],
        tier: ResolutionTier,
        frame: u32,
        idx: u32,
        pass: u32,
    ) -> Accum {
        let edge = tier.edge() as usize;
        let seed_id = pass_seed_id(identity, pass);
        // Seed binds the sheep identity (not the bare genome id) so two tiers of
        // one genome are distinct sheep with distinct sample streams.
        render_batch(
            genome,
            &seed_id,
            frame,
            idx,
            edge,
            edge,
            1,
            SPP,
            crate::spec::N_FRAMES,
        )
    }

    /// Pick the least-covered eligible block (§4 + §4.1) for a fresh claim, or
    /// `None` if every sheep is capped or there is no flock.
    fn pick_block(&self, now_ms: u64) -> Option<BlockId> {
        if self.flock.is_empty() {
            return None;
        }
        let total = self.total_coverage();
        let min_cov = self
            .flock
            .keys()
            .map(|s| self.coverage(s))
            .min()
            .unwrap_or(0);
        let enforce = total > COVERAGE_FLOOR;

        // Candidate sheep: not over the per-sheep cap (§4.1). Among eligible,
        // pick the lowest coverage (least-covered selection, §4). Ties broken
        // by identity hex for determinism.
        let mut eligible: Vec<(&String, u64)> = self
            .flock
            .keys()
            .map(|s| (s, self.coverage(s)))
            .filter(|(_, cov)| !(enforce && *cov > min_cov + COVERAGE_TOLERANCE))
            .collect();
        eligible.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));

        let (sheep_hex, _) = eligible.first()?;
        let identity = decode_hex_32(sheep_hex)?;

        // Lowest unclaimed block for that sheep. Live claims by OTHERS are
        // preferred-against (soft claims, §4); our own held claim doesn't apply
        // (we only claim when idle). Scan from block 0 upward; work is
        // unbounded so a free block always exists.
        let mut block_index = 0u64;
        loop {
            let block = BlockId { sheep_identity: identity, block_index };
            let wire = block.to_wire();
            let claimed_by_other = self
                .claims
                .get(&wire)
                .is_some_and(|c| c.expiry_ms > now_ms && c.claimant != self.self_pub);
            let already_done = self.completed_blocks.contains(&wire);
            if !claimed_by_other && !already_done {
                return Some(block);
            }
            block_index += 1;
            // Safety bound: never spin unboundedly in a pure function.
            if block_index > 1_000_000 {
                return None;
            }
        }
    }

    /// Emit a signed `Claim` for `block`, set it as the active work (§4 one at a
    /// time), and bump the node's own claim sequence.
    fn emit_claim(&mut self, block: BlockId, now_ms: u64) -> Envelope {
        let seq = self.next_seq;
        self.next_seq += 1;
        let claim = Claim {
            block_id: block.to_wire(),
            expiry: now_ms + CLAIM_TTL_MS,
            claimant: self.self_pub.clone(),
            seq,
        };
        let env = self.sign(proto::CLAIMS, to_value(&claim), now_ms);
        // Record our own claim locally so we don't re-pick it.
        self.claims.insert(
            block.to_wire(),
            LiveClaim {
                claimant: self.self_pub.clone(),
                expiry_ms: now_ms + CLAIM_TTL_MS,
                seq,
            },
        );
        // Remember our own seq for self-equivocation safety.
        self.seq_seen
            .entry(self.self_pub.clone())
            .or_default()
            .insert(seq, env.canonical());
        self.active = Some(ActiveWork {
            block,
            remaining: block_units(block),
        });
        env
    }

    // ---- internals ---------------------------------------------------------

    /// Drop expired claims (TTL lapsed; §4).
    fn prune_claims(&mut self, now_ms: u64) {
        self.claims.retain(|_, c| c.expiry_ms > now_ms);
    }

    /// Slash a key (§6/§7): mark it slashed + banned, queue a banning `RepDelta`
    /// for propagation, and drop any live claims it holds. Idempotent.
    fn slash_key(&mut self, key: &str) {
        let newly = self.slashed.insert(key.to_string());
        self.banned.insert(key.to_string());
        self.claims.retain(|_, c| c.claimant != key);
        self.reputation.remove(key);
        if newly {
            self.pending_rep.push(RepDelta {
                peer: key.to_string(),
                rep: 0,
                banned: true,
            });
        }
    }

    /// Raise a peer's log-derived reputation (§6) by `d`. Banned keys never
    /// accrue standing.
    fn bump_rep(&mut self, peer: &str, d: u64) {
        if self.banned.contains(peer) {
            return;
        }
        *self.reputation.entry(peer.to_string()).or_insert(0) += d;
    }

    /// Sign a body into an outbound envelope (`ts = ` not clock-read here; the
    /// transport sets a real ts, but for a pure engine we leave it 0 and sign —
    /// signature still verifies since `canonical()` includes whatever ts is).
    fn sign(&self, t: &str, body: Value, ts: u64) -> Envelope {
        let mut env = Envelope::new(t, self.self_pub.clone(), ts, body);
        env.sign(&self.signing_key);
        env
    }
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).expect("message body -> Value cannot fail")
}

/// The 32-byte seed-id handed to `render_batch` for a given `(identity, pass)`.
/// Pass 0 is the identity verbatim (no behavior change vs. a passless render);
/// pass `p>0` is `sha256(identity ‖ "pass" ‖ le32(p))` so each pass renders a
/// distinct, deterministic sample stream that adds density when accumulated.
pub fn pass_seed_id(identity: &[u8; 32], pass: u32) -> [u8; 32] {
    if pass == 0 {
        return *identity;
    }
    let mut hasher = Sha256::new();
    hasher.update(identity);
    hasher.update(b"pass");
    hasher.update(pass.to_le_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = s.as_bytes();
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    for i in 0..32 {
        let hi = nibble(bytes[i * 2])?;
        let lo = nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}
