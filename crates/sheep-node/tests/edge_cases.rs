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
use sheep_node::engine::{DecayParams, Engine, WorldConfig};
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

/// A backed founding sheep is alive for a bounded span under wall-clock decay and
/// dies afterward without further votes. We drive the engine directly with an
/// injected clock (no wall-clock, no networking), using a deliberately STEEP decay
/// (small half-life) so the lifetime is short + the test is fast — proving the
/// bootstrap-flock lifetime is a real, decay-driven property.
#[test]
fn founding_sheep_lives_its_decay_lifetime() {
    // A steep, short-lived world: decay reaches the backing level within seconds.
    let world = WorldConfig {
        decay: DecayParams {
            time_unit_ms: 1_000, // 1 decay unit == 1s of age
            base: 0.0,
            linear: 1.0, // decay ≈ age_in_seconds (plus the exp tail)
            quad: 0.0,
            exp_scale: 0.0,
            half_life: 8.0,
        },
        bootstrap_flock: 1,
        ..WorldConfig::DEFAULT
    };

    let mut engine = Engine::new_with_config(SigningKey::from_bytes(&[0x42; 32]), world);

    // Seed one founding sheep at t=0 with 5 self-votes (backing = 5). With
    // linear≈1/s decay, vitality = 5 − ~age_s, so it should be alive at a few
    // seconds and dead by ~6s+ (the exp tail pulls it under a touch sooner).
    let birth = 1_000_000u64; // arbitrary non-zero "now"
    let envs = engine.bootstrap_seed_flock(1, 5, birth);
    assert!(!envs.is_empty(), "bootstrap minted a founding sheep + votes");

    let sheep = engine
        .live_flock(birth)
        .keys()
        .next()
        .cloned()
        .expect("one founding sheep is live at birth");
    assert_eq!(engine.backing(&sheep), 5, "5 self-votes → backing 5");

    // Alive partway through its span (well before backing is consumed by decay).
    assert!(
        engine.is_alive(&sheep, birth + 2_000),
        "founding sheep alive 2s in: vitality={:?}",
        engine.vitality(&sheep, birth + 2_000)
    );

    // Dead well after its span, with NO further votes cast (decay alone kills it).
    let late = birth + 60_000; // 60s of age >> 5 backing under ~1/s decay
    assert!(
        !engine.is_alive(&sheep, late),
        "founding sheep dies under decay without renewed backing: vitality={:?}",
        engine.vitality(&sheep, late)
    );
    assert!(
        engine.live_flock(late).is_empty(),
        "the dead founding sheep leaves the LIVE flock"
    );
    // ...but remains in the raw flock history (never erased).
    assert!(
        engine.flock().contains_key(&sheep),
        "dead sheep stays in the flock history"
    );
}

/// Sweeping many wall-clock-style mint seeds, every founding-flock derivation
/// completes quickly. A hang here is exactly the boot-mint hang (an escape-time /
/// density-filter pathology on some seed) that starves the swarm — so we bound it.
#[test]
fn derive_minted_never_hangs() {
    let world = WorldConfig {
        bootstrap_flock: 1,
        ..WorldConfig::DEFAULT
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
            Engine::new_with_config(SigningKey::from_bytes(&[(k as u8).wrapping_add(1); 32]), world);
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
