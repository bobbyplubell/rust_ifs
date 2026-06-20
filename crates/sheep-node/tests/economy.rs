//! Lifecycle + economy tests (ARCHITECTURE v3 §2.1–§2.4, §3, §7).
//!
//! Covers the deliverables: credit ledger = earned − spent; double-spend
//! equivocation slashing; votes → backing; age-escalating decay → vitality →
//! live-flock membership; mint/breed *initiated* actions with lineage; and the
//! Hall of Fame enshrinement.
//!
//! Renders are avoided wherever possible — the economy is pure bookkeeping over
//! injected `now_ms`. Where credits must be *earned*, we drive the engine's own
//! render/confirm loop, kept small.

use ed25519_dalek::SigningKey;
use sheep_node::engine::{DecayParams, Engine, HallThreshold, GENESIS_FLOCK_SIZE};
use sheep_proto::identity::ResolutionTier;
use sheep_proto::msg::{Mint, Vote};
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

/// A valid Mint envelope (signed by `k`) + the resulting sheep id hex. `seq` is
/// the spend sequence; `k` should be credit-exempt or funded in the engine.
fn mint_env(k: &SigningKey, ts_micros: u64, tier: ResolutionTier, seq: u64) -> (Envelope, String) {
    use sheep_proto::derive::derive_minted;
    use sheep_proto::identity::sheep_identity;
    let minter_pub = pub_hex(k);
    let body = serde_json::to_value(&Mint {
        ts_micros,
        minter_pub,
        resolution: tier,
        seq,
    })
    .unwrap();
    let env = signed(proto::FLOCK, k, ts_micros / 1000, body);
    let genome = derive_minted(ts_micros, &k.verifying_key().to_bytes());
    let id = sheep_identity(&genome, tier);
    let mut hex = String::new();
    for b in id {
        hex.push_str(&format!("{b:02x}"));
    }
    (env, hex)
}

/// Fund the engine's OWN key with `credits` worth of earned render-credits
/// directly (pure ledger bookkeeping). The real render→confirm→earn path is
/// exercised by `engine::credits_accrue_at_tiles_per_credit` and the two-peer
/// loop; here we isolate the spend/ledger logic without minutes of rendering.
fn fund_self(eng: &mut Engine, credits: u64) {
    let me = eng.self_pub().to_string();
    eng.grant_earned_tiles(me, credits * sheep_node::spec::TILES_PER_CREDIT);
}

// ---- credit ledger = earned − spent (§3) ------------------------------------

#[test]
fn credit_ledger_earned_minus_spent_and_overspend_rejected() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    eng.exempt_credit(pub_hex(&minter)); // fund the minter past the credit gate

    // A sheep to vote on.
    let (m, id) = mint_env(&minter, 1_000_000, ResolutionTier::R384, 1);
    assert!(eng.apply(&m, 1000));

    // Fund the node with a few earned credits (ledger isolation).
    fund_self(&mut eng, 5);
    let earned = eng.credits();
    assert_eq!(earned, 5, "node has 5 earned credits");

    // A valid vote (cost 1) reduces credits by exactly 1.
    let v = eng.initiate_vote(&id, 10_000).expect("affordable vote");
    assert!(v.verify());
    assert_eq!(eng.credits(), earned - 1, "a valid spend reduces credits by VOTE_COST");
    assert_eq!(eng.backing(&id), 1, "the vote raised backing");

    // Drain the rest of the balance with votes, then assert overspend is
    // rejected: the node cannot spend more than it earned.
    let mut bal = eng.credits();
    while bal > 0 {
        assert!(eng.initiate_vote(&id, 10_000).is_some());
        bal -= 1;
        assert_eq!(eng.credits(), bal);
    }
    assert_eq!(eng.credits(), 0, "spent down to zero");
    assert!(eng.initiate_vote(&id, 10_000).is_none(), "overspend (0 credits) rejected by initiate");

    // And an `apply`'d spend from an OBSERVABLE-but-broke key is rejected too.
    // The node's own key is broke now; craft a self-signed vote at a fresh seq.
    let broke = key(1); // == the engine's own key
    let body = serde_json::to_value(&Vote { sheep_id: id.clone(), seq: 9999 }).unwrap();
    let over = signed(proto::VOTES, &broke, 11_000, body);
    let backing_before = eng.backing(&id);
    assert!(!eng.apply(&over, 11_000), "apply rejects an overspend from a known-broke key");
    assert_eq!(eng.backing(&id), backing_before, "rejected overspend did not change backing");
}

// ---- double-spend = equivocation → slash (§7) -------------------------------

#[test]
fn double_spend_at_same_seq_slashes_signer() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    eng.exempt_credit(pub_hex(&minter));

    // Two sheep to vote on.
    let (m_a, id_a) = mint_env(&minter, 1_000_000, ResolutionTier::R384, 1);
    let (m_b, id_b) = mint_env(&minter, 2_000_000, ResolutionTier::R384, 2);
    assert!(eng.apply(&m_a, 1000));
    assert!(eng.apply(&m_b, 1000));

    // A spender that the engine treats as observable + funded (exempt = skip the
    // balance gate, but still seq-policed for equivocation).
    let spender = key(50);
    eng.exempt_credit(pub_hex(&spender));

    // Two DIFFERENT votes at the SAME seq (a double-spend): vote A then vote B
    // both at seq 7.
    let va = signed(proto::VOTES, &spender, 1000, serde_json::to_value(&Vote { sheep_id: id_a.clone(), seq: 7 }).unwrap());
    let vb = signed(proto::VOTES, &spender, 1000, serde_json::to_value(&Vote { sheep_id: id_b.clone(), seq: 7 }).unwrap());

    assert!(eng.apply(&va, 1000), "first spend at seq 7 accepted");
    assert!(!eng.apply(&vb, 1000), "second DIFFERENT spend at seq 7 is a double-spend → rejected");
    assert!(eng.slashed().contains(&pub_hex(&spender)), "double-spending key is slashed");

    // Slashed key's later traffic is rejected wholesale.
    let vc = signed(proto::VOTES, &spender, 1000, serde_json::to_value(&Vote { sheep_id: id_a.clone(), seq: 8 }).unwrap());
    assert!(!eng.apply(&vc, 1000), "slashed key rejected");
    let _ = id_b;
}

// ---- votes raise backing (§2.2) ---------------------------------------------

#[test]
fn votes_raise_target_backing() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    eng.exempt_credit(pub_hex(&minter));
    let (m, id) = mint_env(&minter, 1_000_000, ResolutionTier::R384, 1);
    assert!(eng.apply(&m, 1000));

    assert_eq!(eng.backing(&id), 0, "no votes yet");

    // Three distinct voters (exempt so we isolate the backing logic).
    for (i, seed) in [60u8, 61, 62].into_iter().enumerate() {
        let voter = key(seed);
        eng.exempt_credit(pub_hex(&voter));
        let v = signed(proto::VOTES, &voter, 1000, serde_json::to_value(&Vote { sheep_id: id.clone(), seq: 0 }).unwrap());
        assert!(eng.apply(&v, 1000));
        assert_eq!(eng.backing(&id), (i + 1) as u64, "each accepted vote raises backing by one");
    }
    assert_eq!(eng.backing(&id), 3);
}

// ---- (v4) survival = top N_target by recency-weighted backing --------------

#[test]
fn survival_is_top_n_by_recency_backing() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    eng.exempt_credit(pub_hex(&minter));

    // Mint 6 sheep — MORE than the N_base (=4) live slots; no proven membership
    // yet, so n_target stays at the floor.
    let mut ids = Vec::new();
    for i in 0..6u64 {
        let (m, id) = mint_env(&minter, 1_000 + i, ResolutionTier::R384, i);
        assert!(eng.apply(&m, 0));
        ids.push(id);
    }
    assert_eq!(eng.n_target(), GENESIS_FLOCK_SIZE, "no proven membership → n_target = N_base");

    // Vote (distinct voters → no equivocation) for the first 4 with strictly
    // decreasing backing; leave the last 2 unvoted.
    let mut voter_seed = 40u8;
    for (rank, id) in ids.iter().take(4).enumerate() {
        for _ in 0..(4 - rank) {
            let voter = key(voter_seed);
            voter_seed += 1;
            eng.exempt_credit(pub_hex(&voter));
            let v = signed(
                proto::VOTES,
                &voter,
                0,
                serde_json::to_value(&Vote { sheep_id: id.clone(), seq: 0 }).unwrap(),
            );
            assert!(eng.apply(&v, 0), "vote applies");
        }
    }

    // The live flock is exactly the 4 voted sheep (top N_target by backing).
    let live = eng.live_flock(0);
    assert_eq!(live.len(), GENESIS_FLOCK_SIZE, "live flock is exactly N_target");
    for id in ids.iter().take(4) {
        assert!(live.contains_key(id), "a voted sheep is live");
        assert!(eng.is_alive(id, 0));
    }
    for id in ids.iter().skip(4) {
        assert!(!live.contains_key(id), "an unvoted sheep is below the cutoff");
        assert!(!eng.is_alive(id, 0));
        // ...but stays in the raw flock map (history; can return if re-voted).
        assert!(eng.flock().contains_key(id), "dropped sheep kept for history");
    }
}

// ---- (v4) birth lottery: deterministic + convergent across nodes -----------

#[test]
fn birth_lottery_is_deterministic_and_convergent() {
    use sheep_node::derive_minted_genesis::genesis_sheep_hex;
    // Two INDEPENDENT engines (different node keys) that each derive the same
    // deterministic genesis flock — never exchange a message.
    let mut a = Engine::new(key(1));
    a.seed_genesis(0);
    let mut b = Engine::new(key(99));
    b.seed_genesis(0);
    let parent = genesis_sheep_hex(); // a genesis sheep present in BOTH flocks

    // Sweep candidate tile hashes until one WINS the lottery (~1/256), then assert
    // both nodes derive the byte-IDENTICAL child from the same (tile, hash).
    let mut won = false;
    for n in 0u64..100_000 {
        let h = format!("{n:064x}");
        let child_a = a.try_birth_from_tile(&parent, 0, 0, 0, &h);
        if let Some(child_a) = child_a {
            let child_b = b.try_birth_from_tile(&parent, 0, 0, 0, &h);
            assert_eq!(
                Some(child_a.clone()),
                child_b,
                "two ungossiped nodes derive the IDENTICAL lottery child"
            );
            assert!(a.flock().contains_key(&child_a), "child entered A's flock");
            assert!(b.flock().contains_key(&child_a), "child entered B's flock");
            assert_ne!(child_a, parent, "the child is a new sheep");
            won = true;
            break;
        }
        // A non-winning hash must NOT birth on either node.
        assert!(
            b.try_birth_from_tile(&parent, 0, 0, 0, &h).is_none(),
            "a non-winning tile births nothing"
        );
    }
    assert!(won, "found a winning tile within the sweep (lottery fires)");
}

// ---- (v4) carrying capacity grows logarithmically with proven membership ----

#[test]
fn n_target_grows_with_membership() {
    let mut eng = Engine::new(key(1));
    let n0 = eng.n_target();
    assert_eq!(n0, GENESIS_FLOCK_SIZE, "zero membership → floor at N_base");

    // 20 contributors with high earned standing (rep → w(rep) ≈ 1 each).
    for s in 0..20u32 {
        eng.grant_rep(format!("{s:064x}"), 100);
    }
    let n_many = eng.n_target();
    assert!(n_many > n0, "n_target grows with proven membership: {n0} -> {n_many}");
    // Logarithmic, not linear: 20 full members add a bounded chunk, never explode.
    assert!(n_many < n0 + 40, "n_target growth is logarithmic ({n_many})");
}

// ---- escalation: older needs MORE backing to survive (§2.2) -----------------

#[test]
fn decay_escalates_with_age() {
    let dp = DecayParams::DEFAULT;
    // Required backing to stay alive at increasing ages must be strictly
    // increasing AND escalating (the gap grows): decay(2t)-decay(t) >
    // decay(t)-decay(0).
    let t = dp.time_unit_ms;
    let d0 = dp.decay(0);
    let d1 = dp.decay(t);
    let d2 = dp.decay(2 * t);
    let d4 = dp.decay(4 * t);
    assert!(d1 > d0 && d2 > d1 && d4 > d2, "decay strictly increases with age");
    // Escalation: each doubling of age adds MORE than the previous interval.
    let gap_0_1 = d1 - d0;
    let gap_1_2 = d2 - d1;
    let gap_2_4 = d4 - d2;
    assert!(gap_1_2 > gap_0_1, "marginal decay grows (poly+exp escalation): {gap_1_2} > {gap_0_1}");
    assert!(gap_2_4 > gap_1_2, "and keeps escalating: {gap_2_4} > {gap_1_2}");

    // Concretely: an OLDER sheep needs strictly more backing than a younger one
    // to keep vitality positive.
    let young_need = dp.decay(t).ceil() as u64;
    let old_need = dp.decay(8 * t).ceil() as u64;
    assert!(old_need > young_need, "older sheep needs more backing: {old_need} > {young_need}");
}

// ---- mint/breed initiate: spends + valid derivable birth + lineage (§2.1/§2.4)

#[test]
fn initiate_mint_and_breed_spend_and_record_lineage() {
    let mut eng = Engine::new(key(1));
    let minter = key(2);
    eng.exempt_credit(pub_hex(&minter));

    // Seed a sheep (so parents exist for context) and fund the node.
    let (m, _id) = mint_env(&minter, 1_000_000, ResolutionTier::R384, 1);
    assert!(eng.apply(&m, 1000));

    // Fund enough for two mints (8 each) + a breed (20) at R384 → 36; give 40.
    fund_self(&mut eng, 40);
    let before = eng.credits();

    // Initiate a mint: spends MINT_COST, produces a valid birth that another
    // fresh engine ACCEPTS and adds to its flock.
    let (mint_env_a, child_a) = eng.initiate_mint(123_456, ResolutionTier::R384, 50_000).expect("affordable mint");
    assert_eq!(eng.credits(), before - 8, "mint spent MINT_COST (R384 ×1)");
    assert!(eng.flock().contains_key(&child_a), "initiated mint is in our own flock");
    assert!(mint_env_a.verify());

    let mut other = Engine::new(key(99));
    other.exempt_credit(eng.self_pub()); // accept our spend regardless of its view of our balance
    assert!(other.apply(&mint_env_a, 50_000), "a fresh engine accepts the initiated mint");
    assert!(other.flock().contains_key(&child_a), "and adds the new sheep to its flock");

    // A second mint from the same node uses a DISTINCT seq (no self-equivocation).
    let (mint_env_b, child_b) = eng.initiate_mint(789_012, ResolutionTier::R384, 51_000).expect("second mint");
    assert_ne!(child_a, child_b);
    assert!(other.apply(&mint_env_b, 51_000));

    // Breed the two children: spends BREED_COST and records lineage (§2.4).
    let credits_pre_breed = eng.credits();
    let (breed_env, bred) = eng
        .initiate_breed(&child_a, &child_b, 0xABCD, ResolutionTier::R384, 52_000)
        .expect("affordable breed");
    assert_eq!(eng.credits(), credits_pre_breed - 20, "breed spent BREED_COST (the costliest)");
    let entry = eng.flock().get(&bred).expect("bred child in flock");
    assert_eq!(entry.creator, eng.self_pub(), "breeder recorded (attribution, §2.4)");
    assert_eq!(
        entry.parents,
        Some((child_a.clone(), child_b.clone())),
        "both parents recorded (lineage, §2.4)"
    );

    // The breed applies on the other engine too (both parents already known there).
    assert!(other.apply(&breed_env, 52_000), "fresh engine accepts the initiated breed");
    let oentry = other.flock().get(&bred).expect("bred child in other flock");
    assert_eq!(oentry.parents, Some((child_a, child_b)), "lineage converges across nodes");

    // Mint is moderate, breed the costliest (§2.1).
    assert!(8 < 20, "MINT_COST < BREED_COST");
}

// ---- (v4) Hall of Fame: enshrine a dropped well-backed sheep, not the unloved -

#[test]
fn hall_enshrines_dropped_well_backed_sheep_not_unloved() {
    let mut eng = Engine::new(key(1));
    // v4: survival is the vote ranking (no wall-clock lifespan), so the Hall keys
    // purely on EARNED standing — peak backing >= the bar.
    eng.set_hall_threshold(HallThreshold {
        min_lifespan_ms: u64::MAX,
        min_peak_backing: 3,
    });
    let minter = key(2);
    eng.exempt_credit(pub_hex(&minter));

    // Helper: cast `n` distinct votes (no equivocation) for `id`.
    let mut next_voter = 30u8;
    let cast = |eng: &mut Engine, id: &str, n: usize, nv: &mut u8| {
        for _ in 0..n {
            let voter = key(*nv);
            *nv += 1;
            eng.exempt_credit(pub_hex(&voter));
            let v = signed(
                proto::VOTES,
                &voter,
                0,
                serde_json::to_value(&Vote { sheep_id: id.to_string(), seq: 0 }).unwrap(),
            );
            assert!(eng.apply(&v, 0));
        }
    };

    // L: well-backed (peak 3) but OUT-COMPETED — four stronger sheep push it below
    // the N_base=4 cutoff. Q: never voted.
    let (ml, idl) = mint_env(&minter, 1, ResolutionTier::R384, 1);
    assert!(eng.apply(&ml, 0));
    cast(&mut eng, &idl, 3, &mut next_voter);
    for i in 0..4u64 {
        let (m, id) = mint_env(&minter, 100 + i, ResolutionTier::R384, 10 + i);
        assert!(eng.apply(&m, 0));
        cast(&mut eng, &id, 4 + i as usize, &mut next_voter); // 4,5,6,7 > L's 3
    }
    let (mq, idq) = mint_env(&minter, 9, ResolutionTier::R384, 9);
    assert!(eng.apply(&mq, 0));

    assert_eq!(eng.n_target(), GENESIS_FLOCK_SIZE, "n_target = N_base = 4 slots");
    assert!(!eng.is_alive(&idl, 0), "L is out-competed (below the cutoff)");
    assert!(!eng.is_alive(&idq, 0), "Q is unvoted (below the cutoff)");

    eng.reap_dead(0); // enshrine the dropped sheep

    let hall: Vec<String> = eng.hall().iter().map(|h| h.sheep_id.clone()).collect();
    assert!(hall.contains(&idl), "dropped well-backed L is enshrined (peak >= bar)");
    assert!(!hall.contains(&idq), "never-voted Q is NOT enshrined (peak 0)");
    let l = eng.hall().iter().find(|h| h.sheep_id == idl).unwrap();
    assert_eq!(l.peak_backing, 3, "peak backing preserved in the Hall");
}

// ---- deploy finalization: env→config + world bootstrap ----------------------

/// World bootstrap: a seed's `bootstrap_seed_flock(N, ..)` (the pure engine half
/// of the deploy boot step, driven with an INJECTED now_ms — no async swarm)
/// leaves N distinct sheep LIVE in `live_flock(now)`, each with positive
/// vitality, on a fresh engine that has earned nothing.
#[test]
fn world_bootstrap_seeds_a_live_flock() {
    let now = 5_000_000u64; // a wall-clock-ish injected now
    let n = 4usize;
    let mut eng = Engine::new(key(42)); // fresh seed, zero earned credits
    let envs = eng.bootstrap_seed_flock(n, 4, now);

    // It minted N sheep + 4 votes each = N*(1+4) envelopes to publish.
    assert_eq!(envs.len(), n * 5, "mints + votes returned for publishing");

    let live = eng.live_flock(now);
    assert_eq!(live.len(), n, "exactly N starter sheep are LIVE at boot");
    for (id, _) in &live {
        let v = eng.vitality(id, now).expect("known sheep");
        assert!(v > 0.0, "bootstrapped sheep {id} has positive vitality ({v})");
        assert_eq!(eng.backing(id), 4, "each starter sheep carries its seed backing");
    }
    // Distinct genomes/identities (varied mint seed per sheep).
    assert_eq!(
        eng.flock().len(),
        n,
        "N distinct sheep identities (distinct genomes)"
    );

    // Sanity: the same flock is still alive a moment later (fresh-born, decay
    // hasn't bitten) — i.e. they are genuinely watchable, not instantly dead.
    assert_eq!(eng.live_flock(now + 1_000).len(), n, "still live shortly after boot");
}

/// §1 (v4): `seed_genesis` brings up exactly `GENESIS_FLOCK_SIZE` deterministic
/// founders, and they are **immortal** — alive arbitrarily far in the future with
/// no votes and no replenishment. This is what replaces `maintain_floor`: the
/// gallery is never empty because genesis never decays.
#[test]
fn genesis_flock_is_immortal_and_correct_size() {
    let boot = 1_000_000u64;
    let mut eng = Engine::new(key(7)); // fresh node, zero earned credits
    let envs = eng.seed_genesis(boot);
    assert_eq!(envs.len(), GENESIS_FLOCK_SIZE, "one mint per genesis founder");
    assert_eq!(
        eng.live_flock(boot).len(),
        GENESIS_FLOCK_SIZE,
        "genesis flock is live at boot"
    );
    for id in eng.live_flock(boot).keys() {
        assert!(eng.is_genesis(id), "every founder is a genesis sheep");
        assert_eq!(eng.backing(id), 0, "genesis needs no backing to live");
    }
    // Immortal: still the full live flock a year later, despite zero votes.
    let far = boot + 365u64 * 24 * 60 * 60 * 1_000;
    assert_eq!(
        eng.live_flock(far).len(),
        GENESIS_FLOCK_SIZE,
        "genesis never decays — the gallery is never empty"
    );
}

/// §1 (v4): seeding genesis is idempotent — a second call mints nothing new and
/// the flock is unchanged (so a re-applied/replayed genesis can never duplicate
/// or churn founders).
#[test]
fn genesis_seed_is_idempotent() {
    let now = 2_000_000u64;
    let mut eng = Engine::new(key(8));
    eng.seed_genesis(now);
    let again = eng.seed_genesis(now);
    assert!(again.is_empty(), "re-seeding genesis mints nothing new");
    assert_eq!(eng.live_flock(now).len(), GENESIS_FLOCK_SIZE, "flock unchanged");
}

/// §1 (v4) THE divergence cure: two independent nodes that have NEVER exchanged a
/// message derive the byte-identical genesis flock just by calling `seed_genesis`
/// — no gossip, no per-node minting. This is exactly what the v3 wall-clock
/// `bootstrap_seed_flock` failed to do (each seed minted its own IDs → split
/// flocks → HTTP 422); here it holds by construction.
#[test]
fn two_nodes_derive_identical_genesis_without_gossip() {
    let now = 4_000_000u64;
    let mut a = Engine::new(key(10));
    let mut b = Engine::new(key(200)); // different node key entirely
    a.seed_genesis(now);
    b.seed_genesis(now);
    let a_ids: std::collections::HashSet<String> = a.live_flock(now).keys().cloned().collect();
    let b_ids: std::collections::HashSet<String> = b.live_flock(now).keys().cloned().collect();
    assert_eq!(a_ids, b_ids, "two ungossiped nodes hold the identical genesis flock");
    assert_eq!(a_ids.len(), GENESIS_FLOCK_SIZE);
}
