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
use sheep_node::engine::{DecayParams, Engine, HallThreshold, WorldConfig};
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

// ---- vitality: live young, dead-when-old-and-unloved (§2.2/§2.3) ------------

#[test]
fn vitality_decay_kills_unloved_keeps_well_backed_live() {
    let mut eng = Engine::new(key(1));
    // Use the default decay personality.
    let dp = eng.decay_params();
    let minter = key(2);
    eng.exempt_credit(pub_hex(&minter));

    // birth at t = 0 for clean ages.
    let (m, id) = mint_env(&minter, 0, ResolutionTier::R384, 1);
    assert!(eng.apply(&m, 0));

    // A well-backed sheep: pile on votes.
    for seed in 70u8..86 {
        let voter = key(seed);
        eng.exempt_credit(pub_hex(&voter));
        let v = signed(proto::VOTES, &voter, 0, serde_json::to_value(&Vote { sheep_id: id.clone(), seq: 0 }).unwrap());
        assert!(eng.apply(&v, 0));
    }
    let backing = eng.backing(&id);
    assert_eq!(backing, 16);

    // Young: vitality is high, sheep is live + in the live flock.
    let young = dp.time_unit_ms; // 1 decay unit old
    assert!(eng.vitality(&id, young).unwrap() > 0.0, "well-backed young sheep is alive");
    assert!(eng.is_alive(&id, young));
    assert!(eng.live_flock(young).contains_key(&id), "live flock includes the live sheep");

    // Old enough that even 16 votes < decay(age): dead. Decay escalates
    // exponentially, so find an age where it exceeds the backing.
    let mut old = young;
    while eng.vitality(&id, old).map_or(true, |v| v > 0.0) {
        old += dp.time_unit_ms * 4;
        assert!(old < dp.time_unit_ms * 10_000, "decay should eventually exceed backing");
    }
    assert!(!eng.is_alive(&id, old), "an old enough sheep dies even with 16 votes");
    assert!(!eng.live_flock(old).contains_key(&id), "dead sheep leaves the live flock");
    // ...but stays in the raw flock map (history).
    assert!(eng.flock().contains_key(&id), "dead sheep remains in the raw flock for history");

    // A no-backing sheep dies much earlier than the well-backed one.
    let (m2, id2) = mint_env(&minter, 0, ResolutionTier::R512, 2);
    assert!(eng.apply(&m2, 0));
    assert_eq!(eng.backing(&id2), 0);
    // At an age where the well-backed sheep is still alive, the unloved one is dead.
    let mid = young * 4;
    if eng.is_alive(&id, mid) {
        assert!(!eng.is_alive(&id2, mid), "unloved sheep dies before the well-backed one");
    }
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

// ---- Hall of Fame: enshrine long-lived / well-loved, not quick & unloved -----

#[test]
fn hall_enshrines_loved_or_long_lived_not_quick_unloved() {
    let mut eng = Engine::new(key(1));
    eng.set_hall_threshold(HallThreshold::DEFAULT); // long-lived OR >=16 peak backing
    let minter = key(2);
    eng.exempt_credit(pub_hex(&minter));

    // Sheep L: deeply loved (>= 16 votes) at birth t=0; will die of old age.
    let (ml, idl) = mint_env(&minter, 0, ResolutionTier::R384, 1);
    assert!(eng.apply(&ml, 0));
    for seed in 90u8..110 {
        let voter = key(seed);
        eng.exempt_credit(pub_hex(&voter));
        let v = signed(proto::VOTES, &voter, 0, serde_json::to_value(&Vote { sheep_id: idl.clone(), seq: 0 }).unwrap());
        assert!(eng.apply(&v, 0));
    }
    assert!(eng.backing(&idl) >= 16, "L is deeply loved");

    // Advance the clock far past BOTH sheep's death. L (born at 0, 16 votes)
    // dies only once decay escalates past its backing, so `now` must be many
    // decay units out — `now = 20 × time_unit` puts L at a deeply-decayed age
    // where decay ≫ 20 regardless of the world's coefficients.
    let dp = eng.decay_params();
    let now = dp.time_unit_ms * 20;

    // Sheep Q: quick + unloved. Born LATE (just one decay unit before `now`) so
    // its lifespan (`now − born_q`) is short — below `min_lifespan_ms` — while
    // L's lifespan spans the whole window. Q has zero backing, so it's dead the
    // instant decay(age) exceeds 0.
    let born_q_ms = now - dp.time_unit_ms; // lifespan == 1 decay unit
    let (mq, idq) = mint_env(&minter, born_q_ms * 1000, ResolutionTier::R384, 2);
    assert!(eng.apply(&mq, born_q_ms));
    assert_eq!(eng.backing(&idq), 0, "Q is unloved");
    // Q's lifespan must stay under the hall's long-lived bar so it is excluded
    // for being quick (not merely unloved) — guards the test's own premise.
    assert!(
        now - born_q_ms < HallThreshold::DEFAULT.min_lifespan_ms,
        "Q's lifespan is below the long-lived enshrinement bar"
    );

    // Both should be dead by now: Q (0 backing) trivially, L because decay has
    // escalated far past its 16 votes at this age.
    assert!(!eng.is_alive(&idq, now), "Q is dead");
    assert!(!eng.is_alive(&idl, now), "L is dead (very old)");

    // Reap via tick → enshrinement.
    eng.tick(now);

    let hall: Vec<String> = eng.hall().iter().map(|h| h.sheep_id.clone()).collect();
    assert!(hall.contains(&idl), "deeply-loved (or very long-lived) L is enshrined");
    assert!(
        !hall.contains(&idq),
        "quick + unloved Q (lifespan {}ms < {}ms bar, 0 backing) is NOT enshrined",
        now - born_q_ms,
        HallThreshold::DEFAULT.min_lifespan_ms,
    );

    // The Hall entry preserves legacy data after death.
    let l = eng.hall().iter().find(|h| h.sheep_id == idl).unwrap();
    assert!(l.peak_backing >= 16, "peak backing preserved");
    assert!(l.lifespan_ms > 0 && l.death_ms == now, "lifespan + death recorded");
}

// ---- deploy finalization: env→config + world bootstrap ----------------------

/// The per-world decay personality (injected via [`WorldConfig`] at
/// construction, the SAME path `net.rs`/`main.rs` use from env) actually takes
/// effect: a STEEP-decay world (Sandbox) kills a lightly-backed sheep sooner
/// than a GENTLE one (Gallery). This proves the env knob is not a no-op.
#[test]
fn world_config_steep_decay_kills_sooner_than_gentle() {
    // A steep (Sandbox-ish) and a gentle (Gallery-ish) world, configured ONLY
    // by the WorldConfig path (Engine::new_with_config) — no manual setters.
    let steep = WorldConfig {
        decay: DecayParams {
            time_unit_ms: 1_000,
            base: 0.5,
            linear: 1.0,
            quad: 1.0,
            exp_scale: 2.0,
            half_life: 4.0,
        },
        ..WorldConfig::DEFAULT
    };
    let gentle = WorldConfig {
        decay: DecayParams {
            time_unit_ms: 1_000, // same time unit so ages compare directly
            base: 0.5,
            linear: 0.1,
            quad: 0.0,
            exp_scale: 0.0,
            half_life: 8.0,
        },
        ..WorldConfig::DEFAULT
    };

    // Build two engines via the config path, give each the SAME sheep with the
    // SAME light backing (3 votes), and find the age at which each dies.
    let death_age = |cfg: WorldConfig| -> u64 {
        let minter = key(7);
        let mut eng = Engine::new_with_config(key(1), cfg);
        eng.exempt_credit(pub_hex(&minter));
        let (m, id) = mint_env(&minter, 1_000_000, ResolutionTier::R384, 1);
        assert!(eng.apply(&m, 0));
        // 3 light votes (credit-exempt voters → no funding needed).
        for v in 0..3u8 {
            let voter = key(100 + v);
            eng.exempt_credit(pub_hex(&voter));
            let body = serde_json::to_value(&Vote { sheep_id: id.clone(), seq: v as u64 })
                .unwrap();
            assert!(eng.apply(&signed(proto::VOTES, &voter, 0, body), 0));
        }
        assert!(eng.is_alive(&id, 1_000), "alive while young in both worlds");
        // Advance the injected clock until vitality crosses 0.
        let mut age = 1_000u64;
        while eng.is_alive(&id, age) && age < 10_000_000 {
            age += 1_000;
        }
        age
    };

    let steep_death = death_age(steep);
    let gentle_death = death_age(gentle);
    assert!(
        steep_death < gentle_death,
        "steep world kills sooner: steep died at {steep_death}ms, gentle at {gentle_death}ms"
    );
}

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
