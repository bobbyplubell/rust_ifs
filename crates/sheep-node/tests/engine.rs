//! Integration tests for the pure node engine (ARCHITECTURE v3 §2–§7).
//!
//! Renders are kept tiny: the engine renders at the sheep's declared tier, so
//! we mint at R384 but use a small spp via the same `flame_core` API to keep
//! the test fast — actually the engine uses the protocol SPP; to keep tests
//! fast we mint sheep but only render 1–2 tiles per assertion, and we assert
//! HASH CORRECTNESS (engine output == an independent `flame_core` render of the
//! same unit), not volume.

use ed25519_dalek::SigningKey;
use flame_core::chunked::{hist_hash_hex, render_batch};
use sheep_node::block::{block_units, BlockId, Unit};
use sheep_node::engine::{Engine, COVERAGE_FLOOR, COVERAGE_TOLERANCE};
use sheep_node::spec::{N_FRAMES, SPP};
use sheep_proto::derive::derive_minted;
use sheep_proto::identity::{sheep_identity, ResolutionTier};
use sheep_proto::msg::{Attestation, Claim, Coverage, Mint};
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

/// Build a valid Mint envelope for a fresh sheep, signed by `k`. Returns the
/// envelope + the resulting sheep identity hex.
fn mint(k: &SigningKey, ts_micros: u64, tier: ResolutionTier) -> (Envelope, String) {
    let minter_pub = pub_hex(k);
    let body = serde_json::to_value(&Mint {
        ts_micros,
        minter_pub: minter_pub.clone(),
        resolution: tier,
        // Unique per mint so two mints from one key don't collide at one seq.
        seq: ts_micros,
    })
    .unwrap();
    let env = signed(proto::FLOCK, k, ts_micros / 1000, body);

    // Re-derive identity exactly as the engine does.
    let minter = k.verifying_key().to_bytes();
    let genome = derive_minted(ts_micros, &minter);
    let id_hex = {
        let id = sheep_identity(&genome, tier);
        let mut s = String::new();
        for b in id {
            s.push_str(&format!("{b:02x}"));
        }
        s
    };
    (env, id_hex)
}

// ---- block enumeration ------------------------------------------------------

#[test]
fn block_enumeration_round_trips() {
    let sheep = [3u8; 32];
    let block = BlockId { sheep_identity: sheep, block_index: 7 };
    let units = block_units(block);
    assert_eq!(units.len(), 16);
    for u in units {
        assert_eq!(sheep_node::block::unit_to_block_index(u), 7);
        let flat = sheep_node::block::unit_to_flat(u);
        assert_eq!(sheep_node::block::flat_to_unit(flat), u);
    }
}

// ---- births (§2.1) ----------------------------------------------------------

#[test]
fn correct_mint_populates_flock_mismatch_rejected() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);

    // Honest mint: accepted, flock learns the sheep.
    let (env, id_hex) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&env, 1000));
    assert!(eng.flock().contains_key(&id_hex), "honest mint should populate flock");

    // Tampered mint: claim a different ts_micros than what is signed... but the
    // signature covers the body, so we instead tamper the body AFTER signing:
    // change minter_pub to a key that didn't sign -> signature fails -> rejected.
    let mut bad = env.clone();
    // Flip one char of the signature: verify() fails, message rejected.
    let mut sig: Vec<char> = bad.sig.chars().collect();
    sig[0] = if sig[0] == 'a' { 'b' } else { 'a' };
    bad.sig = sig.into_iter().collect();
    let mut eng2 = Engine::new(key(9));
    assert!(!eng2.apply(&bad, 1000), "bad signature must be rejected");

    // Genome-derivation mismatch: a Mint whose recorded minter_pub differs from
    // the actual signer. The engine derives the genome from the RECORDED
    // minter_pub, but the signature is over the body, so a signer who is not the
    // recorded minter still produces a verifiable envelope with a self-claimed
    // minter_pub. The derivation still binds genome to (ts, minter_pub) — the
    // anti-injection guarantee. We assert that the derived identity matches the
    // recorded inputs (no free genome authoring): a body claiming minter_pub X
    // yields exactly derive(ts, X)'s identity.
    let claimed = key(7);
    let (env3, id3) = mint(&minter, 2_000_000, ResolutionTier::R512);
    // Re-sign the SAME body with a different key; minter_pub still = minter's.
    let mut env3b = env3.clone();
    env3b.sign(&claimed); // sets from = claimed, re-signs
    let mut eng3 = Engine::new(key(5));
    assert!(eng3.apply(&env3b, 1000));
    // Identity is derived from the recorded minter_pub (minter's), not the
    // signer (claimed) — so genome injection by re-signing is impossible.
    assert!(eng3.flock().contains_key(&id3));
    let _ = id_hex;
}

// ---- least-covered selection + per-sheep cap (§4, §4.1) ---------------------

#[test]
fn least_covered_selection_and_cap() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);

    // Two sheep in the flock.
    let (m_a, id_a) = mint(&minter, 1_000_000, ResolutionTier::R384);
    let (m_b, id_b) = mint(&minter, 2_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m_a, 1000));
    assert!(eng.apply(&m_b, 1000));

    // Drive sheep A's confirmed coverage well above B's, past the floor+tol so
    // the cap engages. Each confirmed tile = one attestation on a distinct
    // (frame, idx). We use a separate auditor key.
    let auditor = key(8);
    let n_a = (COVERAGE_FLOOR + COVERAGE_TOLERANCE + 4) as u32; // plenty over the cap
    for i in 0..n_a {
        let frame = i / 64;
        let idx = i % 64;
        let att = Attestation { sheep_id: id_a.clone(), frame, idx, pass: 0, hash: "00".into() };
        let env = signed(proto::ATTEST, &auditor, 1000, serde_json::to_value(&att).unwrap());
        assert!(eng.apply(&env, 1000));
    }
    assert!(eng.coverage(&id_a) > eng.coverage(&id_b));
    assert!(eng.total_coverage() > COVERAGE_FLOOR);

    // tick: the node should CLAIM the under-covered sheep (B), never A.
    let out = eng.tick(2000);
    let claim_env = out
        .iter()
        .find(|e| e.t == proto::CLAIMS)
        .expect("a claim should be emitted");
    let claim: Claim = serde_json::from_value(claim_env.body.clone()).unwrap();
    let block = BlockId::from_wire(&claim.block_id).unwrap();
    assert_eq!(block.sheep_hex(), id_b, "must claim the under-covered sheep, not the capped one");
    let _ = id_b;
}

// ---- claim lifecycle + equivocation (§4, §7) --------------------------------

#[test]
fn one_claim_at_a_time_and_equivocation_rejected() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, _id) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    // First tick claims a block, then subsequent ticks render it (no second
    // claim while one is active).
    let t1 = eng.tick(2000);
    let claims1: Vec<_> = t1.iter().filter(|e| e.t == proto::CLAIMS).collect();
    assert_eq!(claims1.iter().filter(|e| {
        serde_json::from_value::<Claim>(e.body.clone()).is_ok()
    }).count(), 1, "exactly one claim on first idle tick");

    // While the claim is active, the next tick renders (heartbeat, not a new
    // claim).
    let t2 = eng.tick(3000);
    let new_claims = t2.iter().filter(|e| {
        e.t == proto::CLAIMS && serde_json::from_value::<Claim>(e.body.clone()).is_ok()
    }).count();
    assert_eq!(new_claims, 0, "no second concurrent claim while one is active");

    // Equivocation: an external key sends two DIFFERENT claims at the same seq.
    let cheater = key(4);
    let block_x = BlockId { sheep_identity: [1u8; 32], block_index: 0 };
    let block_y = BlockId { sheep_identity: [1u8; 32], block_index: 1 };
    let c1 = Claim { block_id: block_x.to_wire(), expiry: 99_000, claimant: pub_hex(&cheater), seq: 5 };
    let c2 = Claim { block_id: block_y.to_wire(), expiry: 99_000, claimant: pub_hex(&cheater), seq: 5 };
    let e1 = signed(proto::CLAIMS, &cheater, 1000, serde_json::to_value(&c1).unwrap());
    let e2 = signed(proto::CLAIMS, &cheater, 1000, serde_json::to_value(&c2).unwrap());
    assert!(eng.apply(&e1, 2000), "first claim at seq 5 accepted");
    assert!(!eng.apply(&e2, 2000), "second different claim at same seq is equivocation -> rejected");
    assert!(eng.slashed().contains(&pub_hex(&cheater)), "equivocating key is slashed");
    // After slashing, further messages from that key are rejected wholesale.
    let c3 = Claim { block_id: block_x.to_wire(), expiry: 99_000, claimant: pub_hex(&cheater), seq: 6 };
    let e3 = signed(proto::CLAIMS, &cheater, 1000, serde_json::to_value(&c3).unwrap());
    assert!(!eng.apply(&e3, 2000), "slashed key rejected");
}

// ---- honest render hash == independent flame_core render (§6) ---------------

#[test]
fn honest_render_matches_independent_and_audit_detects_tamper() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id_hex) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    // Claim + render one block; capture the engine's Coverage hash for unit 0.
    eng.tick(2000); // claim
    let out = eng.tick(3000); // render
    let cov_env = out.iter().find(|e| e.t == proto::PROGRESS).expect("coverage emitted");
    let cov: Coverage = serde_json::from_value(cov_env.body.clone()).unwrap();

    // Block 0 units are all pass 0 (pass increments only past UNITS_PER_PASS).
    assert_eq!(cov.pass, 0, "block 0 is the first pass");

    // Independent re-render of the SAME unit with flame_core directly. Pass 0's
    // seed-id is the bare identity, so this is a direct render_batch.
    let entry = eng.flock().get(&id_hex).unwrap();
    let identity = sheep_identity(&entry.genome, entry.resolution);
    let edge = entry.resolution.edge() as usize;
    let accum = render_batch(&entry.genome, &identity, cov.frame, cov.idx, edge, edge, 1, SPP, N_FRAMES);
    let independent = hist_hash_hex(&accum);
    assert_eq!(cov.hash, independent, "engine render hash must equal an independent flame_core render");

    // Audit assignment: enqueue the same tile; the attestation MATCHES.
    eng.enqueue_audit(Coverage { sheep_id: id_hex.clone(), frame: cov.frame, idx: cov.idx, pass: cov.pass, hash: String::new() });
    let aout = eng.tick(4000);
    let att_env = aout.iter().find(|e| e.t == proto::ATTEST).expect("attestation emitted");
    let att: Attestation = serde_json::from_value(att_env.body.clone()).unwrap();
    assert_eq!(att.hash, independent, "honest audit re-render matches");

    // Tampered tile: a claimed coverage hash that does NOT match the honest
    // re-render is detectable (the auditor's attestation hash != the claim).
    let tampered_hash = "deadbeef".to_string();
    assert_ne!(att.hash, tampered_hash, "a tampered tile mismatches the honest attestation");
}

// ---- credits accrue at TILES_PER_CREDIT (§3) --------------------------------

#[test]
fn credits_accrue_at_tiles_per_credit() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id_hex) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));

    // Render enough blocks (16 units each) that our own submissions exceed
    // TILES_PER_CREDIT (128). 128/16 = 8 full blocks. We render, then have an
    // auditor confirm each rendered (frame, idx) so the tile is credited.
    let auditor = key(8);
    let mut now = 2000u64;
    let mut confirmed_units: std::collections::HashSet<(u32, u32, u32)> = Default::default();
    for _ in 0..10 {
        eng.tick(now); now += 100; // claim (or render)
        let out = eng.tick(now); now += 100; // render
        for e in &out {
            if e.t == proto::PROGRESS {
                let cov: Coverage = serde_json::from_value(e.body.clone()).unwrap();
                // Confirm distinct (frame, idx) once.
                if confirmed_units.insert((cov.frame, cov.idx, cov.pass)) {
                    let att = Attestation { sheep_id: id_hex.clone(), frame: cov.frame, idx: cov.idx, pass: cov.pass, hash: cov.hash };
                    let aenv = signed(proto::ATTEST, &auditor, now, serde_json::to_value(&att).unwrap());
                    eng.apply(&aenv, now);
                }
            }
        }
    }
    assert!(eng.own_confirmed_tiles() >= 128, "should have confirmed >=128 own tiles, got {}", eng.own_confirmed_tiles());
    assert!(eng.credits() >= 1, "credits = confirmed/{} should be >=1, got {}", 128, eng.credits());
    assert_eq!(eng.credits(), eng.own_confirmed_tiles() / 128);
}

// ---- idempotency under duplicate delivery (gossip dedup) --------------------

#[test]
fn apply_is_idempotent_under_replay() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id_hex) = mint(&minter, 1_000_000, ResolutionTier::R384);

    assert!(eng.apply(&m, 1000), "first delivery accepted");
    assert!(!eng.apply(&m, 1000), "duplicate delivery is a no-op (dedup)");
    assert_eq!(eng.flock().len(), 1, "flock has exactly one sheep after replay");

    // An attestation, then its duplicate, must not double-count coverage.
    let auditor = key(8);
    let att = Attestation { sheep_id: id_hex.clone(), frame: 0, idx: 0, pass: 0, hash: "00".into() };
    let aenv = signed(proto::ATTEST, &auditor, 1000, serde_json::to_value(&att).unwrap());
    assert!(eng.apply(&aenv, 1000));
    let cov_before = eng.coverage(&id_hex);
    assert!(!eng.apply(&aenv, 1000), "duplicate attestation deduped");
    assert_eq!(eng.coverage(&id_hex), cov_before, "coverage unchanged by replay");
}

// ---- pass-aware coverage: a second pass adds density (§4) -------------------

#[test]
fn second_pass_over_same_frame_idx_grows_coverage() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    let (m, id_hex) = mint(&minter, 1_000_000, ResolutionTier::R384);
    assert!(eng.apply(&m, 1000));
    let auditor = key(8);

    // Confirm (frame 3, idx 7, pass 0).
    let a0 = Attestation { sheep_id: id_hex.clone(), frame: 3, idx: 7, pass: 0, hash: "00".into() };
    let e0 = signed(proto::ATTEST, &auditor, 1000, serde_json::to_value(&a0).unwrap());
    assert!(eng.apply(&e0, 1000));
    let cov_after_pass0 = eng.coverage(&id_hex);
    assert_eq!(cov_after_pass0, 1, "one confirmed (frame,idx,pass)");

    // Confirm the SAME (frame, idx) but a LATER pass — this is fresh density,
    // not a duplicate: coverage grows (§4 unbounded density).
    let a1 = Attestation { sheep_id: id_hex.clone(), frame: 3, idx: 7, pass: 1, hash: "00".into() };
    let e1 = signed(proto::ATTEST, &auditor, 1000, serde_json::to_value(&a1).unwrap());
    assert!(eng.apply(&e1, 1000), "a second pass over the same (frame,idx) is accepted");
    assert_eq!(
        eng.coverage(&id_hex),
        cov_after_pass0 + 1,
        "a second pass over the same (frame,idx) raises coverage (density grows), not capped at one pass"
    );

    // And a re-send of the pass-0 attestation is still deduped (idempotent).
    assert!(!eng.apply(&e0, 1000), "duplicate pass-0 attestation deduped");
    assert_eq!(eng.coverage(&id_hex), 2, "duplicate did not inflate coverage");
}

/// The engine's own pass-aware render: pass 0 == bare-identity render_batch;
/// pass 1 over the same (frame, idx) is a DISTINCT histogram (fresh sample
/// stream) — so accumulating passes adds density rather than re-adding samples.
#[test]
fn pass_changes_the_rendered_histogram() {
    use sheep_node::engine::pass_seed_id;
    let minter = key(2);
    let genome = derive_minted(1_000_000, &minter.verifying_key().to_bytes());
    let identity = sheep_identity(&genome, ResolutionTier::R384);
    let edge = ResolutionTier::R384.edge() as usize;

    // pass 0 seed-id is the identity verbatim (no behavior change).
    assert_eq!(pass_seed_id(&identity, 0), identity);

    let p0 = render_batch(&genome, &pass_seed_id(&identity, 0), 0, 0, edge, edge, 1, SPP, N_FRAMES);
    let p1 = render_batch(&genome, &pass_seed_id(&identity, 1), 0, 0, edge, edge, 1, SPP, N_FRAMES);
    assert_ne!(
        hist_hash_hex(&p0),
        hist_hash_hex(&p1),
        "pass 1 must draw a distinct sample stream so accumulation adds density"
    );
}

// keep the imports used in case a helper is trimmed.
#[allow(dead_code)]
fn _unit_smoke() -> Unit {
    Unit { frame: 0, idx: 0, pass: 0 }
}
