//! Step-2 deliverable (ARCHITECTURE v3 §12-step-2): a robust **two-peer**
//! integration test that proves the neutral-claim + attestation loop closes
//! **with no central node**.
//!
//! Two full engine+swarm nodes run IN-PROCESS on libp2p's **in-memory
//! transport** (no real TCP ports → no port-collision flakiness). They connect
//! peer-to-peer, both inject the deterministic genesis sheep, and then we poll
//! (never fixed-sleep) until the loop demonstrably closes:
//!
//! 1. both peers learn the genesis sheep (gossip births, §2.1/§2.3);
//! 2. both peers CLAIM and render → gossip `Coverage`, and coverage for the
//!    sheep grows on BOTH peers (gossip convergence, §4);
//! 3. each audits the other's tiles, emits matching `Attestation`s (honest
//!    re-render, §6) → tiles become confirmed → `own_confirmed_tiles()` advances
//!    on both peers;
//! 4. the two peers hold claims on DISTINCT blocks (soft-claim collision
//!    avoidance, §4 — least-covered selection + per-key seq).
//!
//! Renders are real `flame_core` renders at R384/SPP — slow in a debug build,
//! fast in `--release` (run `cargo test -p sheep-node --release --test two_peer`
//! for the quick path). The success condition is bounded by ticks, not volume:
//! a single rendered block (16 tiles) on each peer is enough to advance
//! `own_confirmed_tiles`, so the test does not need to mint a full credit.

use std::time::Duration;

use ed25519_dalek::SigningKey;
use libp2p::core::transport::MemoryTransport;
use libp2p::core::upgrade::Version;
use libp2p::{noise, yamux, Multiaddr, Transport};
use sheep_node::engine::WorldConfig;
use sheep_node::net::{run_on_transport_with, Control, Snapshot};
use sheep_node::{genesis_sheep_hex, net::libp2p_key};

/// Lowercase-hex public key for a node seed (matches `Envelope.from`).
fn pub_hex(seed: [u8; 32]) -> String {
    let pk = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
    pk.iter().map(|b| format!("{b:02x}")).collect()
}

/// A world for a collaborating seed: trusts the peer (as the two production seeds
/// mutually do via `SHEEP_TRUSTED_KEYS`, so neither stands down for the other
/// under the §4 contributor-deference gate) and seeds a single immortal genesis
/// sheep (one is enough to close the loop, and keeps debug renders fast).
fn collab_world(trusted: &str) -> WorldConfig {
    let mut w = WorldConfig::default();
    w.trusted_keys.insert(trusted.to_string());
    w.genesis_flock_size = 1;
    w
}
use tokio::sync::{mpsc, oneshot};

/// Build a boxed in-memory transport (noise + yamux) for one node's key.
fn mem_transport(
    signing_key: &SigningKey,
) -> libp2p::core::transport::Boxed<(libp2p::PeerId, libp2p::core::muxing::StreamMuxerBox)> {
    let key = libp2p_key(signing_key);
    MemoryTransport::default()
        .upgrade(Version::V1)
        .authenticate(noise::Config::new(&key).expect("noise config"))
        .multiplex(yamux::Config::default())
        .boxed()
}

/// Snapshot a running node via its control channel.
async fn snap(tx: &mpsc::UnboundedSender<Control>) -> Option<Snapshot> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(Control::Snapshot(reply_tx)).ok()?;
    reply_rx.await.ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_peers_close_the_loop() {
    // Two distinct node identities.
    let key_a = SigningKey::from_bytes(&[0xA1; 32]);
    let key_b = SigningKey::from_bytes(&[0xB2; 32]);

    // Fixed in-memory listen addrs; B dials A. /memory/<n> never collides with
    // real TCP and is process-local, so this is deterministic and port-free.
    let addr_a: Multiaddr = "/memory/424242".parse().unwrap();
    let addr_b: Multiaddr = "/memory/424243".parse().unwrap();

    let (tx_a, rx_a) = mpsc::unbounded_channel::<Control>();
    let (tx_b, rx_b) = mpsc::unbounded_channel::<Control>();

    // Peer A: listens, no bootstrap.
    let ta = mem_transport(&key_a);
    let addr_a2 = addr_a.clone();
    let world_a = collab_world(&pub_hex([0xB2; 32]));
    let ha = tokio::spawn(async move {
        let _ = run_on_transport_with(key_a, ta, addr_a2, vec![], rx_a, None, world_a).await;
    });

    // Peer B: listens on its own addr, dials A (the only "bootstrap").
    let tb = mem_transport(&key_b);
    let addr_b2 = addr_b.clone();
    let addr_a_dial = addr_a.clone();
    let world_b = collab_world(&pub_hex([0xA1; 32]));
    let hb = tokio::spawn(async move {
        let _ = run_on_transport_with(key_b, tb, addr_b2, vec![addr_a_dial], rx_b, None, world_b).await;
    });

    let sheep = genesis_sheep_hex();

    // Poll for the full close-the-loop condition. Generous bound: in release
    // each tick renders a 16-tile block (~0.5s); ~120s of debug-build headroom.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let mut last_a: Option<Snapshot> = None;
    let mut last_b: Option<Snapshot> = None;
    // Soft-claim collision avoidance (§4) is a TRANSIENT property: at startup
    // both peers can briefly pick block 0 before each other's claim gossip
    // arrives (a duplicate render determinism dedups harmlessly), then diverge.
    // So we assert we EVER observed the two peers holding DISTINCT blocks across
    // the run, rather than requiring it at the exact success instant.
    let mut ever_distinct_claims = false;
    let mut dbg_tick = 0u32;

    loop {
        if tokio::time::Instant::now() > deadline {
            panic!(
                "loop did not close within deadline.\n A={:?}\n B={:?}",
                last_a, last_b
            );
        }

        let (sa, sb) = (snap(&tx_a).await, snap(&tx_b).await);
        last_a = sa.clone();
        last_b = sb.clone();

        dbg_tick += 1;
        if std::env::var("SHEEP_DEBUG").is_ok() && dbg_tick % 13 == 0 {
            let f = |s: &Option<Snapshot>| match s {
                Some(x) => format!(
                    "flock={} cov={:?} own={} claims={}",
                    x.flock.len(),
                    x.coverage,
                    x.own_confirmed_tiles,
                    x.live_claims.len()
                ),
                None => "None".into(),
            };
            eprintln!("[poll] A: {} | B: {}", f(&last_a), f(&last_b));
        }

        if let (Some(a), Some(b)) = (sa, sb) {
            let a_knows = a.flock.contains(&sheep);
            let b_knows = b.flock.contains(&sheep);

            let a_cov = a
                .coverage
                .iter()
                .find(|(s, _)| s == &sheep)
                .map(|(_, c)| *c)
                .unwrap_or(0);
            let b_cov = b
                .coverage
                .iter()
                .find(|(s, _)| s == &sheep)
                .map(|(_, c)| *c)
                .unwrap_or(0);

            // Distinct-claim check: across both peers' live claims, the two
            // peers' OWN active claims must be on different blocks (soft-claim
            // collision avoidance). We read each peer's own claim by claimant.
            let a_own_block = a
                .live_claims
                .iter()
                .find(|(_, claimant)| claimant == &a.self_pub)
                .map(|(blk, _)| blk.clone());
            let b_own_block = b
                .live_claims
                .iter()
                .find(|(_, claimant)| claimant == &b.self_pub)
                .map(|(blk, _)| blk.clone());
            if let (Some(x), Some(y)) = (&a_own_block, &b_own_block) {
                if x != y {
                    ever_distinct_claims = true;
                }
            }

            let closed = a_knows
                && b_knows
                && a_cov > 0
                && b_cov > 0
                && a.own_confirmed_tiles > 0
                && b.own_confirmed_tiles > 0
                && ever_distinct_claims;

            if closed {
                // Final assertions (explicit, for a clear failure message).
                assert!(a_knows && b_knows, "both peers learned the genesis sheep");
                assert!(
                    a_cov > 0 && b_cov > 0,
                    "coverage grew on BOTH peers (gossip convergence): a={a_cov} b={b_cov}"
                );
                assert!(
                    a.own_confirmed_tiles > 0 && b.own_confirmed_tiles > 0,
                    "each peer's own tiles got confirmed via the OTHER's attestations: a={} b={}",
                    a.own_confirmed_tiles,
                    b.own_confirmed_tiles
                );
                assert!(
                    ever_distinct_claims,
                    "the two peers were observed claiming distinct blocks at some point \
                     (soft-claim collision avoidance, §4)"
                );

                // Clean shutdown.
                let _ = tx_a.send(Control::Shutdown);
                let _ = tx_b.send(Control::Shutdown);
                let _ = ha.await;
                let _ = hb.await;
                return;
            }
        }

        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}
