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
    assigned_to_audit, sample_rate, Engine, DEFAULT_ROUND_SALT, SAMPLE_FLOOR, TRUST_REP,
};
use sheep_node::spec::{N_FRAMES, SPP};
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

    // Zero-rep → audited ~everything (sample_rate(0) == 1.0).
    assert!(
        low as f64 > 0.95 * total as f64,
        "zero-rep submitter audited near-fully: {low}/{total}"
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
    assert!((sample_rate(0) - 1.0).abs() < 1e-9, "zero rep → audit everything");
    assert!(
        (sample_rate(TRUST_REP) - 0.5).abs() < 1e-9,
        "rep == TRUST_REP → 50%"
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
    let submitter = key(3);
    let auditor = key(4);
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
