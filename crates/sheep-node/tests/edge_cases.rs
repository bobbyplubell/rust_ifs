//! Edge-case tests around the swarm/engine robustness the real-transport work
//! surfaced (ARCHITECTURE v3 §2.2 decay, §10 identity, §2.1 mint). These are
//! cheap, deterministic, clock-injected unit-level tests — NO TCP, NO renders —
//! so they run in milliseconds and guard the properties a live two-node deploy
//! depends on:
//!
//! - **restart_keeps_peer_id**: a `--key-file` node (same ed25519 secret) derives
//!   the SAME libp2p PeerId after a restart — so durable bootstrap multiaddrs
//!   (`/p2p/<peerid>`) survive a node restart (the deploy relies on this).
//! - **founding_sheep_lives_its_decay_lifetime**: a backed founding sheep stays
//!   alive for a configured span under wall-clock decay and dies afterward with
//!   no further votes — proving the bootstrap-flock lifetime is real (and driven
//!   purely by the injected clock).
//! - **derive_minted_never_hangs**: sweeping many wall-clock-style mint seeds, the
//!   genome derivation (escape-reseed / density filter) always completes quickly —
//!   guarding the bootstrap-mint robustness (a hang here is the boot-mint hang).

use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use sheep_node::engine::{Engine, WorldConfig};
use sheep_node::net::libp2p_key;

/// A node identified by a fixed 32-byte secret (as `--key-file` persists) maps to
/// a STABLE libp2p PeerId across "restarts" — i.e. `libp2p_key` is a pure function
/// of the secret. This is what makes a seed's `/p2p/<peerid>` bootstrap multiaddr
/// durable across restarts (the deploy depends on it).
#[test]
fn restart_keeps_peer_id() {
    let secret = [0x7e; 32];

    // "First boot": derive the PeerId from the persisted secret.
    let peer_id_1 = libp2p_key(&SigningKey::from_bytes(&secret)).public().to_peer_id();
    // "Restart": load the SAME secret from the key file and re-derive.
    let peer_id_2 = libp2p_key(&SigningKey::from_bytes(&secret)).public().to_peer_id();

    assert_eq!(
        peer_id_1, peer_id_2,
        "a --key-file node keeps its PeerId across restarts (durable bootstrap addr)"
    );

    // And a DIFFERENT secret yields a DIFFERENT PeerId (no accidental collision).
    let other = libp2p_key(&SigningKey::from_bytes(&[0x11; 32]))
        .public()
        .to_peer_id();
    assert_ne!(peer_id_1, other, "distinct keys → distinct PeerIds");
}

/// (v4) Survival is the vote ranking, NOT wall-clock age: a lone backed sheep
/// (no competitors) stays live forever regardless of the injected clock — there
/// is no decay-driven death. It dies only by being out-competed (dropping below
/// the `n_target` cutoff), and even then stays in the raw flock history.
#[test]
fn lone_sheep_survives_without_decay_dies_only_when_outcompeted() {
    let world = WorldConfig {
        bootstrap_flock: 1,
        ..WorldConfig::default()
    };
    let mut engine = Engine::new_with_config(SigningKey::from_bytes(&[0x42; 32]), &world);

    // Seed one founding sheep with 5 self-votes (backing 5).
    let birth = 1_000_000u64;
    let envs = engine.bootstrap_seed_flock(1, 5, birth);
    assert!(!envs.is_empty(), "bootstrap minted a founding sheep + votes");
    let sheep = engine
        .live_flock(birth)
        .keys()
        .next()
        .cloned()
        .expect("one founding sheep is live at birth");
    assert_eq!(engine.backing(&sheep), 5, "5 self-votes → backing 5");

    // No competitors → live regardless of how far the injected clock advances
    // (survival is vote-count driven, not wall-clock; an idle swarm freezes).
    assert!(engine.is_alive(&sheep, birth), "alive at birth");
    assert!(
        engine.is_alive(&sheep, birth + 365 * 24 * 60 * 60 * 1_000),
        "still alive a year later — no wall-clock decay"
    );

    // Death only by being out-competed: mint 4 sheep, each more-backed, so our
    // sheep falls below the N_base (=4) cutoff.
    let minter = SigningKey::from_bytes(&[0x77; 32]);
    let minter_pub: String = minter
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    engine.exempt_credit(minter_pub.clone());
    for i in 0..4u64 {
        // Mint a distinct sheep.
        let mint = serde_json::json!({
            "ts_micros": 5_000 + i,
            "minter_pub": minter_pub,
            "resolution": "R384",
            "seq": i,
        });
        let mut menv = sheep_proto::Envelope::new(sheep_proto::proto::FLOCK, "", 5, mint);
        menv.sign(&minter);
        assert!(engine.apply(&menv, birth), "mint applies");
        let id = {
            let g = sheep_proto::derive::derive_minted(5_000 + i, &minter.verifying_key().to_bytes());
            let idb = sheep_proto::identity::sheep_identity(&g, sheep_proto::identity::ResolutionTier::R384);
            idb.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        // 6 votes each (> our sheep's 5) from distinct voters.
        for v in 0..6u8 {
            let voter = SigningKey::from_bytes(&[100 + i as u8 * 10 + v; 32]);
            let vp: String = voter.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
            engine.exempt_credit(vp);
            let vote = serde_json::json!({ "sheep_id": id, "seq": 0 });
            let mut venv = sheep_proto::Envelope::new(sheep_proto::proto::VOTES, "", 5, vote);
            venv.sign(&voter);
            assert!(engine.apply(&venv, birth), "vote applies");
        }
    }

    assert!(
        !engine.is_alive(&sheep, birth),
        "out-competed by 4 better-backed sheep → below the n_target cutoff"
    );
    assert!(
        engine.flock().contains_key(&sheep),
        "out-competed sheep stays in the flock history (can return if re-voted)"
    );
}

/// Sweeping many wall-clock-style mint seeds, every founding-flock derivation
/// completes quickly. A hang here is exactly the boot-mint hang (an escape-time /
/// density-filter pathology on some seed) that starves the swarm — so we bound it.
#[test]
fn derive_minted_never_hangs() {
    let world = WorldConfig {
        bootstrap_flock: 1,
        ..WorldConfig::default()
    };

    // Sweep distinct minter keys (distinct pubkeys → distinct seed streams) and a
    // spread of "now" values mimicking wall-clock ms, minting a small flock each —
    // every individual mint must derive its genome well under a generous bound.
    let mut worst = Duration::ZERO;
    let bases: [u64; 6] = [
        1_700_000_000_000, // ~2023 ms
        1_750_000_000_123,
        1_800_000_000_456,
        1_900_000_000_789,
        2_000_000_000_000,
        2_500_000_000_321,
    ];
    for (k, now) in bases.iter().enumerate() {
        let mut engine =
            Engine::new_with_config(SigningKey::from_bytes(&[(k as u8).wrapping_add(1); 32]), &world);
        // Mint a handful of founding sheep at this clock; time the whole batch and
        // bound the PER-SHEEP cost (the batch mints `count` distinct genomes).
        let count = 4usize;
        let t0 = Instant::now();
        let envs = engine.bootstrap_seed_flock(count, 8, *now);
        let elapsed = t0.elapsed();
        assert!(
            !envs.is_empty(),
            "mint produced birth+vote envelopes for now={now}"
        );
        assert_eq!(
            engine.flock().len(),
            count,
            "all {count} founding sheep minted (no dropped/duplicate genome) at now={now}"
        );
        let per_sheep = elapsed / count as u32;
        if per_sheep > worst {
            worst = per_sheep;
        }
        // Bound the per-sheep derive. The guard catches a HANG (a pathological
        // escape-reseed / density-filter seed that never terminates — seconds to
        // forever), not a micro-benchmark, so the bound is profile-aware: ~150ms
        // in an optimized build, with generous 10x headroom in an unoptimized
        // debug build on this ARM laptop (~220ms/sheep observed). Either way a true
        // hang (>= the bound) trips it.
        let bound = if cfg!(debug_assertions) {
            Duration::from_millis(1_500)
        } else {
            Duration::from_millis(150)
        };
        assert!(
            per_sheep < bound,
            "derive_minted hung/slow at now={now}: {per_sheep:?}/sheep ({elapsed:?} for {count}, bound {bound:?})"
        );
    }
    eprintln!("[derive_minted_never_hangs] worst per-sheep derive = {worst:?}");
}
