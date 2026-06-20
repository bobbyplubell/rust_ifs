//! Step-4 trust / anti-fraud layer tests (ARCHITECTURE v3 §6).
//!
//! Covers the four §6 properties built into the engine:
//!  - **unpredictable, verifiable, unselectable audit assignment** (pure);
//!  - **reputation-graduated sampling** (pure);
//!  - **disputes** — the only re-render: a tampered tile is caught by an honest
//!    auditor → submitter slashed + its contribution retracted (render-heavy);
//!  - **honeypots** — a lazy auditor that attests without rendering is caught
//!    (render-heavy);
//!  - **reputation/ban propagation** — `RepDelta` round-trips; a slashed key's
//!    later submissions are rejected.
//!
//! The pure tests are instant; the two render-heavy ones each render a SINGLE
//! tile at R384 (slow in debug, fast in `--release`).

use ed25519_dalek::SigningKey;
use flame_core::chunked::{hist_hash_hex, render_batch};
use sheep_node::engine::{
    assigned_to_audit, sample_rate, Engine, CONFIRM_QUORUM_REP_SUM, DEFAULT_ROUND_SALT,
    NEW_PEER_RATE, SAMPLE_FLOOR, TRUSTED_ATTESTOR_REP, TRUST_REP,
};
use sheep_node::spec::{IDXS_PER_FRAME, N_FRAMES, SPP};
use sheep_proto::derive::derive_minted;
use sheep_proto::identity::{sheep_identity, ResolutionTier};
use sheep_proto::msg::{Attestation, Coverage, Mint, RepDelta};
use sheep_proto::{proto, Envelope};

// ---- helpers ----------------------------------------------------------------

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn pub_hex(k: &SigningKey) -> String {
    let mut s = String::new();
    for b in k.verifying_key().to_bytes() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn signed(t: &str, k: &SigningKey, ts: u64, body: serde_json::Value) -> Envelope {
    let mut env = Envelope::new(t, "", ts, body);
    env.sign(k);
    env
}

/// A valid Mint envelope + the resulting sheep identity hex.
fn mint(k: &SigningKey, ts_micros: u64, tier: ResolutionTier) -> (Envelope, String) {
    let minter_pub = pub_hex(k);
    let body = serde_json::to_value(&Mint {
        ts_micros,
        minter_pub,
        resolution: tier,
        // Unique per mint so two mints from one key don't collide at one seq.
        seq: ts_micros,
    })
    .unwrap();
    let env = signed(proto::FLOCK, k, ts_micros / 1000, body);
    let minter = k.verifying_key().to_bytes();
    let genome = derive_minted(ts_micros, &minter);
    let id = sheep_identity(&genome, tier);
    let mut id_hex = String::new();
    for b in id {
        id_hex.push_str(&format!("{b:02x}"));
    }
    (env, id_hex)
}

/// A high reputation that drives `sample_rate` to the 5% floor, so audit
/// assignment for this submitter is RARE — used by the rep-gating tests to pin a
/// specific attestor/tile to NOT-assigned (so confirmation must come from the
/// rep/trust path under test, not the optimistic assigned path).
const SPARSE_SUBMITTER_REP: u64 = 100_000;

/// Find a `(frame, idx)` tile on `sheep` for which `attestor` is NOT assigned to
/// audit a submitter of rep `SPARSE_SUBMITTER_REP` — i.e. `assigned_to_audit`
/// returns false. At the 5% floor ~95% of tiles qualify, so this resolves almost
/// immediately. Pure + deterministic (a hash of public facts), so the chosen tile
/// is the same on every run / node.
fn unassigned_tile(attestor: &str, sheep: &str) -> (u32, u32) {
    for idx in 0..IDXS_PER_FRAME {
        for frame in 0..N_FRAMES {
            if !assigned_to_audit(
                attestor,
                (sheep, frame, idx, 0),
                SPARSE_SUBMITTER_REP,
                DEFAULT_ROUND_SALT,
            ) {
                return (frame, idx);
            }
        }
    }
    panic!("no unassigned tile found (assignment too dense?)");
}

/// Find a `(frame, idx)` tile on `sheep` that NEITHER `a` nor `b` is assigned to
/// audit for a `SPARSE_SUBMITTER_REP` submitter — used to isolate the rep-sum
/// quorum path (c) from the optimistic assigned path (b). At the 5% floor a tile
/// unassigned for BOTH keys is ~90% likely, so this resolves almost immediately.
fn unassigned_for_both(a: &str, b: &str, sheep: &str) -> (u32, u32) {
    for idx in 0..IDXS_PER_FRAME {
        for frame in 0..N_FRAMES {
            let t = (sheep, frame, idx, 0u32);
            if !assigned_to_audit(a, t, SPARSE_SUBMITTER_REP, DEFAULT_ROUND_SALT)
                && !assigned_to_audit(b, t, SPARSE_SUBMITTER_REP, DEFAULT_ROUND_SALT)
            {
                return (frame, idx);
            }
        }
    }
    panic!("no tile unassigned for both keys found");
}

/// Find a `(frame, idx)` tile on `sheep` for which `attestor` IS assigned to audit
/// a REP-0 submitter (rate ~1.0, so this is the first tile or close to it). Used to
/// prove the `A != S` bar holds even when the submitter is assigned to its own tile.
fn assigned_tile_rep0(attestor: &str, sheep: &str) -> (u32, u32) {
    for idx in 0..IDXS_PER_FRAME {
        for frame in 0..N_FRAMES {
            if assigned_to_audit(attestor, (sheep, frame, idx, 0), 0, DEFAULT_ROUND_SALT) {
                return (frame, idx);
            }
        }
    }
    panic!("no rep-0-assigned tile found");
}

// ---- §6 audit assignment: verifiable, unpredictable, unselectable -----------

#[test]
fn assignment_is_verifiable_and_unselectable() {
    let salt = DEFAULT_ROUND_SALT;
    let auditor = "a".repeat(64);
    let tile = ("ab".repeat(32), 3u32, 7u32, 1u32);
    let t = (tile.0.as_str(), tile.1, tile.2, tile.3);

    // Verifiable: recompute → identical answer (pure, deterministic).
    let first = assigned_to_audit(&auditor, t, 0, salt);
    let again = assigned_to_audit(&auditor, t, 0, salt);
    assert_eq!(first, again, "assignment is a pure, re-verifiable function");

    // Unselectable: changing ONLY the auditor pubkey changes the assignment set.
    // Across many tiles, a different auditor is assigned a DIFFERENT subset — an
    // auditor cannot pick which tiles fall to it without changing its key.
    let other = "b".repeat(64);
    let mut differ = 0;
    let mut n = 0;
    for f in 0..N_FRAMES {
        for idx in 0..8u32 {
            // mid reputation so the rate is ~50% → a meaningful, varied subset.
            let tt = (tile.0.as_str(), f, idx, 0u32);
            let a = assigned_to_audit(&auditor, tt, TRUST_REP, salt);
            let b = assigned_to_audit(&other, tt, TRUST_REP, salt);
            if a != b {
                differ += 1;
            }
            n += 1;
        }
    }
    assert!(
        differ > 0,
        "two different auditors get DIFFERENT assignment sets ({differ}/{n} tiles differ) — \
         an auditor cannot self-select its confederate's tiles"
    );

    // Salt-dependent: changing the round salt (which the auditor doesn't control)
    // reshuffles assignments — so a fixed grind against one salt doesn't persist.
    let mut salt_changed = 0;
    for f in 0..N_FRAMES {
        let tt = (tile.0.as_str(), f, 0u32, 0u32);
        let with_a = assigned_to_audit(&auditor, tt, TRUST_REP, b"salt-A");
        let with_b = assigned_to_audit(&auditor, tt, TRUST_REP, b"salt-B");
        if with_a != with_b {
            salt_changed += 1;
        }
    }
    assert!(salt_changed > 0, "the round salt reshuffles assignment");
}

#[test]
fn assignment_rate_is_reputation_graduated() {
    let salt = DEFAULT_ROUND_SALT;
    let auditor = "c".repeat(64);
    let sheep = "12".repeat(32);

    // Count assigned tiles over a large fixed tile set, for a zero-rep submitter
    // vs. a very-high-rep one. Zero-rep must be audited far more.
    let count_assigned = |rep: u64| -> usize {
        let mut c = 0;
        for f in 0..N_FRAMES {
            for idx in 0..16u32 {
                if assigned_to_audit(&auditor, (sheep.as_str(), f, idx, 0), rep, salt) {
                    c += 1;
                }
            }
        }
        c
    };
    let total = (N_FRAMES as usize) * 16;
    let low = count_assigned(0);
    let high = count_assigned(100_000);

    // Zero-rep → audited at the new-peer rate (sample_rate(0) == NEW_PEER_RATE),
    // not 100%: partial auditing still deters fraud (statistical + retroactive).
    let low_frac = low as f64 / total as f64;
    assert!(
        (low_frac - NEW_PEER_RATE).abs() < 0.05,
        "zero-rep submitter audited at ~NEW_PEER_RATE: {low}/{total} = {low_frac}"
    );
    // Very-high-rep → audited far less, but never below the 5% floor.
    assert!(high < low, "trusted submitter audited far less: high={high} low={low}");
    let high_frac = high as f64 / total as f64;
    assert!(
        high_frac >= SAMPLE_FLOOR - 0.03,
        "even a trusted submitter is audited at >= ~floor: {high_frac}"
    );
}

#[test]
fn sample_rate_curve_and_floor() {
    assert!(
        (sample_rate(0) - NEW_PEER_RATE).abs() < 1e-9,
        "zero rep → new-peer rate"
    );
    assert!(
        (sample_rate(TRUST_REP) - NEW_PEER_RATE / 2.0).abs() < 1e-9,
        "rep == TRUST_REP → half the new-peer rate"
    );
    // Monotonically decreasing.
    assert!(sample_rate(10) > sample_rate(1000));
    // Never below the floor, no matter how trusted.
    assert!(sample_rate(u64::MAX / 2) >= SAMPLE_FLOOR);
    assert_eq!(sample_rate(u64::MAX / 2), SAMPLE_FLOOR);
}

// ---- §6 reputation: log-derived from confirmations --------------------------

#[test]
fn confirmations_raise_submitter_and_auditor_rep() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    let submitter = key(3);
    let auditor = key(4);
    // Submitter gossips a Coverage (claims a hash); auditor attests the SAME hash.
    let h = "00".repeat(32);
    let cov = Coverage {
        sheep_id: id.clone(),
        frame: 0,
        idx: 0,
        pass: 0,
        hash: h.clone(),
    };
    assert!(eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov).unwrap()), 1000));
    let att = Attestation { sheep_id: id.clone(), frame: 0, idx: 0, pass: 0, hash: h };
    assert!(eng.apply(&signed(proto::ATTEST, &auditor, 1000, serde_json::to_value(&att).unwrap()), 1000));

    assert!(eng.reputation_of(&pub_hex(&submitter)) >= 1, "submitter earned rep");
    assert!(eng.reputation_of(&pub_hex(&auditor)) >= 1, "auditor earned rep");
}

// ---- §6 reputation / ban propagation ----------------------------------------

#[test]
fn ban_propagates_and_rejects_later_submissions() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    let cheater = key(7);

    // A seed/peer broadcasts a banning RepDelta for the cheater.
    let banner = key(9);
    let rd = RepDelta { peer: pub_hex(&cheater), rep: 0, banned: true };
    assert!(eng.apply(&signed(proto::REP, &banner, 1000, serde_json::to_value(&rd).unwrap()), 1000));
    assert!(eng.banned().contains(&pub_hex(&cheater)), "ban consumed");

    // The cheater's later submission is rejected wholesale.
    let cov = Coverage { sheep_id: id, frame: 0, idx: 0, pass: 0, hash: "00".repeat(32) };
    assert!(
        !eng.apply(&signed(proto::PROGRESS, &cheater, 2000, serde_json::to_value(&cov).unwrap()), 2000),
        "a banned key's later submission is rejected"
    );

    // A positive advisory rep delta round-trips into reputation.
    let subject = key(5);
    let up = RepDelta { peer: pub_hex(&subject), rep: 7, banned: false };
    assert!(eng.apply(&signed(proto::REP, &banner, 3000, serde_json::to_value(&up).unwrap()), 3000));
    assert_eq!(eng.reputation_of(&pub_hex(&subject)), 7, "advisory rep round-trips");
}

#[test]
fn slash_emits_rep_delta_on_tick() {
    // Equivocation slashes a key; the engine should EMIT a banning RepDelta on
    // the next tick so the ban propagates to the swarm.
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, _id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    let cheater = key(4);
    use sheep_node::block::BlockId;
    use sheep_proto::msg::Claim;
    let bx = BlockId { sheep_identity: [1u8; 32], block_index: 0 };
    let by = BlockId { sheep_identity: [1u8; 32], block_index: 1 };
    let c1 = Claim { block_id: bx.to_wire(), expiry: 99_000, claimant: pub_hex(&cheater), seq: 5 };
    let c2 = Claim { block_id: by.to_wire(), expiry: 99_000, claimant: pub_hex(&cheater), seq: 5 };
    assert!(eng.apply(&signed(proto::CLAIMS, &cheater, 1000, serde_json::to_value(&c1).unwrap()), 2000));
    assert!(!eng.apply(&signed(proto::CLAIMS, &cheater, 1000, serde_json::to_value(&c2).unwrap()), 2000));
    assert!(eng.slashed().contains(&pub_hex(&cheater)));

    // The tick drains a banning RepDelta for the slashed key.
    let out = eng.tick(3000);
    let ban_rd = out.iter().find_map(|e| {
        if e.t == proto::REP {
            serde_json::from_value::<RepDelta>(e.body.clone()).ok()
        } else {
            None
        }
    });
    let rd = ban_rd.expect("a RepDelta is emitted for the slashed key");
    assert_eq!(rd.peer, pub_hex(&cheater));
    assert!(rd.banned, "the emitted RepDelta marks the key banned");
}

// ---- §6 disputes — the only re-render (render-heavy, single tile) ------------

#[test]
fn dispute_catches_tampered_tile_slashes_and_retracts() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    // The TRUE hash for tile (0,0,0) — an independent flame_core render.
    let entry = eng.flock().get(&id).unwrap();
    let identity = sheep_identity(&entry.genome, entry.resolution);
    let edge = entry.resolution.edge() as usize;
    let truth = hist_hash_hex(&render_batch(&entry.genome, &identity, 0, 0, edge, edge, 1, SPP, N_FRAMES));

    // A fraudulent SUBMITTER gossips a Coverage with a WRONG hash.
    let fraud = key(7);
    let bad_hash = "deadbeef".repeat(8); // 64 hex, != truth
    assert_ne!(bad_hash, truth);
    let cov = Coverage { sheep_id: id.clone(), frame: 0, idx: 0, pass: 0, hash: bad_hash.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &fraud, 1000, serde_json::to_value(&cov).unwrap()), 1000));

    // An HONEST auditor attests the TRUTH — corroborated mismatch → dispute.
    let auditor = key(8);
    let att = Attestation { sheep_id: id.clone(), frame: 0, idx: 0, pass: 0, hash: truth.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &auditor, 1000, serde_json::to_value(&att).unwrap()), 1000));

    // Tick triggers the single-tile re-render → ground truth → slash + retract.
    let _ = eng.tick(2000);

    assert!(
        eng.slashed().contains(&pub_hex(&fraud)),
        "the fraudulent submitter is slashed by the dispute"
    );
    assert!(
        !eng.slashed().contains(&pub_hex(&auditor)),
        "the honest auditor is NOT slashed"
    );
    assert!(
        eng.retracted_hashes().contains(&bad_hash),
        "the fraudulent content hash is retracted (accumulator subtracts it)"
    );
    assert!(
        !eng.retracted_hashes().contains(&truth),
        "the honest hash is never retracted"
    );
}

#[test]
fn honest_tile_is_never_disputed() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    let entry = eng.flock().get(&id).unwrap();
    let identity = sheep_identity(&entry.genome, entry.resolution);
    let edge = entry.resolution.edge() as usize;
    let truth = hist_hash_hex(&render_batch(&entry.genome, &identity, 0, 0, edge, edge, 1, SPP, N_FRAMES));

    // Submitter and auditor agree on the true hash — no conflict, no dispute.
    // The auditor is granted trusted standing (§6) so its lone honest attestation
    // confirms the tile — this test asserts an honest tile is never disputed AND
    // stays confirmed, so we need confirmation to land (the §6 trusted-attestor
    // path) without standing up a second auditor for quorum.
    let submitter = key(3);
    let auditor = key(4);
    eng.grant_rep(pub_hex(&auditor), TRUSTED_ATTESTOR_REP);
    let cov = Coverage { sheep_id: id.clone(), frame: 0, idx: 0, pass: 0, hash: truth.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov).unwrap()), 1000));
    let att = Attestation { sheep_id: id.clone(), frame: 0, idx: 0, pass: 0, hash: truth.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &auditor, 1000, serde_json::to_value(&att).unwrap()), 1000));

    let _ = eng.tick(2000);
    assert!(eng.slashed().is_empty(), "an honest tile slashes nobody");
    assert!(eng.retracted_hashes().is_empty(), "nothing retracted for honest work");
    assert!(eng.coverage(&id) >= 1, "the honest tile stays confirmed");
}

// ---- §6 honeypots (render-heavy, single tile) -------------------------------

#[test]
fn honeypot_catches_lazy_auditor() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    // Plant a known-answer tile: the node renders it and remembers the truth.
    let truth = eng.plant_honeypot(&id, 0, 0, 0).expect("plant a honeypot");

    // An honest auditor attests the truth → not caught, earns rep.
    let honest = key(5);
    let good = Attestation { sheep_id: id.clone(), frame: 0, idx: 0, pass: 0, hash: truth.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &honest, 1000, serde_json::to_value(&good).unwrap()), 1000));
    assert!(!eng.honeypot_caught().contains(&pub_hex(&honest)), "honest auditor not caught");
    assert!(eng.reputation_of(&pub_hex(&honest)) >= 1, "honest honeypot pass earns rep");

    // A lazy auditor attests WITHOUT rendering (a fabricated hash) → caught + slashed.
    let lazy = key(6);
    let bogus = "feedface".repeat(8);
    assert_ne!(bogus, truth);
    let bad = Attestation { sheep_id: id, frame: 0, idx: 0, pass: 0, hash: bogus };
    // Apply returns true (state changed: the liar is slashed).
    assert!(eng.apply(&signed(proto::ATTEST, &lazy, 1000, serde_json::to_value(&bad).unwrap()), 1000));
    assert!(eng.honeypot_caught().contains(&pub_hex(&lazy)), "lazy auditor caught by honeypot");
    assert!(eng.slashed().contains(&pub_hex(&lazy)), "lazy auditor slashed");
}

// ---- §6.2 reputation-anchored, quorum confirmation (the Sybil fix) ----------
//
// These exercise the NEW confirmation rule that replaced "any one attestation
// confirms": a tile is confirmed iff its set of valid (non-slashed) distinct
// attestors satisfies EITHER (a) one local-node / rep>=TRUSTED_ATTESTOR_REP
// attestor, OR (b) >=CONFIRM_QUORUM distinct attestors whose summed rep reaches
// CONFIRM_QUORUM_REP_SUM. Rep-0 disposable keys add 0 to the sum, so Sybils
// can't self-confirm.

/// **Retargeted for the optimistic posture (was `sybil_self_confirm_is_rejected`).**
/// Under the old rep-gating rule the assertion was "no rep-0 key confirms", but the
/// scaling fix lets any *assigned non-submitter* rep-0 key confirm (1:1 audit↔render).
/// So that blanket assertion no longer holds. What survives — and is the load-bearing
/// invariant — is that the SUBMITTER ITSELF can never confirm its own tile, even when
/// it (and a flood of its own disposable keys) is assigned. We pin the submitter to a
/// tile it IS assigned to audit, prove its self-attestation does NOT confirm, and then
/// prove the *only* way a fresh self-submitted tile gets retracted is the dispute path
/// (a conflicting honest attestation → re-render → slash) — i.e. fraud is caught
/// optimistically, not prevented up front.
#[test]
fn submitter_cannot_self_confirm() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    let h = "00".repeat(32);
    let submitter = key(50);
    let sub_pub = pub_hex(&submitter);

    // Pick a tile the submitter IS assigned to audit (rep-0 submitter ⇒ assigned
    // at rate ~1.0, so almost any tile qualifies — assert it explicitly so the
    // test proves the `A != S` bar holds *despite* assignment, not by luck).
    let (frame, idx) = assigned_tile_rep0(&sub_pub, &id);
    assert!(
        eng.is_assigned(&sub_pub, (id.as_str(), frame, idx, 0), &sub_pub),
        "submitter is assigned to audit its own tile (so only the A != S bar stops it)"
    );

    // The submitter gossips Coverage for the tile, then attests it itself, several
    // times over from its own key — none of which may confirm (A == S barred).
    let cov = Coverage { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov).unwrap()), 1000));
    let self_att = Attestation { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
    eng.apply(&signed(proto::ATTEST, &submitter, 1000, serde_json::to_value(&self_att).unwrap()), 1000);
    assert_eq!(
        eng.coverage(&id), 0,
        "the submitter's own (even assigned) attestation can never confirm its own tile"
    );

    // The optimistic defense in action: a fraudulent self-submitted tile is caught
    // only when an HONEST auditor attests a CONFLICTING (true) hash → dispute → the
    // submitter is slashed. (Reuses the dispute machinery the dispute test exercises;
    // here we just confirm the fraudster's hash is the one retracted, not the truth.)
    let entry = eng.flock().get(&id).unwrap();
    let identity = sheep_identity(&entry.genome, entry.resolution);
    let edge = entry.resolution.edge() as usize;
    let truth = hist_hash_hex(&render_batch(&entry.genome, &identity, frame, idx, edge, edge, 1, SPP, N_FRAMES));
    assert_ne!(truth, h, "the submitter's bogus hash differs from ground truth");
    let honest = key(60);
    let honest_att = Attestation { sheep_id: id.clone(), frame, idx, pass: 0, hash: truth.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &honest, 1000, serde_json::to_value(&honest_att).unwrap()), 1000));
    let _ = eng.tick(2000);
    assert!(eng.slashed().contains(&sub_pub), "the dispute slashes the fraudulent self-submitter");
    assert!(eng.retracted_hashes().contains(&h), "the fraudster's hash is retracted, not the truth");
}

/// The trusted-attestor path (a): the LOCAL node always confirms (gateway/seed
/// path preserved), and a peer with rep >= TRUSTED_ATTESTOR_REP confirms alone.
#[test]
fn trusted_attestor_confirms() {
    // --- local node (self_pub) attests → confirmed (gateway/seed path) -------
    let local = key(1);
    let mut eng = Engine::new(local.clone());
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));
    let h = "00".repeat(32);
    // A submitter gossips the tile, then the LOCAL node attests it. We forge the
    // local node's signed attestation envelope with its own key so `env.from`
    // equals `self_pub` (the always-trusted path).
    let submitter = key(3);
    let cov = Coverage { sheep_id: id.clone(), frame: 1, idx: 0, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov).unwrap()), 1000));
    let att = Attestation { sheep_id: id.clone(), frame: 1, idx: 0, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &local, 1000, serde_json::to_value(&att).unwrap()), 1000));
    assert!(eng.coverage(&id) >= 1, "the local node's own attestation always confirms");

    // --- a peer with rep >= TRUSTED_ATTESTOR_REP attests → confirmed ---------
    let mut eng2 = Engine::new(key(1));
    assert!(eng2.apply(&m, 1000));
    let peer = key(9);
    eng2.grant_rep(pub_hex(&peer), TRUSTED_ATTESTOR_REP); // exactly at the bar
    let att2 = Attestation { sheep_id: id.clone(), frame: 2, idx: 0, pass: 0, hash: h.clone() };
    assert!(eng2.apply(&signed(proto::ATTEST, &peer, 1000, serde_json::to_value(&att2).unwrap()), 1000));
    assert!(eng2.coverage(&id) >= 1, "a single rep>=32 attestor confirms alone");
}

/// Fix 3 — the explicit mutual-trust path (a): a key in `trusted_keys` confirms
/// a tile ALONE even with rep 0 and even though it is not the local node. This is
/// the two-seed cold-start fix: each seed lists the OTHER's pubkey, so both seeds'
/// attestations confirm immediately with no rep warm-up between them.
#[test]
fn trusted_key_confirms_alone_with_zero_rep() {
    let local = key(1);
    let mut eng = Engine::new(local.clone());
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));
    let h = "00".repeat(32);

    // A peer that is NOT the local node and has rep 0, but is explicitly trusted
    // (e.g. the other seed). Its lone attestation must confirm the tile.
    let other_seed = key(77);
    eng.add_trusted_key(pub_hex(&other_seed));
    assert_eq!(eng.reputation_of(&pub_hex(&other_seed)), 0, "trusted key has earned no rep");

    let submitter = key(3);
    let cov = Coverage { sheep_id: id.clone(), frame: 0, idx: 0, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov).unwrap()), 1000));
    let att = Attestation { sheep_id: id.clone(), frame: 0, idx: 0, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &other_seed, 1000, serde_json::to_value(&att).unwrap()), 1000));
    assert!(eng.coverage(&id) >= 1, "an explicitly-trusted rep-0 attestor confirms a tile alone");

    // A NON-trusted rep-0 peer's lone attestation still does NOT confirm (the
    // trust is explicit, not a blanket free pass).
    let stranger = key(78);
    let cov2 = Coverage { sheep_id: id.clone(), frame: 1, idx: 0, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &stranger, 1000, serde_json::to_value(&cov2).unwrap()), 1000));
    let att2 = Attestation { sheep_id: id.clone(), frame: 1, idx: 0, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &stranger, 1000, serde_json::to_value(&att2).unwrap()), 1000));
    assert!(
        eng.coverage(&id) < 2,
        "an untrusted rep-0 attestor does not confirm (trust stays explicit)"
    );
}

/// The quorum path (c): two distinct attestors whose rep SUMS to >= the quorum
/// rep-sum confirm; two rep-0 keys do not. **Posture note:** under the scaling fix
/// an *assigned* attestor confirms alone via path (b), which would mask the quorum
/// path. To isolate path (c) we give the tile a HIGH-REP recorded submitter (so the
/// audit lottery is at its 5% floor) and place it on a tile NEITHER attestor is
/// assigned to — then only the rep-sum quorum can confirm it.
#[test]
fn quorum_of_established_confirms() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));
    let h = "00".repeat(32);

    // Two established peers, each BELOW the trusted bar (so neither confirms via
    // path (a)), but summing to >= CONFIRM_QUORUM_REP_SUM (path (c)).
    let p1 = key(20);
    let p2 = key(21);
    let half = CONFIRM_QUORUM_REP_SUM / 2 + 1; // each < TRUSTED_ATTESTOR_REP (32); sum >= 24
    assert!(half < TRUSTED_ATTESTOR_REP, "each attestor is individually sub-trusted");
    eng.grant_rep(pub_hex(&p1), half);
    eng.grant_rep(pub_hex(&p2), half);

    // A HIGH-REP submitter records the tile (drives the audit rate to the floor),
    // and we pick a tile NEITHER p1 nor p2 is assigned to — so path (b) can't fire
    // and only the rep-sum quorum (c) can confirm.
    let submitter = key(19);
    eng.grant_rep(pub_hex(&submitter), SPARSE_SUBMITTER_REP);
    let (frame, idx) = unassigned_for_both(&pub_hex(&p1), &pub_hex(&p2), &id);
    let cov = Coverage { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov).unwrap()), 1000));

    // First attestor alone does NOT confirm (only one, sub-trusted, unassigned).
    let a1 = Attestation { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &p1, 1000, serde_json::to_value(&a1).unwrap()), 1000));
    assert_eq!(eng.coverage(&id), 0, "one sub-trusted unassigned attestor is not a quorum");

    // Second distinct attestor brings the count to 2 and the rep-sum over the
    // threshold → confirmed via path (c).
    let a2 = Attestation { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &p2, 1000, serde_json::to_value(&a2).unwrap()), 1000));
    assert!(eng.coverage(&id) >= 1, "two distinct attestors summing past the rep-sum confirm");

    // Control: two FRESH rep-0 keys on a DIFFERENT tile, again recorded by the
    // high-rep submitter and unassigned to both, do NOT confirm it (count 2, but
    // rep-sum stays under the threshold). Coverage must not grow.
    let cov_before = eng.coverage(&id);
    let z1 = key(22);
    let z2 = key(23);
    let (f2, i2) = unassigned_for_both(&pub_hex(&z1), &pub_hex(&z2), &id);
    assert!((f2, i2) != (frame, idx), "control tile differs from the confirmed one");
    let cov2 = Coverage { sheep_id: id.clone(), frame: f2, idx: i2, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov2).unwrap()), 1000));
    let b1 = Attestation { sheep_id: id.clone(), frame: f2, idx: i2, pass: 0, hash: h.clone() };
    let b2 = Attestation { sheep_id: id.clone(), frame: f2, idx: i2, pass: 0, hash: h.clone() };
    eng.apply(&signed(proto::ATTEST, &z1, 1000, serde_json::to_value(&b1).unwrap()), 1000);
    eng.apply(&signed(proto::ATTEST, &z2, 1000, serde_json::to_value(&b2).unwrap()), 1000);
    // NB: each attestation bumps the attestor's rep by 1 (the bootstrap), so after
    // both, z1/z2 have rep 1 each → sum 2, still well under CONFIRM_QUORUM_REP_SUM.
    assert_eq!(
        eng.coverage(&id), cov_before,
        "two rep-0 unassigned keys (quorum count met, rep-sum not) do not confirm a new tile"
    );
}

/// **Bootstrap proof:** a browser-like key with NO initial standing climbs rep
/// past the trusted bar purely by attesting assigned tiles (the unchanged
/// `bump_rep` path), and THEN its attestation confirms a tile — auditors earn
/// trust by honest work even before any of their attestations confirm anything.
#[test]
fn attestor_earns_rep_then_can_confirm() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));
    let h = "00".repeat(32);

    let browser = key(40);
    let browser_pub = pub_hex(&browser);
    assert_eq!(eng.reputation_of(&browser_pub), 0, "starts with no standing");

    // A HIGH-REP submitter records each warm-up tile (drives the audit rate to the
    // floor) AND we pick tiles the browser is NOT assigned to — so none of the
    // browser's warm-up attestations can confirm via the optimistic assigned path
    // (b); they only accrue standing. (Under the scaling fix an assigned attestor
    // confirms alone, which would mask the rep climb this test is about.)
    let submitter = key(41);
    eng.grant_rep(pub_hex(&submitter), SPARSE_SUBMITTER_REP);

    // Collect TRUSTED_ATTESTOR_REP - 1 distinct tiles the browser is NOT assigned
    // to. We warm up to exactly ONE BELOW the bar: each valid attestation bumps the
    // browser's rep by 1 (the bootstrap), and `bump_rep` runs BEFORE the confirm
    // test, so the attestation that *crosses* the bar would confirm its own tile.
    // Stopping at rep == TRUSTED_ATTESTOR_REP - 1 keeps every warm-up tile
    // sub-trusted + unassigned ⇒ none confirms — then the next (fresh-tile)
    // attestation crosses the bar and confirms via path (a).
    let warmup_n = TRUSTED_ATTESTOR_REP - 1;
    let mut warmup: Vec<(u32, u32)> = Vec::new();
    'outer: for idx in 0..IDXS_PER_FRAME {
        for frame in 0..N_FRAMES {
            if !assigned_to_audit(&browser_pub, (id.as_str(), frame, idx, 0), SPARSE_SUBMITTER_REP, DEFAULT_ROUND_SALT) {
                warmup.push((frame, idx));
                if warmup.len() as u64 == warmup_n {
                    break 'outer;
                }
            }
        }
    }
    assert_eq!(warmup.len() as u64, warmup_n, "enough unassigned warm-up tiles");

    // The browser attests each unassigned tile (recorded by the high-rep submitter
    // so the browser is provably unassigned). Each valid attestation bumps its rep
    // by 1; none confirms (sub-trusted, lone, unassigned), but it accrues standing.
    for &(frame, idx) in &warmup {
        let cov = Coverage { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
        eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov).unwrap()), 1000);
        let att = Attestation { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
        eng.apply(&signed(proto::ATTEST, &browser, 1000, serde_json::to_value(&att).unwrap()), 1000);
    }
    assert_eq!(eng.coverage(&id), 0, "no warm-up tile confirmed (browser sub-trusted + unassigned)");
    assert_eq!(
        eng.reputation_of(&browser_pub),
        TRUSTED_ATTESTOR_REP - 1,
        "the browser is exactly one short of the trusted bar after warm-up"
    );

    // Now its attestation of a FRESH tile (one NOT in the warm-up set, so it isn't
    // deduped) confirms it alone via path (a): this attestation bumps the browser's
    // rep from 31 to TRUSTED_ATTESTOR_REP (32) BEFORE the confirm test, crossing the
    // bar. We pick a fresh unassigned tile distinct from every warm-up tile.
    let cov_before = eng.coverage(&id);
    let (ff, fi) = {
        let used: std::collections::HashSet<(u32, u32)> = warmup.iter().copied().collect();
        let mut found = None;
        'f: for idx in 0..IDXS_PER_FRAME {
            for frame in 0..N_FRAMES {
                if !used.contains(&(frame, idx))
                    && !assigned_to_audit(&browser_pub, (id.as_str(), frame, idx, 0), SPARSE_SUBMITTER_REP, DEFAULT_ROUND_SALT)
                {
                    found = Some((frame, idx));
                    break 'f;
                }
            }
        }
        found.expect("a fresh unassigned tile outside the warm-up set")
    };
    let fresh_sub = key(42);
    eng.grant_rep(pub_hex(&fresh_sub), SPARSE_SUBMITTER_REP);
    let cov = Coverage { sheep_id: id.clone(), frame: ff, idx: fi, pass: 0, hash: h.clone() };
    eng.apply(&signed(proto::PROGRESS, &fresh_sub, 1000, serde_json::to_value(&cov).unwrap()), 1000);
    let att = Attestation { sheep_id: id.clone(), frame: ff, idx: fi, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &browser, 1000, serde_json::to_value(&att).unwrap()), 1000));
    assert_eq!(
        eng.coverage(&id),
        cov_before + 1,
        "once trusted, the browser's attestation confirms a tile (bootstrap closed)"
    );
}

/// A slashed attestor (caught on a honeypot) no longer counts toward the §6
/// confirmation quorum, even if it previously attested the tile. **Posture note:**
/// the target tile is recorded by a HIGH-REP submitter and chosen UNASSIGNED for
/// both attestors, so neither confirms via the optimistic assigned path (b) — only
/// the rep-sum quorum (c) could, and the slash must keep it from completing.
#[test]
fn slashed_assigned_attestor_excluded() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));
    let h = "00".repeat(32);

    let good = key(30);
    let liar = key(31);
    // A high-rep submitter records the target tile (rate → floor) and we pick a
    // tile UNASSIGNED to both attestors so the assigned path (b) can't fire.
    let submitter = key(29);
    eng.grant_rep(pub_hex(&submitter), SPARSE_SUBMITTER_REP);
    let (frame, idx) = unassigned_for_both(&pub_hex(&good), &pub_hex(&liar), &id);
    assert!((frame, idx) != (7, 7), "target tile must differ from the honeypot tile (7,7)");
    let cov = Coverage { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov).unwrap()), 1000));

    // One established sub-trusted attestor (below the bar) attests the tile —
    // not enough alone (lone, unassigned).
    eng.grant_rep(pub_hex(&good), CONFIRM_QUORUM_REP_SUM); // sub-trusted? ensure < 32
    assert!(CONFIRM_QUORUM_REP_SUM < TRUSTED_ATTESTOR_REP);
    let a_good = Attestation { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &good, 1000, serde_json::to_value(&a_good).unwrap()), 1000));
    // rep(good) == CONFIRM_QUORUM_REP_SUM after the +1 bump; but it's a LONE
    // attestor (count 1 < quorum) so the tile is not yet confirmed.
    assert_eq!(eng.coverage(&id), 0, "one sub-trusted unassigned attestor is not a quorum");

    // A second peer would normally complete the quorum — but this one gets
    // SLASHED first (caught lying on a honeypot), so it must NOT count.
    let truth = eng.plant_honeypot(&id, 7, 7, 0).expect("plant honeypot");
    assert_ne!(truth, "ff".repeat(32));
    eng.grant_rep(pub_hex(&liar), CONFIRM_QUORUM_REP_SUM);
    // The liar attests the honeypot with a WRONG hash → slashed.
    let hp = Attestation { sheep_id: id.clone(), frame: 7, idx: 7, pass: 0, hash: "ff".repeat(32) };
    assert!(eng.apply(&signed(proto::ATTEST, &liar, 1000, serde_json::to_value(&hp).unwrap()), 1000));
    assert!(eng.slashed().contains(&pub_hex(&liar)), "liar slashed on the honeypot");

    // Now the slashed liar ALSO attests the original tile. Even though count would
    // be 2 and the naive rep-sum would clear the threshold, the slashed key is
    // discarded from the valid-attestor set → still not confirmed.
    let a_liar = Attestation { sheep_id: id.clone(), frame, idx, pass: 0, hash: h.clone() };
    // (apply may return false because the key is now banned/slashed and rejected
    // wholesale — either way it must not push the tile to confirmed.)
    let _ = eng.apply(&signed(proto::ATTEST, &liar, 2000, serde_json::to_value(&a_liar).unwrap()), 2000);
    assert_eq!(
        eng.coverage(&id), 0,
        "a slashed attestor does not count toward the confirmation quorum"
    );
}

/// **The scaling fix.** A rep-0 attestor that is neither the local node, nor
/// trusted, nor the submitter, but IS assigned by the §6 audit lottery, confirms a
/// tile immediately (1:1 audit↔render). The SAME key, on a tile it is NOT assigned
/// to, does NOT confirm — so confirmation tracks assignment, not standing.
#[test]
fn assigned_auditor_confirms_zero_rep() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));
    let h = "00".repeat(32);

    // The attestor: a fresh rep-0 key, not the local node, not trusted.
    let auditor = key(70);
    let auditor_pub = pub_hex(&auditor);
    assert_ne!(auditor_pub, *eng.self_pub());
    assert_eq!(eng.reputation_of(&auditor_pub), 0, "auditor has no earned standing");

    // --- POSITIVE: assigned tile, REP-0 submitter → assigned ~always → confirms ---
    // A distinct rep-0 submitter records the tile (so A != S holds and the lottery
    // runs at rate ~1.0). Pick a tile the auditor IS assigned to (rep-0 submitter).
    let submitter = key(71);
    let (af, ai) = assigned_tile_rep0(&auditor_pub, &id);
    assert!(
        eng.is_assigned(&auditor_pub, (id.as_str(), af, ai, 0), &pub_hex(&submitter)),
        "auditor is assigned to the positive tile (rep-0 submitter)"
    );
    let cov = Coverage { sheep_id: id.clone(), frame: af, idx: ai, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &submitter, 1000, serde_json::to_value(&cov).unwrap()), 1000));
    let att = Attestation { sheep_id: id.clone(), frame: af, idx: ai, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::ATTEST, &auditor, 1000, serde_json::to_value(&att).unwrap()), 1000));
    assert_eq!(
        eng.coverage(&id), 1,
        "an assigned, non-submitter rep-0 auditor confirms the tile immediately (the scaling fix)"
    );

    // --- NEGATIVE: a tile the auditor is NOT assigned to → no confirm. Use a
    // HIGH-REP submitter (rate → floor) so a NOT-assigned tile exists for this key. ---
    let hi_submitter = key(72);
    eng.grant_rep(pub_hex(&hi_submitter), SPARSE_SUBMITTER_REP);
    let (uf, ui) = unassigned_tile(&auditor_pub, &id);
    assert!(
        !eng.is_assigned(&auditor_pub, (id.as_str(), uf, ui, 0), &pub_hex(&hi_submitter)),
        "auditor is NOT assigned to the negative tile (high-rep submitter)"
    );
    let cov2 = Coverage { sheep_id: id.clone(), frame: uf, idx: ui, pass: 0, hash: h.clone() };
    assert!(eng.apply(&signed(proto::PROGRESS, &hi_submitter, 1000, serde_json::to_value(&cov2).unwrap()), 1000));
    let cov_before = eng.coverage(&id);
    let att2 = Attestation { sheep_id: id.clone(), frame: uf, idx: ui, pass: 0, hash: h.clone() };
    eng.apply(&signed(proto::ATTEST, &auditor, 1000, serde_json::to_value(&att2).unwrap()), 1000);
    assert_eq!(
        eng.coverage(&id), cov_before,
        "a NOT-assigned rep-0 auditor does not confirm the tile (confirmation tracks assignment)"
    );
}
