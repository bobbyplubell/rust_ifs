//! REAL-TRANSPORT swarm CONVERGENCE tests (ARCHITECTURE v3 §10/§12).
//!
//! Sibling to `swarm_tcp.rs` (reusing the same real-TCP+noise+yamux harness on
//! ephemeral `127.0.0.1:0` ports): those tests prove the FOUNDING flock syncs to
//! a peer that bootstraps at startup. THESE tests prove the swarm CONVERGES — a
//! node learns the *full current flock*, not just the founding set, regardless of
//! WHEN it joins:
//!
//! - **late_joiner_syncs_full_flock** — a sheep minted on the seed AFTER startup
//!   (not one of the persistently-republished founding sheep) reaches a peer that
//!   joins later. This is the gap the founding-republish path does NOT cover:
//!   births are one-shot gossip, so a late joiner never hears a birth that already
//!   happened. Fixed by the flock-sync req/resp catch-up (`/sheep/flock-sync`).
//! - **three_node_gossip_propagation** — A↔B↔C where C only bootstraps off B.
//!   C must learn A's founding sheep (gossip + catch-up relays through B).
//! - **reconnect_resyncs** — a sheep minted while a peer is gone is learned once
//!   it reconnects (catch-up covers reconnect too).
//!
//! All poll-with-timeout (never fixed-sleep for correctness), use a tiny
//! `bootstrap_flock` + the cheapest render tier, and reap every spawned node.

use std::time::Duration;

use ed25519_dalek::SigningKey;
use libp2p::Multiaddr;
use sheep_node::engine::WorldConfig;
use sheep_node::net::{run_tcp_reporting, Control, InjectResult, Snapshot};
use sheep_node::ServeConfig;
use sheep_proto::identity::ResolutionTier;
use sheep_proto::msg::{Mint, Vote};
use sheep_proto::Envelope;
use tokio::sync::{mpsc, oneshot};

/// A spawned in-process node over real TCP (mirrors `swarm_tcp.rs::Node`).
struct Node {
    ctl: mpsc::UnboundedSender<Control>,
    handle: tokio::task::JoinHandle<()>,
    addr: Multiaddr,
}

impl Node {
    /// Snapshot the node's engine state (read-only), bounded so a momentarily
    /// busy reactor returns `None` rather than hanging the poller.
    async fn snap(&self) -> Option<Snapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.ctl.send(Control::Snapshot(reply_tx)).ok()?;
        tokio::time::timeout(Duration::from_millis(500), reply_rx)
            .await
            .ok()?
            .ok()
    }

    /// Inject a signed envelope through the node's `Control::Inject` path — the
    /// exact `/api/msg` write path: `engine.apply` + gossip re-publish. Bounded.
    async fn inject(&self, env: Envelope) -> Option<InjectResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.ctl.send(Control::Inject(env, reply_tx)).ok()?;
        tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .ok()?
            .ok()
    }

    async fn shutdown(self) {
        let _ = self.ctl.send(Control::Shutdown);
        let _ = self.handle.await;
    }
}

/// Unique scoped temp dir for a serving node's regenerable cache (removed on drop).
struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "sheep-conv-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn small_world(bootstrap_flock: usize) -> WorldConfig {
    WorldConfig {
        bootstrap_flock,
        ..WorldConfig::DEFAULT
    }
}

async fn spawn_node(
    seed: [u8; 32],
    bootstrap: Vec<Multiaddr>,
    serve: bool,
    world: WorldConfig,
    tempdirs: &mut Vec<TempDir>,
) -> Node {
    let key = SigningKey::from_bytes(&seed);
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<Control>();
    let (listen_tx, mut listen_rx) = mpsc::unbounded_channel::<Multiaddr>();

    let serve_cfg = if serve {
        let dir = TempDir::new("data");
        let cfg = ServeConfig {
            http_addr: "127.0.0.1:0".parse().unwrap(),
            data_dir: dir.0.clone(),
            ingest_audit: false,
        };
        tempdirs.push(dir);
        Some(cfg)
    } else {
        None
    };

    let listen: Multiaddr = "/ip4/127.0.0.1/tcp/0".parse().unwrap();
    let handle = tokio::spawn(async move {
        let _ = run_tcp_reporting(key, listen, bootstrap, serve_cfg, world, ctl_rx, listen_tx).await;
    });

    let addr = tokio::time::timeout(Duration::from_secs(10), listen_rx.recv())
        .await
        .expect("node reported its bound listen addr within 10s")
        .expect("listen channel delivered an addr");

    Node { ctl: ctl_tx, handle, addr }
}

/// Poll `cond` on a single node's snapshot until it holds or the deadline passes.
async fn poll_node_until(
    n: &Node,
    timeout: Duration,
    mut cond: impl FnMut(&Snapshot) -> bool,
) -> Result<Snapshot, Option<Snapshot>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let s = n.snap().await;
        if let Some(x) = &s {
            if cond(x) {
                return Ok(x.clone());
            }
        }
        if tokio::time::Instant::now() > deadline {
            return Err(s);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Build a signed `Mint` envelope from a fresh key (a "user-minted" sheep). The
/// minter is an unknown key, so `commit_spend` applies it optimistically (no
/// earned-credit record → no overspend rejection): exactly the user-mint path a
/// browser/REST `/api/msg` submission takes. Returns `(envelope, sheep_id_hex)`.
fn user_mint(minter_seed: [u8; 32], ts_micros: u64) -> (Envelope, String) {
    fn hex_lower(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
        out
    }
    let key = SigningKey::from_bytes(&minter_seed);
    let minter_bytes = key.verifying_key().to_bytes();
    let minter_pub = hex_lower(&minter_bytes);
    let mint = Mint {
        ts_micros,
        minter_pub: minter_pub.clone(),
        resolution: ResolutionTier::R384,
        seq: 0,
    };
    let mut env = Envelope::new(
        sheep_proto::proto::FLOCK,
        minter_pub.clone(),
        ts_micros / 1000,
        serde_json::to_value(&mint).unwrap(),
    );
    env.sign(&key);
    // Re-derive the resulting sheep identity exactly as `apply_mint` does.
    let genome = sheep_proto::derive::derive_minted(ts_micros, &minter_bytes);
    let id_hex = sheep_proto::identity::sheep_identity_hex(&genome, ResolutionTier::R384);
    (env, id_hex)
}

/// Build a signed `Vote` envelope from a fresh key (a "user vote" — the §10
/// `/api/msg` backing path). Like `user_mint`, the voter key is unknown so the
/// spend applies optimistically; the engine still requires the target sheep to be
/// known (its birth verified) before the backing counts.
fn user_vote(voter_seed: [u8; 32], sheep_id: &str, ts_ms: u64) -> Envelope {
    let key = SigningKey::from_bytes(&voter_seed);
    let vote = Vote {
        sheep_id: sheep_id.to_string(),
        seq: 0,
    };
    let mut env = Envelope::new(
        sheep_proto::proto::VOTES,
        String::new(),
        ts_ms,
        serde_json::to_value(&vote).unwrap(),
    );
    env.sign(&key);
    env
}

/// Read a sheep's backing from a snapshot (0 if the sheep is unknown to that node).
fn backing_of(s: &Snapshot, sheep_id: &str) -> u64 {
    s.backing
        .iter()
        .find(|(id, _)| id == sheep_id)
        .map(|(_, b)| *b)
        .unwrap_or(0)
}

/// **late_joiner_syncs_full_flock** — the convergence regression.
///
/// A is a seed that mints ONE founding sheep at startup, then — AFTER it has
/// settled — receives a user-minted `Mint` (a fresh signed birth, NOT one of A's
/// persistently-republished founding sheep). A's flock now has 2+ sheep: founding
/// + the extra. THEN B is spawned and bootstraps off A.
///
/// B must converge to ALL of A's sheep, INCLUDING the post-startup mint. The
/// founding-republish path covers the founding sheep, but the extra mint's birth
/// gossip already happened before B existed — so without a catch-up mechanism, B
/// never learns it. Asserts B's flock ⊇ A's flock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_joiner_syncs_full_flock() {
    let mut dirs = Vec::new();

    // A: a serving seed minting exactly one founding sheep.
    let a = spawn_node([0x5A; 32], vec![], true, small_world(1), &mut dirs).await;

    // Wait until A's founding flock exists (the boot-mint returned).
    let a_before = poll_node_until(&a, Duration::from_secs(20), |s| !s.flock.is_empty())
        .await
        .expect("A mints its founding flock");
    let founding_count = a_before.flock.len();

    // Inject a user-minted sheep on A AFTER startup (the gap: one-shot birth
    // gossip with no live peer to hear it). Unique ts so the genome is distinct.
    let ts_micros = (sheep_node::net::now_ms() as u64).saturating_mul(1000) + 7;
    let (mint_env, extra_id) = user_mint([0xEE; 32], ts_micros);
    let res = a.inject(mint_env).await.expect("inject replies");
    assert!(res.accepted, "A must accept the user mint: {res:?}");

    // A's flock now contains the extra sheep (founding + this one).
    let a_after = poll_node_until(&a, Duration::from_secs(10), |s| {
        s.flock.iter().any(|x| x == &extra_id) && s.flock.len() > founding_count
    })
    .await
    .expect("A's flock grows by the user mint");
    let a_flock = a_after.flock.clone();

    // NOW start B and bootstrap it off A. B mints nothing.
    let b = spawn_node([0x6B; 32], vec![a.addr.clone()], false, small_world(0), &mut dirs).await;

    // B must converge to the FULL flock A holds — founding AND the late mint.
    let res = poll_node_until(&b, Duration::from_secs(30), |s| {
        a_flock.iter().all(|id| s.flock.contains(id))
    })
    .await;

    if let Err(last) = res {
        let bf = last.map(|s| s.flock);
        a.shutdown().await;
        b.shutdown().await;
        panic!(
            "B did not converge to A's full flock within 30s.\n A_flock={a_flock:?}\n extra(late mint)={extra_id}\n B_flock={bf:?}"
        );
    }

    a.shutdown().await;
    b.shutdown().await;
}

/// **three_node_gossip_propagation** — A↔B↔C, where C bootstraps ONLY off B (it
/// never learns A's address). C must still converge to A's founding sheep: gossip
/// + the catch-up relays it through B. If C never sees A's sheep, that's a real
/// mesh-relay gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn three_node_gossip_propagation() {
    let mut dirs = Vec::new();

    // A seeds one sheep. B bootstraps off A (and serves so it relays/holds state).
    let a = spawn_node([0x1A; 32], vec![], true, small_world(1), &mut dirs).await;
    let a_flock = poll_node_until(&a, Duration::from_secs(20), |s| !s.flock.is_empty())
        .await
        .expect("A mints founding flock")
        .flock;

    let b = spawn_node([0x2B; 32], vec![a.addr.clone()], true, small_world(0), &mut dirs).await;
    // Let B learn A's flock first (so there is something to relay to C).
    poll_node_until(&b, Duration::from_secs(20), |s| {
        a_flock.iter().all(|id| s.flock.contains(id))
    })
    .await
    .expect("B learns A's flock");

    // C bootstraps ONLY off B — it has no direct path to A.
    let c = spawn_node([0x3C; 32], vec![b.addr.clone()], false, small_world(0), &mut dirs).await;

    let res = poll_node_until(&c, Duration::from_secs(30), |s| {
        a_flock.iter().all(|id| s.flock.contains(id))
    })
    .await;

    if let Err(last) = res {
        let cf = last.map(|s| s.flock);
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
        panic!("C (bootstrapped only off B) never learned A's sheep within 30s.\n A_flock={a_flock:?}\n C_flock={cf:?}");
    }

    a.shutdown().await;
    b.shutdown().await;
    c.shutdown().await;
}

/// **reconnect_resyncs** — a sheep minted while a peer is DISCONNECTED is learned
/// once it reconnects. B bootstraps off A and learns the founding flock; B is then
/// torn down; A receives a user mint while B is gone; a FRESH B (same identity)
/// re-bootstraps off A and must converge to A's full flock — proving catch-up
/// covers reconnect, not just first-join. (A fresh process with B's key models a
/// reconnect: it starts with empty state and must re-sync the current flock.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_resyncs() {
    let mut dirs = Vec::new();

    let a = spawn_node([0x7A; 32], vec![], true, small_world(1), &mut dirs).await;
    let founding = poll_node_until(&a, Duration::from_secs(20), |s| !s.flock.is_empty())
        .await
        .expect("A mints founding flock")
        .flock;

    // B connects and learns the founding flock, then disconnects (shutdown).
    let b1 = spawn_node([0x8B; 32], vec![a.addr.clone()], false, small_world(0), &mut dirs).await;
    poll_node_until(&b1, Duration::from_secs(20), |s| {
        founding.iter().all(|id| s.flock.contains(id))
    })
    .await
    .expect("B learns founding flock before disconnect");
    b1.shutdown().await;

    // While B is gone, A receives a user mint (one-shot birth, no B to hear it).
    let ts_micros = (sheep_node::net::now_ms() as u64).saturating_mul(1000) + 11;
    let (mint_env, extra_id) = user_mint([0xAB; 32], ts_micros);
    assert!(
        a.inject(mint_env).await.expect("inject replies").accepted,
        "A accepts the user mint while B is gone"
    );
    let a_flock = poll_node_until(&a, Duration::from_secs(10), |s| s.flock.contains(&extra_id))
        .await
        .expect("A's flock includes the mint")
        .flock;

    // A fresh B (same key) reconnects and must converge to A's CURRENT full flock.
    let b2 = spawn_node([0x8B; 32], vec![a.addr.clone()], false, small_world(0), &mut dirs).await;
    let res = poll_node_until(&b2, Duration::from_secs(30), |s| {
        a_flock.iter().all(|id| s.flock.contains(id))
    })
    .await;

    if let Err(last) = res {
        let bf = last.map(|s| s.flock);
        a.shutdown().await;
        b2.shutdown().await;
        panic!("reconnecting B did not re-converge to A's full flock within 30s.\n A_flock={a_flock:?}\n extra={extra_id}\n B_flock={bf:?}");
    }

    a.shutdown().await;
    b2.shutdown().await;
}

/// **vote_backing_converges** — votes gossip + converge across the swarm (§2.2).
/// A vote cast on node A for a founding sheep raises that sheep's backing as
/// OBSERVED on node B (which minted nothing). This is a non-render convergence
/// property: backing is log-derived from gossiped Vote envelopes, so B's tally
/// must rise once A's vote propagates — purely over real TCP. (It also exercises
/// the §10 catch-up: votes are in the birth log, so B converges on backing whether
/// it hears the live vote gossip or catches it up on connect.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vote_backing_converges() {
    let mut dirs = Vec::new();

    // A seeds one sheep; B bootstraps off A and mints nothing.
    let a = spawn_node([0x9A; 32], vec![], true, small_world(1), &mut dirs).await;
    let b = spawn_node([0xAB; 32], vec![a.addr.clone()], false, small_world(0), &mut dirs).await;

    // Wait for A's founding flock, then pick a sheep both nodes will know.
    let target = poll_node_until(&a, Duration::from_secs(20), |s| !s.flock.is_empty())
        .await
        .map(|s| s.flock.first().cloned().expect("A has a founding sheep"))
        .expect("A mints its founding flock");
    // Wait for B to learn the target sheep + observe its backing baseline.
    let baseline = poll_node_until(&b, Duration::from_secs(20), |s| s.flock.contains(&target))
        .await
        .map(|s| backing_of(&s, &target))
        .expect("B learns the target sheep");

    // Cast THREE user votes on A for the target (distinct keys so each is a fresh
    // optimistic spend, not an equivocation/dup).
    for (i, seed) in [[0x01u8; 32], [0x02; 32], [0x03; 32]].into_iter().enumerate() {
        let v = user_vote(seed, &target, sheep_node::net::now_ms() + i as u64);
        assert!(
            a.inject(v).await.expect("vote inject replies").accepted,
            "A accepts user vote {i}"
        );
    }

    // B's observed backing for the target must rise above the baseline — the votes
    // cast on A converged to B purely via gossip/catch-up over real TCP.
    let res = poll_node_until(&b, Duration::from_secs(30), |s| {
        backing_of(s, &target) > baseline
    })
    .await;

    if let Err(last) = res {
        let bb = last.map(|s| backing_of(&s, &target));
        a.shutdown().await;
        b.shutdown().await;
        panic!("B's backing for {target} did not rise above baseline {baseline} within 30s (saw {bb:?})");
    }

    a.shutdown().await;
    b.shutdown().await;
}
