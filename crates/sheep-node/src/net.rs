//! The libp2p transport layer that wraps the pure [`Engine`] (ARCHITECTURE v3
//! §10). The engine is the brain; this is *just I/O*.
//!
//! - **Inbound** gossip → `serde_json::from_slice::<Envelope>()` →
//!   [`Engine::apply`].
//! - **Outbound**: a ~1s timer calls [`Engine::tick`]; each returned [`Envelope`]
//!   is published — already signed + ts-stamped by the engine — on the gossip
//!   topic for its `env.t`.
//!
//! The [`NetworkBehaviour`] combines **gossipsub** (the topic fan-out for the
//! `env.t` tags), **identify** (capability/connect handshake, §1.1), and
//! **request-response** declared for [`proto::PIECE`] / [`proto::ASSIGN`] (§10
//! req/resp; stub bodies for now — heavy histogram transfer is step 3).
//!
//! Two run entry points share one loop body:
//! - [`run`] — real TCP+noise+yamux; the `main` binary uses it.
//! - [`run_on_transport`] — generic over any libp2p `Transport`, so the
//!   two-peer integration test drives it on the **in-memory** transport (no real
//!   ports → no flakiness).

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use futures::StreamExt;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::transport::Boxed;
use libp2p::core::upgrade::Version;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{
    gossipsub, identify, noise, request_response, yamux, Multiaddr, PeerId, StreamProtocol, Swarm,
    SwarmBuilder, Transport,
};
use sheep_proto::msg::{Coverage, PieceUpload};
use sheep_proto::{proto, Envelope};
use tokio::sync::mpsc;

use crate::accumulator::Accumulator;
use crate::derive_minted_genesis::genesis_mint;
use crate::engine::{Engine, WorldConfig};
use crate::http::{self, HallView, HttpState, ReadState, SheepView};

/// How often the contribute loop fires `engine.tick(now)` (§4 / §12-step-2).
pub const TICK_INTERVAL: Duration = Duration::from_millis(1000);

/// The gossip topics this node subscribes to. Each is a bare `/sheep/...` topic
/// string (§10); the engine tags outbound envelopes with these exact `env.t`s.
fn gossip_topics() -> [&'static str; 7] {
    [
        proto::FLOCK,    // births / flock membership (§2.1, §2.3)
        proto::CLAIMS,   // soft claims + heartbeats (§4)
        proto::PROGRESS, // coverage / `have` (§4)
        proto::ATTEST,   // audit attestations (§6)
        proto::REP,      // reputation deltas + bans (§6)
        proto::VOTES,    // survival backing (§2.2) — subscribed for completeness
        proto::PIECE,    // see note below
    ]
}

/// Map a message-type tag (`env.t`) to its gossipsub topic.
///
/// `PIECE` is *declared* as a req/resp protocol (§10), but the engine emits it
/// from `tick` as an envelope. The engine now fills `hist_b64` with the
/// compressed tile histogram (the heavy artifact, §5) for an accumulator to
/// ingest; for now we still carry PIECE on a gossip topic of the same name,
/// keeping the publish path uniform. A later step moves the heavy histogram to
/// the dedicated peer→accumulator req/resp channel (so it is not gossip-flooded).
fn topic_for(t: &str) -> gossipsub::IdentTopic {
    gossipsub::IdentTopic::new(t)
}

/// Real wall-clock milliseconds (§ engine clock injection: the transport reads
/// the clock, the engine stays pure).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---- behaviour --------------------------------------------------------------

/// The combined node behaviour (§10): gossipsub + identify + request-response.
#[derive(NetworkBehaviour)]
pub struct SheepBehaviour {
    pub gossipsub: gossipsub::Behaviour,
    pub identify: identify::Behaviour,
    /// Declared for [`proto::PIECE`] and [`proto::ASSIGN`] (§10). Bodies are raw
    /// JSON for now; the upload/assign handlers are stubs (step 3 / step 4).
    pub req_resp: request_response::json::Behaviour<Vec<u8>, Vec<u8>>,
    /// §10 convergence — the dedicated flock catch-up req/resp ([`proto::FLOCK_SYNC`]).
    /// Kept SEPARATE from `req_resp` so its request/response events route
    /// unambiguously (a single multi-protocol `request_response` behaviour does
    /// not tell you which protocol a request arrived on). Request body is empty
    /// (`FlockSyncRequest`); the response is a JSON array of birth-log [`Envelope`]s
    /// the requester re-applies (each re-verified) to converge to the full flock.
    pub flock_sync: request_response::json::Behaviour<FlockSyncRequest, FlockSyncResponse>,
}

/// §10 flock-sync request — empty marker (the requester wants the responder's
/// full birth log). A unit-like struct so the JSON codec has a concrete type.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FlockSyncRequest {}

/// §10 flock-sync response — the responder's birth log: every accepted Mint/Breed
/// + Vote envelope, in arrival order. Bounded by `FLOCK_SYNC_MAX` so a large flock
/// can't make one response unbounded; the requester re-applies each through
/// `engine.apply` (re-verifying signature + genome derivation), so it is trustless.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FlockSyncResponse {
    pub births: Vec<Envelope>,
}

/// Cap on how many birth-log envelopes one flock-sync response carries (the most
/// RECENT are sent — a late joiner most needs births it hasn't heard, and the
/// founding sheep are also covered by the persistent republish path). A bounded
/// full list; a fuller deployment can page or send a have-set diff.
pub const FLOCK_SYNC_MAX: usize = 4096;

/// Build the gossipsub + identify + request-response behaviour for one node.
fn build_behaviour(key: &libp2p::identity::Keypair) -> Result<SheepBehaviour, NetError> {
    // A short heartbeat so the two-peer test's mesh forms quickly; production
    // can lengthen it. Permissive validation: bodies are self-signed Envelopes
    // (§10), so the engine — not gossipsub — is the trust gate.
    let gs_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_millis(200))
        .validation_mode(gossipsub::ValidationMode::Permissive)
        // A PIECE envelope carries a full-frame histogram (§5): ~4.7 MB for an
        // R384 tile, far over gossipsub's default 64 KiB cap, so without this
        // pieces fail to publish (`MessageTooLarge`) and never propagate. 10 MiB
        // comfortably fits one R384 piece. STOPGAP: heavy histograms shouldn't be
        // gossip-flooded at all — the real fix (announce-over-gossip + pull the
        // bytes over the req/resp PIECE channel) is a Phase-2 follow-up.
        .max_transmit_size(10 * 1024 * 1024)
        // Content-address messages by their bytes so identical re-publishes
        // (engine envelopes are deterministic) dedup in the mesh.
        .message_id_fn(|m: &gossipsub::Message| {
            let mut h = DefaultHasher::new();
            m.data.hash(&mut h);
            gossipsub::MessageId::from(h.finish().to_be_bytes().to_vec())
        })
        .build()
        .map_err(|e| NetError::Behaviour(e.to_string()))?;

    let gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(key.clone()),
        gs_config,
    )
    .map_err(|e| NetError::Behaviour(e.to_string()))?;

    let identify = identify::Behaviour::new(identify::Config::new(
        proto::ID.to_string(),
        key.public(),
    ));

    let req_resp = request_response::json::Behaviour::<Vec<u8>, Vec<u8>>::new(
        [
            (
                StreamProtocol::new(proto::PIECE),
                request_response::ProtocolSupport::Full,
            ),
            (
                StreamProtocol::new(proto::ASSIGN),
                request_response::ProtocolSupport::Full,
            ),
        ],
        request_response::Config::default(),
    );

    // §10 convergence: the dedicated flock-sync req/resp (separate behaviour so
    // its events route unambiguously). Outbound requests need a small timeout so
    // a peer that goes away doesn't leak an outbound slot.
    let flock_sync = request_response::json::Behaviour::<FlockSyncRequest, FlockSyncResponse>::new(
        [(
            StreamProtocol::new(proto::FLOCK_SYNC),
            request_response::ProtocolSupport::Full,
        )],
        request_response::Config::default()
            .with_request_timeout(Duration::from_secs(10)),
    );

    Ok(SheepBehaviour {
        gossipsub,
        identify,
        req_resp,
        flock_sync,
    })
}

// ---- errors -----------------------------------------------------------------

#[derive(Debug)]
pub enum NetError {
    Behaviour(String),
    Transport(String),
    Listen(String),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::Behaviour(s) => write!(f, "behaviour: {s}"),
            NetError::Transport(s) => write!(f, "transport: {s}"),
            NetError::Listen(s) => write!(f, "listen: {s}"),
        }
    }
}

impl std::error::Error for NetError {}

// ---- libp2p key derivation --------------------------------------------------

/// Build a libp2p ed25519 [`libp2p::identity::Keypair`] from the same 32-byte
/// ed25519 secret the [`Engine`] signs envelopes with — so the node's libp2p
/// `PeerId` and its protocol signing key share one secret.
pub fn libp2p_key(signing_key: &SigningKey) -> libp2p::identity::Keypair {
    let mut secret = signing_key.to_bytes();
    // `ed25519::SecretKey::try_from_bytes` consumes the 32-byte seed.
    let sk = libp2p::identity::ed25519::SecretKey::try_from_bytes(&mut secret)
        .expect("32-byte ed25519 secret is always valid");
    libp2p::identity::ed25519::Keypair::from(sk).into()
}

// ---- swarm construction -----------------------------------------------------

/// Build a Swarm over a caller-provided authenticated, multiplexed transport.
/// Used by both [`run`] (TCP) and the in-memory test path.
fn build_swarm(
    key: libp2p::identity::Keypair,
    transport: Boxed<(PeerId, StreamMuxerBox)>,
) -> Result<Swarm<SheepBehaviour>, NetError> {
    let behaviour = build_behaviour(&key)?;
    let swarm = SwarmBuilder::with_existing_identity(key)
        .with_tokio()
        .with_other_transport(|_| transport)
        .map_err(|e| NetError::Transport(e.to_string()))?
        .with_behaviour(|_| behaviour)
        .map_err(|e| NetError::Behaviour(e.to_string()))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();
    Ok(swarm)
}

// ---- server / accumulate capability gate (§1.1) -----------------------------

/// The **serve + accumulate** capability bundle (ARCHITECTURE v3 §1.1). A node
/// given a [`ServeConfig`] is a "server/accumulator": it ingests observed (and
/// its own) [`PieceUpload`]s into an [`Accumulator`] and serves the read-only
/// watch face (flock/sheep/video/hall) over HTTP. A node WITHOUT one is a plain
/// peer/worker — it never accumulates the heavy histograms and runs no HTTP
/// server. This is the orthogonal-capability switch: `--http-addr` turns it on.
#[derive(Clone)]
pub struct ServeConfig {
    /// Address to bind the read-only HTTP watch face on (§10 reads half).
    pub http_addr: SocketAddr,
    /// On-disk dir for the regenerable video cache (§5).
    pub data_dir: PathBuf,
    /// §6.1 gateway ingest-audit policy for the browser/REST write-face. When
    /// `true` (default for a public deployment), browser-origin render
    /// submissions (`Coverage`/`PieceUpload` posted to `/api/msg`) are
    /// sampled-audited (reputation-graduated re-render of a sampled fraction)
    /// BEFORE this node vouches/gossips them; a hash mismatch is rejected. When
    /// `false` (personal-node mode), submissions are optimistically forwarded
    /// and the swarm peer-audits downstream. Either way the node co-signs/re-
    /// emits what it injects (so it has skin in the game, §6.1).
    pub ingest_audit: bool,
}

// ---- public run entry points -------------------------------------------------

/// Run a node on a **real TCP** transport (noise + yamux): the `main` binary's
/// path. Listens on `listen`, dials each `bootstrap` multiaddr, injects the
/// genesis sheep, then drives the engine loop until the process ends.
///
/// `serve` gates the **server/accumulator** role (§1.1): `Some(cfg)` enables the
/// accumulator + HTTP watch face; `None` is a plain peer/worker.
pub async fn run(
    signing_key: SigningKey,
    listen: Multiaddr,
    bootstrap: Vec<Multiaddr>,
    serve: Option<ServeConfig>,
    world: WorldConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let key = libp2p_key(&signing_key);
    let tcp = libp2p::tcp::tokio::Transport::new(libp2p::tcp::Config::default())
        .upgrade(Version::V1)
        .authenticate(noise::Config::new(&key)?)
        .multiplex(yamux::Config::default())
        .boxed();
    let mut swarm = build_swarm(key, tcp)?;
    swarm.listen_on(listen).map_err(|e| NetError::Listen(e.to_string()))?;
    // The libp2p PeerId (base58 multihash of the pubkey) — NOT the ed25519 hex.
    // Bootstrap multiaddrs use this as the `/p2p/<peerid>` component; the actual
    // dialable listen addr is logged on `NewListenAddr` below.
    eprintln!("[node] peer_id={}", swarm.local_peer_id());
    // §2.2/§3 personality knobs (decay/hall/costs) are injected at construction;
    // the engine stays pure (config is data, the clock is still passed in).
    let engine = Engine::new_with_config(signing_key, world);
    // The control channel is unused by `main` (no programmatic shutdown), but
    // shares the loop body with the test path. The event loop dials + re-dials
    // the bootstrap addrs itself (robust to listen/dial ordering races).
    let (_tx, rx) = mpsc::unbounded_channel::<Control>();
    event_loop(swarm, engine, bootstrap, rx, serve, world, None).await;
    Ok(())
}

/// Like [`run`] (real TCP transport), but reports each bound listen [`Multiaddr`]
/// (already `/p2p/<peerid>`-suffixed) on `listen_tx` as the swarm binds — so a
/// test that listens on an ephemeral port (`/ip4/127.0.0.1/tcp/0`) can discover
/// the actual dialable address + PeerId to bootstrap a second node off. The
/// transport, behaviour, and event loop are IDENTICAL to [`run`]; only the
/// addr-reporting hook differs (the real-TCP swarm integration test path).
pub async fn run_tcp_reporting(
    signing_key: SigningKey,
    listen: Multiaddr,
    bootstrap: Vec<Multiaddr>,
    serve: Option<ServeConfig>,
    world: WorldConfig,
    rx: mpsc::UnboundedReceiver<Control>,
    listen_tx: mpsc::UnboundedSender<Multiaddr>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let key = libp2p_key(&signing_key);
    let tcp = libp2p::tcp::tokio::Transport::new(libp2p::tcp::Config::default())
        .upgrade(Version::V1)
        .authenticate(noise::Config::new(&key)?)
        .multiplex(yamux::Config::default())
        .boxed();
    let mut swarm = build_swarm(key, tcp)?;
    swarm.listen_on(listen).map_err(|e| NetError::Listen(e.to_string()))?;
    let engine = Engine::new_with_config(signing_key, world);
    event_loop(swarm, engine, bootstrap, rx, serve, world, Some(listen_tx)).await;
    Ok(())
}

/// Run a node on a caller-built transport (the test passes the in-memory one).
/// `serve` gates the server/accumulator role exactly as [`run`].
pub async fn run_on_transport(
    signing_key: SigningKey,
    transport: Boxed<(PeerId, StreamMuxerBox)>,
    listen: Multiaddr,
    bootstrap: Vec<Multiaddr>,
    rx: mpsc::UnboundedReceiver<Control>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_on_transport_with(
        signing_key,
        transport,
        listen,
        bootstrap,
        rx,
        None,
        WorldConfig::DEFAULT,
    )
    .await
}

/// [`run_on_transport`] with an explicit server/accumulator [`ServeConfig`] and
/// per-world [`WorldConfig`] (decay/hall/costs + seed bootstrap-flock size) —
/// the HTTP integration test uses this to spawn the watch face in-process on a
/// bound ephemeral port. Pass [`WorldConfig::DEFAULT`] for the engine defaults.
pub async fn run_on_transport_with(
    signing_key: SigningKey,
    transport: Boxed<(PeerId, StreamMuxerBox)>,
    listen: Multiaddr,
    bootstrap: Vec<Multiaddr>,
    rx: mpsc::UnboundedReceiver<Control>,
    serve: Option<ServeConfig>,
    world: WorldConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let key = libp2p_key(&signing_key);
    let mut swarm = build_swarm(key, transport)?;
    swarm.listen_on(listen).map_err(|e| NetError::Listen(e.to_string()))?;
    let engine = Engine::new_with_config(signing_key, world);
    event_loop(swarm, engine, bootstrap, rx, serve, world, None).await;
    Ok(())
}

/// Out-of-band control messages the test (or a future UI) sends into the loop.
pub enum Control {
    /// Snapshot the engine state into the reply channel (read-only).
    Snapshot(tokio::sync::oneshot::Sender<Snapshot>),
    /// §10 writes / §6.1 — the HTTP write-face hands a (signature-verified)
    /// inbound [`Envelope`] to the loop to be routed EXACTLY like an inbound
    /// gossip message: `engine.apply` (+ §6 audit routing) AND re-published on
    /// the gossip topic for `env.t`. For a `PieceUpload` it is also fed to the
    /// accumulator (same path as observed pieces). The reply carries the result
    /// + the resulting standing for the submitter. This is the one-code-path
    /// bridge: a browser contribution becomes a swarm contribution (§10 1:1 skin).
    Inject(Envelope, tokio::sync::oneshot::Sender<InjectResult>),
    /// Compute the §10 advisory work hand-out for a worker pubkey (read-only):
    /// least-covered blocks + assigned audit tiles, reusing the engine's
    /// block-selection + audit-assignment logic. Backs `GET /api/assign`.
    Assign(String, u32, tokio::sync::oneshot::Sender<AssignResult>),
    /// Stop the loop.
    Shutdown,
}

/// The outcome of a [`Control::Inject`] — what the `/api/msg` handler reports
/// back. `accepted` is whether the engine applied + the node vouched/gossiped it.
#[derive(Debug, Clone)]
pub struct InjectResult {
    /// True if the message was accepted (signature ok, ingest-audit passed if
    /// on, and `engine.apply` changed state) and re-emitted to the swarm.
    pub accepted: bool,
    /// A human-readable rejection reason when `!accepted` (bad sig is filtered
    /// before injection; this covers audit-mismatch / apply-rejected / unknown).
    pub reason: Option<String>,
    /// The submitter's resulting credits (§3, log-derived) after applying.
    pub credits: u64,
    /// The SUBMITTING KEY's (`env.from`) running total of confirmed tiles (§3,
    /// log-derived) — the numerator of its earned credits
    /// (`confirmed_tiles / TILES_PER_CREDIT`). `0` when unknown / mid-render. The
    /// §10 write-face returns this so a browser contributor can display its
    /// accepted-tile total + progress to the next credit.
    pub confirmed_tiles: u64,
    /// If the message named a sheep, that sheep's resulting coverage (§4.1) and
    /// backing (§2.2) after applying — the standing the §10 skin returns.
    pub sheep_id: Option<String>,
    pub coverage: u64,
    pub backing: u64,
}

/// The outcome of a [`Control::Assign`] — the §10 `AssignResp` shape, built from
/// the engine's block-selection + audit-assignment logic.
#[derive(Debug, Clone)]
pub struct AssignResult {
    /// `(block_wire_id, sheep_hex, frame, idx, pass)` per advisory block —
    /// the work units a worker should render next (least-covered, uncapped).
    pub blocks: Vec<(String, String, u32, u32, u32)>,
    /// Tiles the worker is assigned to audit (§6), as `(sheep_hex, frame, idx, pass)`.
    pub audits: Vec<(String, u32, u32, u32)>,
}

/// A read-only snapshot of engine state for assertions / display.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub self_pub: String,
    pub flock: Vec<String>,
    /// `(sheep_hex, confirmed_coverage)` per known sheep.
    pub coverage: Vec<(String, u64)>,
    /// `(sheep_hex, backing)` per known sheep (§2.2 survival votes tally) — lets a
    /// test assert vote gossip converges (a vote cast on one node raises a sheep's
    /// backing as observed on another).
    pub backing: Vec<(String, u64)>,
    pub total_coverage: u64,
    pub own_confirmed_tiles: u64,
    pub credits: u64,
    /// Live (block_wire, claimant) pairs — to check distinct claims.
    pub live_claims: Vec<(String, String)>,
}

// ---- the shared event loop ---------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn event_loop(
    mut swarm: Swarm<SheepBehaviour>,
    engine: Engine,
    bootstrap: Vec<Multiaddr>,
    mut control: mpsc::UnboundedReceiver<Control>,
    serve: Option<ServeConfig>,
    world: WorldConfig,
    listen_tx: Option<mpsc::UnboundedSender<Multiaddr>>,
) {
    // ---- server/accumulator capability (§1.1) ----------------------------
    // A node given a ServeConfig holds an Accumulator (ingesting observed + own
    // PieceUploads into the merged per-(sheep, frame) CRDT) and serves the
    // read-only HTTP watch face from a shared ReadState snapshot. A plain
    // worker leaves both `None` and never touches the heavy histograms.
    let accumulate = serve.is_some();
    // §6.1 gateway ingest-audit policy for the browser/REST write-face (default
    // off when no serve config; a worker has no write-face). A server's
    // `ServeConfig.ingest_audit` decides whether browser submissions are
    // verify-before-vouch'd or optimistically forwarded.
    let ingest_audit = serve.as_ref().map(|c| c.ingest_audit).unwrap_or(false);
    // The accumulator spills its merged-frame working set under the node's
    // data_dir (the same dir the video cache uses) and bounds RAM by
    // `world.accum_ram_mb` (§5 memory-bounded accumulate). Only a serving node
    // holds one — a plain worker never accumulates the heavy histograms.
    let accum: Option<Arc<Mutex<Accumulator>>> = serve.as_ref().map(|cfg| {
        Arc::new(Mutex::new(Accumulator::new(
            cfg.data_dir.clone(),
            world.accum_ram_mb,
        )))
    });
    let read_state: Option<Arc<Mutex<ReadState>>> =
        accumulate.then(|| Arc::new(Mutex::new(ReadState::default())));
    // Genome registration for tonemap is one-shot per sheep; track who's done.
    let mut registered: HashSet<String> = HashSet::new();
    // Retracted hashes already pushed to the accumulator (§6 fraud removal), so
    // we don't re-scan the whole disputed set every tick.
    let mut retracted_seen: HashSet<String> = HashSet::new();

    // §10 writes: a dedicated HTTP→loop command channel. The HTTP write handlers
    // hand `Control::Inject`/`Control::Assign` down this; the loop selects on it
    // alongside the out-of-band `control` channel (tests / UI). Kept separate so
    // the read-only watch-face path (no `cmd`) is unaffected.
    let (http_cmd_tx, mut http_cmd_rx) = mpsc::unbounded_channel::<Control>();

    if let (Some(cfg), Some(accum), Some(read_state)) =
        (serve.as_ref(), accum.as_ref(), read_state.as_ref())
    {
        let st = HttpState {
            read: read_state.clone(),
            accum: accum.clone(),
            data_dir: cfg.data_dir.clone(),
            n_frames: crate::spec::N_FRAMES,
            cmd: Some(http_cmd_tx.clone()),
        };
        let addr = cfg.http_addr;
        tokio::spawn(async move {
            if let Err(e) = http::serve(addr, st).await {
                eprintln!("[node] http watch face on {addr} stopped: {e}");
            }
        });
        eprintln!(
            "[node] watch face (accumulate + serve) on http://{addr} (ingest-audit {})",
            if ingest_audit { "on" } else { "off" }
        );
    }
    // Drop our retained sender clone if there's no write face, so the receiver
    // can close cleanly; otherwise keep it alive for the HTTP server's lifetime.
    drop(http_cmd_tx);

    // Subscribe to every gossip topic the node speaks (§10).
    for t in gossip_topics() {
        if let Err(e) = swarm
            .behaviour_mut()
            .gossipsub
            .subscribe(&gossip_topics_ident(t))
        {
            eprintln!("subscribe {t} failed: {e}");
        }
    }

    // ---- boot minting, OFF the reactor AND off the boot critical path -----
    // Inject the genesis sheep so peers share something to render (§12-step-2:
    // lifecycle/mint is a later step; this is a fixed demo birth), and — on a
    // SEED — mint the world bootstrap flock.
    //
    // BOTH derive genomes via `Genome::random` (genesis through `engine.apply`,
    // the bootstrap through `bootstrap_seed_flock`). Genome derivation runs a
    // deterministic density filter that, while now bounded/cheap, is still pure
    // CPU — and the bootstrap mints `world.bootstrap_flock` of them. We run it on
    // a blocking thread (`spawn_blocking`), moving the engine in and back out.
    //
    // CRITICAL (swarm-sync fix, root cause #1): we do NOT `.await` the boot-mint
    // here in the preamble. Awaiting it before entering the `select!` loop means
    // the swarm is not polled while it runs — gossipsub subscriptions/heartbeats
    // don't progress, the mesh with a freshly-dialed peer never forms, and a peer
    // that connects during the boot-mint learns NOTHING (the founding sheep are
    // published onto an unformed mesh, dropped as `InsufficientPeers`). Instead we
    // start the loop IMMEDIATELY with the engine checked OUT (engine_slot = None,
    // exactly as during a render tick): the swarm is polled from t=0, the mesh
    // forms, and when the boot-mint task returns we merge the engine back in,
    // register genomes, and publish the founding envelopes onto an ALREADY-FORMED
    // mesh (plus the persistent republish path below keeps re-offering them).
    let genesis = genesis_mint();
    let bootstrap_flock = if accumulate { world.bootstrap_flock } else { 0 };
    // Apply the genesis birth INLINE (cheap: one Mint → one genome derivation).
    // Keeping it synchronous makes the genesis sheep known to the engine the
    // instant the loop starts — so the §6.1 ingest-audit write path (which re-
    // renders the named sheep) and the watch-face snapshot have it immediately,
    // with no boot-mint-window race. Only the HEAVY part — `bootstrap_seed_flock`,
    // which derives `bootstrap_flock` genomes — is deferred to the blocking task
    // below (the swarm-sync fix: that's what could starve the reactor).
    let mut engine = engine;
    {
        let boot_now = now_ms();
        let _ = engine.apply(&genesis, boot_now);
        if accumulate {
            if let (Some(accum), Some(entry)) = (
                accum.as_ref(),
                engine.flock().get(&crate::derive_minted_genesis::genesis_sheep_hex()),
            ) {
                let id = crate::derive_minted_genesis::genesis_sheep_hex();
                accum.lock().unwrap().register_sheep(&id, entry.genome.clone());
                registered.insert(id);
            }
            refresh_read_state(&engine, read_state.as_ref(), boot_now);
        }
    }
    let mut boot_task: Option<tokio::task::JoinHandle<(Engine, Vec<Envelope>)>> = {
        let boot_now = now_ms();
        Some(tokio::task::spawn_blocking(move || {
            let mut engine = engine;
            // A SEED (a serving/accumulator node) mints `bootstrap_flock` LIVE
            // starter sheep at boot so the deployed world is watchable from the
            // first request (the fixed-2023 genesis is already dead under real
            // wall-clock decay). Workers (no serve config) never seed. The
            // births+votes are applied locally and stashed so they (re)publish
            // on every (re)formed mesh, exactly like the genesis birth
            // (FLOCK/VOTES are never re-emitted by the engine). Restart re-seeds
            // with fresh ids — see `bootstrap_seed_flock`.
            let envs = if bootstrap_flock > 0 {
                // 8 self-votes/sheep: with the recalibrated decay this gives the
                // founding flock a comfortable lifetime (DEFAULT ~44 min, Sandbox
                // ~2 min, Gallery hours) so a fresh world shows a LIVE, persisting
                // flock from the first request instead of sheep that die in seconds.
                engine.bootstrap_seed_flock(bootstrap_flock, 8, boot_now)
            } else {
                Vec::new()
            };
            (engine, envs)
        }))
    };
    // The founding envelopes (genesis birth + bootstrap mints/votes) the boot-mint
    // task produces — populated when `boot_task` returns. These are re-published
    // PERSISTENTLY (FLOCK/VOTES are never re-emitted by the engine) so any peer
    // that connects — before, during, or after the boot-mint — eventually learns
    // the founding flock once the mesh forms.
    let mut founding_envs: Vec<Envelope> = vec![genesis.clone()];

    // Re-dial bootstrap on a timer until we have at least one peer. This makes
    // connection robust to listen/dial ordering: an in-memory dial that races
    // ahead of the other node's `listen_on` just fails and is retried, instead
    // of leaving the mesh permanently unformed (the step-2 flakiness cause).
    let mut redial = tokio::time::interval(Duration::from_millis(250));

    // Founding re-publish: FLOCK/VOTES are never re-emitted by the engine, so we
    // must (re)publish the founding births+votes whenever the mesh (re)forms. We
    // do this on every new connection, on every relevant SUBSCRIBE (above), AND on
    // a persistent throttled timer while connected — so a peer that joins late, or
    // whose mesh formed after our first publish, still learns the founding flock.
    // The timer is throttled (700ms) and, to avoid a long-running node spamming
    // the demo birth forever, only fires while within a bounded window since the
    // FIRST connection formed (re-armed each time we (re)connect from zero peers).
    let mut founding_retry = tokio::time::interval(Duration::from_millis(700));
    let republish_window = Duration::from_secs(30);
    let mut first_connect_at: Option<tokio::time::Instant> = None;

    // Per-node tick-phase stagger. Two peers that tick in lockstep both pick the
    // SAME least-covered block every round (each other's claim gossip hasn't
    // landed yet) — a harmless duplicate render (§4), but it means the soft-claim
    // *collision avoidance* never gets a chance to diverge them. Offsetting each
    // node's tick phase by a small key-derived amount guarantees one peer's claim
    // reliably reaches the other before that peer claims, so they take DISTINCT
    // blocks (the §4 property under test). Derived from the pubkey so it's stable
    // and distinct per node, bounded well under TICK_INTERVAL.
    let phase_offset = {
        let b = swarm
            .local_peer_id()
            .to_bytes()
            .first()
            .copied()
            .unwrap_or(0);
        Duration::from_millis((b as u64 % 8) * 90) // 0..630ms, < 1s tick
    };
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + phase_offset + TICK_INTERVAL,
        TICK_INTERVAL,
    );
    // Track which (sheep,frame,idx) we've already routed to audit, so the
    // step-2 placeholder doesn't re-enqueue the same tile every gossip hit.
    let mut audited_seen: HashSet<(String, u32, u32, u32)> = HashSet::new();

    // `engine.tick()` RENDERS (heavy CPU: a 16-tile block + any audits), which
    // would block the async reactor — starving gossip and snapshots for whole
    // seconds (catastrophic in a debug build). So we offload each tick to a
    // blocking thread (`spawn_blocking`), moving the engine in and back out, and
    // keep servicing the swarm meanwhile. Inbound envelopes that arrive while a
    // tick is in flight are buffered and drained through `apply` when the engine
    // returns — so no gossip is lost during a render.
    // The engine starts CHECKED OUT — the boot-mint task holds it (see the
    // boot-mint comment above). It is merged back into `engine_slot` when
    // `boot_task` returns, exactly as a render tick returns it.
    let mut engine_slot: Option<Engine> = None;
    let mut tick_task: Option<tokio::task::JoinHandle<(Engine, Vec<Envelope>)>> = None;
    let mut pending_inbound: Vec<(Envelope, u64)> = Vec::new();
    // §5 heavy artifacts (PieceUploads) awaiting ingest into the accumulator —
    // both observed-from-gossip and this node's own renders. Buffered and drained
    // whenever the engine is in hand (genome registration for tonemap needs the
    // engine's flock). Empty + unused on a plain worker (no accumulate gate).
    let mut pending_pieces: Vec<PieceUpload> = Vec::new();
    // A cached snapshot, refreshed whenever we hold the engine. `Control::Snapshot`
    // is answered from this even while a render owns the engine on its blocking
    // thread — otherwise, since the engine renders almost continuously, snapshot
    // requests would (correctly) be dropped and a poller would never see state.
    // Empty until the boot-mint returns the engine (the first snapshot after that
    // carries the founding flock).
    let mut last_snapshot = Snapshot {
        self_pub: String::new(),
        flock: Vec::new(),
        coverage: Vec::new(),
        backing: Vec::new(),
        total_coverage: 0,
        own_confirmed_tiles: 0,
        credits: 0,
        live_claims: Vec::new(),
    };

    // §10 convergence — a transport-side mirror of the engine's `birth_log`
    // (every accepted Mint/Breed + Vote envelope), refreshed whenever we hold the
    // engine. We answer an inbound flock-sync REQUEST from this cache so a peer
    // always gets our full known flock even while a render owns the engine on its
    // blocking thread — otherwise, since the engine renders almost continuously in
    // a debug build, most flock-sync requests would (correctly) be answered empty
    // and a late joiner could miss births. Bounded to the most-recent
    // `FLOCK_SYNC_MAX`. Cheap: only re-cloned when a new birth/vote was applied.
    let mut birth_cache: Vec<Envelope> = Vec::new();

    // Mesh-readiness gate. The engine's outbound (Coverage / Attestation / Claim)
    // is ONE-SHOT — a `publish` dropped because the gossipsub mesh hasn't formed
    // for that topic is lost, and the engine won't re-emit that exact tile (it
    // completed the block). So a peer that starts rendering before the remote has
    // subscribed to `/sheep/progress` can publish its first coverage into the void
    // → the other peer never audits it → the loop can stall. We therefore hold off
    // the FIRST render tick until we've seen a remote peer subscribe to our
    // PROGRESS topic (so a publish there will be delivered). If there are no peers
    // at all (solo / bootstrap node), we render anyway after a short grace so a
    // lone node still makes progress.
    let progress_topic_hash = topic_for(proto::PROGRESS).hash();
    let flock_topic_hash = topic_for(proto::FLOCK).hash();
    let votes_topic_hash = topic_for(proto::VOTES).hash();
    let mut progress_meshed = false;
    let start = tokio::time::Instant::now();

    loop {
        let connected = swarm.connected_peers().count() > 0;
        // Ready to render once the PROGRESS topic is meshed with a peer, OR we've
        // waited out a short grace with no peers (solo progress), OR a longer grace
        // has elapsed regardless (never refuse to render forever — the mesh should
        // form in seconds; if it somehow hasn't, render and let the republish path
        // and continued gossip catch up rather than deadlock).
        let elapsed = start.elapsed();
        let solo_grace = !connected && elapsed > Duration::from_secs(3);
        let hard_grace = elapsed > Duration::from_secs(15);
        let render_ready = progress_meshed || solo_grace || hard_grace;
        // Kick off a tick if the interval has elapsed and the engine is idle
        // (not already rendering). We poll the interval non-blockingly below via
        // `ticker.tick()` in the select, and start the task here when ready.
        tokio::select! {
            // ---- the boot-mint finished: merge the engine back in, register the
            // founding genomes with the accumulator, seed the watch-face snapshot,
            // and stash the founding envelopes for the persistent republish path.
            // This runs CONCURRENTLY with the swarm being polled (the fix for root
            // cause #1: a peer dialing during the boot-mint forms its mesh now, and
            // the founding sheep are published below onto that formed mesh).
            res = async { boot_task.as_mut().unwrap().await }, if boot_task.is_some() => {
                boot_task = None;
                match res {
                    Ok((eng, mut bootstrap_envs)) => {
                        if bootstrap_flock > 0 {
                            eprintln!(
                                "[node] world bootstrap: minted {} live starter sheep ({} envelopes)",
                                bootstrap_flock,
                                bootstrap_envs.len()
                            );
                        }
                        // Register every founding sheep's genome with the accumulator
                        // so tonemap works as their tiles land, and seed the watch
                        // face's snapshot once up front.
                        if accumulate {
                            if let Some(accum) = accum.as_ref() {
                                let mut acc = accum.lock().unwrap();
                                for (id, entry) in eng.flock().iter() {
                                    if registered.insert(id.clone()) {
                                        acc.register_sheep(id, entry.genome.clone());
                                    }
                                }
                            }
                            refresh_read_state(&eng, read_state.as_ref(), now_ms());
                        }
                        // Founding set = genesis birth + all bootstrap mint/vote
                        // envelopes. Published now (mesh has likely formed) AND on
                        // the persistent republish timer below.
                        founding_envs.append(&mut bootstrap_envs);
                        if connected {
                            for env in &founding_envs {
                                publish_env(&mut swarm, env);
                            }
                        }
                        last_snapshot = snapshot(&eng);
                        refresh_birth_cache(&eng, &mut birth_cache);
                        engine_slot = Some(eng);
                    }
                    Err(e) => {
                        eprintln!("boot-mint task panicked: {e}");
                        return;
                    }
                }
            }
            // ---- a render tick finished: reclaim the engine, drain buffered
            // inbound, and publish the tick's outbound envelopes.
            res = async { tick_task.as_mut().unwrap().await }, if tick_task.is_some() => {
                tick_task = None;
                match res {
                    Ok((mut eng, outbound)) => {
                        // Drain inbound that arrived during the render (apply +
                        // audit-route each, so attestations flow on the next tick).
                        let now = now_ms();
                        let buffered: Vec<(Envelope, u64)> = pending_inbound.drain(..).collect();
                        for (env, _ts) in buffered {
                            apply_inbound(&mut eng, &env, now, &mut audited_seen);
                        }
                        // §5 accumulate: this node's OWN renders (PIECE envelopes
                        // the tick emitted) are heavy artifacts to ingest too.
                        if accumulate {
                            for env in &outbound {
                                if env.t == proto::PIECE {
                                    if let Ok(p) =
                                        serde_json::from_value::<PieceUpload>(env.body.clone())
                                    {
                                        pending_pieces.push(p);
                                    }
                                }
                            }
                        }
                        for env in &outbound {
                            publish_env(&mut swarm, env);
                        }
                        // Drain buffered pieces + refresh the watch-face snapshot
                        // while the engine is in hand (genome lookup for tonemap).
                        if accumulate {
                            drain_pieces(&eng, accum.as_ref(), &mut pending_pieces, &mut registered);
                            sync_retractions(&eng, accum.as_ref(), &mut retracted_seen);
                            refresh_read_state(&eng, read_state.as_ref(), now);
                        }
                        last_snapshot = snapshot(&eng);
                        refresh_birth_cache(&eng, &mut birth_cache);
                        engine_slot = Some(eng);
                    }
                    Err(e) => {
                        eprintln!("tick task panicked: {e}");
                        // Engine is lost on panic; nothing safe to do but stop.
                        return;
                    }
                }
            }
            ev = swarm.select_next_some() => {
                if let SwarmEvent::NewListenAddr { address, .. } = &ev {
                    // The full dialable multiaddr a peer/seed bootstraps off.
                    let full: Multiaddr = address
                        .clone()
                        .with(libp2p::multiaddr::Protocol::P2p(*swarm.local_peer_id()));
                    eprintln!("[node] listening: {full}");
                    // Report the bound addr so a test on an ephemeral port can
                    // discover the actual dialable addr + PeerId to bootstrap off.
                    if let Some(tx) = listen_tx.as_ref() {
                        let _ = tx.send(full);
                    }
                }
                if let SwarmEvent::OutgoingConnectionError { error, peer_id, .. } = &ev {
                    eprintln!("[node] dial error to {peer_id:?}: {error}");
                }
                if let SwarmEvent::ConnectionEstablished { peer_id, .. } = &ev {
                    // (Re-)arm the persistent founding-republish window whenever a
                    // connection forms while we had no peers (a fresh peer joining) —
                    // so a late joiner gets the founding flock, without a long-lived
                    // node republishing the demo birth forever between joins. (`connected`
                    // is the pre-event reading, so `!connected` here = a fresh join.)
                    if !connected {
                        first_connect_at = Some(tokio::time::Instant::now());
                    }
                    // Publish whatever founding envelopes we have so far (genesis
                    // always; bootstrap mints once the boot-mint task returns). The
                    // mesh may not be formed at this exact instant — that's fine, the
                    // SUBSCRIBE-triggered + persistent republish paths below cover it.
                    for env in &founding_envs {
                        publish_env(&mut swarm, env);
                    }
                    // §10 convergence: ask the freshly-connected peer for its full
                    // birth log (catch-up). Births/votes are one-shot gossip the
                    // engine never re-emits, so without this a node that joins AFTER
                    // a birth never learns it. We re-verify every returned envelope
                    // (`engine.apply`), so the responder can't inject a sheep — this
                    // is purely "tell me the births I may have missed." Requested on
                    // EVERY connection (cheap, idempotent: the engine dedups already-
                    // known births), which also covers reconnect re-sync.
                    swarm
                        .behaviour_mut()
                        .flock_sync
                        .send_request(peer_id, FlockSyncRequest {});
                    if std::env::var("SHEEP_DEBUG").is_ok() {
                        eprintln!("[node] ConnectionEstablished → flock-sync request to {peer_id}");
                    }
                }
                // A remote subscribing to our PROGRESS topic means a publish there
                // will now be delivered — the signal that it's safe to render.
                if let SwarmEvent::Behaviour(SheepBehaviourEvent::Gossipsub(
                    gossipsub::Event::Subscribed { topic, .. },
                )) = &ev
                {
                    if std::env::var("SHEEP_DEBUG").is_ok() {
                        eprintln!("[node] Subscribed topic={topic:?}");
                    }
                    if *topic == progress_topic_hash {
                        progress_meshed = true;
                    }
                    // A remote just subscribed to our FLOCK/VOTES topics — NOW a
                    // publish there is delivered to it (the gossipsub publish/mesh
                    // timing fix, root cause #2). Re-publish the founding envelopes
                    // so a freshly-joined peer learns the founding sheep regardless
                    // of who connected/published first. FLOCK/VOTES are never re-
                    // emitted by the engine, so this republish is load-bearing.
                    if *topic == flock_topic_hash || *topic == votes_topic_hash {
                        for env in &founding_envs {
                            publish_env(&mut swarm, env);
                        }
                    }
                }
                // §10 convergence — flock-sync req/resp. A peer asks for our birth
                // log (we answer from the engine's `birth_log`); or a peer answers
                // ours (we re-apply each envelope — re-verified by `engine.apply` —
                // so a late joiner converges to the full flock). This CONSUMES the
                // event (it owns the response channel); only non-flock-sync events
                // fall through to the generic `ingest_event`.
                if let SwarmEvent::Behaviour(SheepBehaviourEvent::FlockSync(
                    request_response::Event::Message { message, .. },
                )) = ev
                {
                    match message {
                        request_response::Message::Request { channel, .. } => {
                            // Answer from the transport-side `birth_cache` mirror so
                            // we serve the FULL known flock even while a render owns
                            // the engine on its blocking thread (the engine itself
                            // isn't readable then). The cache is refreshed whenever we
                            // hold the engine, so it is at most one render stale —
                            // harmless, since births only ever accrue (a late joiner
                            // catches any just-missed birth on its next connection).
                            if std::env::var("SHEEP_DEBUG").is_ok() {
                                eprintln!("[node] flock-sync request → responding {} births", birth_cache.len());
                            }
                            let _ = swarm
                                .behaviour_mut()
                                .flock_sync
                                .send_response(channel, FlockSyncResponse { births: birth_cache.clone() });
                        }
                        request_response::Message::Response { response, .. } => {
                            // Re-apply each returned birth/vote envelope. Trustless:
                            // `apply` re-verifies signature + re-derives the genome,
                            // and dedups by canonical bytes — so already-known births
                            // are no-ops and a forged envelope is rejected.
                            let now = now_ms();
                            if std::env::var("SHEEP_DEBUG").is_ok() {
                                eprintln!("[node] flock-sync response ← {} births", response.births.len());
                            }
                            for env in &response.births {
                                match engine_slot.as_mut() {
                                    Some(eng) => apply_inbound(eng, env, now, &mut audited_seen),
                                    None => pending_inbound.push((env.clone(), now)),
                                }
                            }
                            if accumulate {
                                if let Some(eng) = engine_slot.as_ref() {
                                    refresh_read_state(eng, read_state.as_ref(), now);
                                }
                            }
                        }
                    }
                    continue;
                }
                ingest_event(
                    engine_slot.as_mut(),
                    &mut pending_inbound,
                    ev,
                    &mut audited_seen,
                    accumulate.then_some(&mut pending_pieces),
                );
                // If the engine is in hand (no render in flight), keep the §10
                // flock-sync birth cache fresh (so a birth just learned via gossip
                // is served onward to the next late joiner — this is what relays A's
                // sheep through B to C). Cheap: a no-op unless a new birth/vote was
                // just applied.
                if let Some(eng) = engine_slot.as_ref() {
                    refresh_birth_cache(eng, &mut birth_cache);
                }
                // If the engine is in hand (no render in flight), drain any newly
                // buffered pieces + refresh the snapshot immediately so the watch
                // face reflects gossip even on a node that isn't actively rendering.
                if accumulate {
                    if let Some(eng) = engine_slot.as_ref() {
                        drain_pieces(eng, accum.as_ref(), &mut pending_pieces, &mut registered);
                        sync_retractions(eng, accum.as_ref(), &mut retracted_seen);
                        refresh_read_state(eng, read_state.as_ref(), now_ms());
                    }
                }
            }
            _ = ticker.tick(), if render_ready && engine_slot.is_some() && tick_task.is_none() => {
                // Take the engine and render on a blocking thread.
                let mut eng = engine_slot.take().expect("engine present");
                let now = now_ms();
                tick_task = Some(tokio::task::spawn_blocking(move || {
                    let out = eng.tick(now);
                    (eng, out)
                }));
            }
            _ = redial.tick(), if !connected => {
                for addr in &bootstrap {
                    let _ = swarm.dial(addr.clone());
                }
            }
            _ = founding_retry.tick(), if connected && first_connect_at
                .map(|t| t.elapsed() < republish_window)
                .unwrap_or(true) =>
            {
                for env in &founding_envs {
                    publish_env(&mut swarm, env);
                }
            }
            // Out-of-band control (tests / UI) and the HTTP write face share one
            // `Control` dispatch — `dispatch_control` routes Inject through the
            // SAME apply+gossip path inbound gossip uses (the §10 1:1 property).
            ctl = control.recv() => {
                let shutdown = match ctl {
                    Some(c) => dispatch_control(
                        c, &mut swarm, &mut engine_slot, &mut pending_inbound,
                        &mut audited_seen, accum.as_ref(), read_state.as_ref(),
                        &mut pending_pieces, &mut registered, &mut retracted_seen,
                        accumulate, ingest_audit, &mut last_snapshot,
                    ),
                    None => true,
                };
                if shutdown { return; }
                // An Inject may have applied a user-mint birth — keep the flock-sync
                // cache current so the new sheep is served to late joiners.
                if let Some(eng) = engine_slot.as_ref() {
                    refresh_birth_cache(eng, &mut birth_cache);
                }
            }
            ctl = http_cmd_rx.recv() => {
                // `None` here just means no write face is attached — keep running.
                if let Some(c) = ctl {
                    let shutdown = dispatch_control(
                        c, &mut swarm, &mut engine_slot, &mut pending_inbound,
                        &mut audited_seen, accum.as_ref(), read_state.as_ref(),
                        &mut pending_pieces, &mut registered, &mut retracted_seen,
                        accumulate, ingest_audit, &mut last_snapshot,
                    );
                    if shutdown { return; }
                    if let Some(eng) = engine_slot.as_ref() {
                        refresh_birth_cache(eng, &mut birth_cache);
                    }
                }
            }
        }
    }
}

/// Dispatch one [`Control`] message. Shared by the out-of-band control channel
/// and the HTTP write-face channel so both go through the SAME code path (the
/// §10 1:1 property: an `/api/msg` Inject is routed exactly like inbound gossip
/// — `engine.apply` + §6 audit-route + gossip re-publish + accumulator ingest).
/// Returns `true` if the loop should shut down.
#[allow(clippy::too_many_arguments)]
fn dispatch_control(
    ctl: Control,
    swarm: &mut Swarm<SheepBehaviour>,
    engine_slot: &mut Option<Engine>,
    pending_inbound: &mut Vec<(Envelope, u64)>,
    audited_seen: &mut HashSet<(String, u32, u32, u32)>,
    accum: Option<&Arc<Mutex<Accumulator>>>,
    read_state: Option<&Arc<Mutex<ReadState>>>,
    pending_pieces: &mut Vec<PieceUpload>,
    registered: &mut HashSet<String>,
    retracted_seen: &mut HashSet<String>,
    accumulate: bool,
    ingest_audit: bool,
    last_snapshot: &mut Snapshot,
) -> bool {
    match ctl {
        Control::Snapshot(reply) => {
            // Always answerable: refresh from the in-hand engine if present;
            // else serve the last settled snapshot (a render owns the engine).
            // Progress is monotonic, so a slightly-stale read is never a false
            // positive.
            if let Some(eng) = engine_slot.as_ref() {
                *last_snapshot = snapshot(eng);
            }
            let _ = reply.send(last_snapshot.clone());
        }
        Control::Inject(env, reply) => {
            // §10 writes / §6.1: route the verified envelope EXACTLY like inbound
            // gossip — apply (+ §6 audit route) AND re-publish (vouch) — through
            // the SAME helpers the libp2p path uses. If a render owns the engine,
            // the apply is buffered for the tick-return drain (the standing read
            // back is pre-apply, which is monotonic → never a false positive),
            // while the gossip re-emit happens regardless.
            let now = now_ms();
            let result = inject_envelope(
                swarm,
                engine_slot.as_mut(),
                pending_inbound,
                audited_seen,
                accumulate.then_some(&mut *pending_pieces),
                ingest_audit,
                &env,
                now,
            );
            if accumulate {
                if let Some(eng) = engine_slot.as_ref() {
                    drain_pieces(eng, accum, pending_pieces, registered);
                    sync_retractions(eng, accum, retracted_seen);
                    refresh_read_state(eng, read_state, now);
                }
            }
            let _ = reply.send(result);
        }
        Control::Assign(worker_pub, want, reply) => {
            // §10 advisory hand-out (read-only). Answerable from the in-hand
            // engine; if a render owns it, answer empty (the browser retries —
            // advisory, never load-bearing).
            let now = now_ms();
            let result = match engine_slot.as_ref() {
                Some(eng) => {
                    let (blocks, audits) = eng.assign_for(&worker_pub, want, now);
                    AssignResult {
                        blocks: blocks
                            .iter()
                            .flat_map(|b| {
                                let sheep = b.sheep_hex();
                                crate::block::block_units(*b).into_iter().map(move |u| {
                                    (b.to_wire(), sheep.clone(), u.frame, u.idx, u.pass)
                                })
                            })
                            .collect(),
                        audits: audits
                            .iter()
                            .map(|c| (c.sheep_id.clone(), c.frame, c.idx, c.pass))
                            .collect(),
                    }
                }
                None => AssignResult { blocks: Vec::new(), audits: Vec::new() },
            };
            let _ = reply.send(result);
        }
        Control::Shutdown => return true,
    }
    false
}

fn gossip_topics_ident(t: &str) -> gossipsub::IdentTopic {
    topic_for(t)
}

/// Refresh the transport-side flock-sync birth cache from the engine's birth log
/// (§10 convergence). Keeps the bounded most-recent tail so an inbound flock-sync
/// request is answerable even when a render later checks the engine out. Cheap: a
/// no-op unless a new birth/vote was applied (the engine's log is append-only +
/// deduped), so we only re-clone when the length changed.
fn refresh_birth_cache(engine: &Engine, cache: &mut Vec<Envelope>) {
    let log = engine.birth_log();
    if log.len() == cache.len() {
        return;
    }
    let start = log.len().saturating_sub(FLOCK_SYNC_MAX);
    *cache = log[start..].to_vec();
}

/// Build a read-only snapshot from the engine.
fn snapshot(engine: &Engine) -> Snapshot {
    let flock: Vec<String> = engine.flock().keys().cloned().collect();
    let coverage: Vec<(String, u64)> = flock
        .iter()
        .map(|s| (s.clone(), engine.coverage(s)))
        .collect();
    let backing: Vec<(String, u64)> = flock
        .iter()
        .map(|s| (s.clone(), engine.backing(s)))
        .collect();
    let live_claims: Vec<(String, String)> = engine
        .live_claims()
        .iter()
        .map(|(k, c)| (k.clone(), c.claimant.clone()))
        .collect();
    Snapshot {
        self_pub: engine.self_pub().to_string(),
        flock,
        coverage,
        backing,
        total_coverage: engine.total_coverage(),
        own_confirmed_tiles: engine.own_confirmed_tiles(),
        credits: engine.credits(),
        live_claims,
    }
}

/// Drain buffered [`PieceUpload`]s into the accumulator (§5 accumulate). For
/// each piece: register the sheep's genome if known (needed only by tonemap), then
/// ingest (the accumulator re-verifies content integrity — hash == hash(bytes) —
/// and rejects mismatches with no render). A piece for a sheep we don't yet know
/// is dropped (its `Mint`/`Breed` birth hasn't propagated): coverage/density only
/// counts sheep whose genome is verified, mirroring the engine's `apply_coverage`.
fn drain_pieces(
    engine: &Engine,
    accum: Option<&Arc<Mutex<Accumulator>>>,
    pending: &mut Vec<PieceUpload>,
    registered: &mut HashSet<String>,
) {
    let Some(accum) = accum else {
        pending.clear();
        return;
    };
    if pending.is_empty() {
        return;
    }
    let mut acc = accum.lock().unwrap();
    for piece in pending.drain(..) {
        let Some(entry) = engine.flock().get(&piece.sheep_id) else {
            continue; // unknown sheep (birth not yet learned) — drop the piece.
        };
        let edge = entry.resolution.edge() as usize;
        if registered.insert(piece.sheep_id.clone()) {
            acc.register_sheep(&piece.sheep_id, entry.genome.clone());
        }
        acc.ingest(&piece, edge);
    }
}

/// Propagate the engine's dispute-proven fraudulent content hashes (§6) into the
/// accumulator as keyed CRDT removals, so a fraudster's merged contribution is
/// subtracted from every frame it poisoned. Idempotent (tracks what's applied).
fn sync_retractions(
    engine: &Engine,
    accum: Option<&Arc<Mutex<Accumulator>>>,
    seen: &mut HashSet<String>,
) {
    let Some(accum) = accum else { return };
    let retracted = engine.retracted_hashes();
    let fresh: Vec<&str> = retracted
        .iter()
        .filter(|h| !seen.contains(*h))
        .map(|h| h.as_str())
        .collect();
    if fresh.is_empty() {
        return;
    }
    let mut acc = accum.lock().unwrap();
    acc.apply_disputes(fresh.iter().copied());
    for h in fresh {
        seen.insert(h.to_string());
    }
}

/// Refresh the HTTP watch face's [`ReadState`] from the engine's live state
/// (§2.2/§2.3/§2.4) + the accumulator-backed coverage. All values are projections
/// of the same state the gossip layer carries — a read cache, never a second
/// source of truth (§10).
fn refresh_read_state(
    engine: &Engine,
    read_state: Option<&Arc<Mutex<ReadState>>>,
    now: u64,
) {
    let Some(read_state) = read_state else { return };
    let live: std::collections::HashMap<String, SheepView> = engine
        .live_flock(now)
        .iter()
        .map(|(id, e)| {
            (
                id.clone(),
                SheepView {
                    id: id.clone(),
                    edge: e.resolution.edge(),
                    backing: engine.backing(id),
                    vitality: engine.vitality(id, now).unwrap_or(0.0),
                    coverage: engine.coverage(id),
                    creator: e.creator.clone(),
                    parents: e.parents.clone(),
                    birth_ms: e.birth_ms,
                    // §10 contribute: expose the genome so the browser worker can
                    // render this sheep's tiles. `to_value` of a Genome (a plain
                    // serde struct) cannot fail; fall back to Null defensively.
                    genome: serde_json::to_value(&e.genome).unwrap_or(serde_json::Value::Null),
                },
            )
        })
        .collect();
    let hall: Vec<HallView> = engine
        .hall()
        .iter()
        .map(|h| HallView {
            id: h.sheep_id.clone(),
            edge: h.resolution.edge(),
            birth_ms: h.birth_ms,
            death_ms: h.death_ms,
            lifespan_ms: h.lifespan_ms,
            peak_backing: h.peak_backing,
        })
        .collect();
    let mut rs = read_state.lock().unwrap();
    rs.self_pub = engine.self_pub().to_string();
    rs.live = live;
    rs.hall = hall;
    rs.now_ms = now;
}

/// Publish one signed engine envelope on the gossip topic for its `env.t`.
/// `NoPeersSubscribedToTopic` (no mesh yet) is swallowed — the engine re-emits claims
/// /heartbeats on the next tick, and genesis has its own retry.
fn publish_env(swarm: &mut Swarm<SheepBehaviour>, env: &Envelope) {
    let bytes = match serde_json::to_vec(env) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("serialize envelope failed: {e}");
            return;
        }
    };
    let topic = topic_for(&env.t);
    match swarm.behaviour_mut().gossipsub.publish(topic, bytes) {
        Ok(_) => {}
        Err(gossipsub::PublishError::NoPeersSubscribedToTopic) => {
            if std::env::var("SHEEP_DEBUG").is_ok() {
                eprintln!("[node] DROP publish {} (no peers subscribed)", env.t);
            }
        }
        Err(gossipsub::PublishError::Duplicate) => {}
        Err(gossipsub::PublishError::AllQueuesFull(_)) => {}
        Err(e) => eprintln!("publish {} failed: {e}", env.t),
    }
}

/// Pull an [`Envelope`] out of a swarm event (gossip message), then route it:
/// if the engine is available, [`apply_inbound`] it now; otherwise buffer it for
/// draining when the in-flight render tick returns the engine — so no gossip is
/// lost while a render holds the engine on a blocking thread.
///
/// identify + request-response events are handled implicitly (peer discovery /
/// protocol advertisement); req/resp upload + assign handlers are step-2 stubs
/// (no inbound requests are issued yet).
fn ingest_event(
    engine: Option<&mut Engine>,
    pending: &mut Vec<(Envelope, u64)>,
    event: SwarmEvent<SheepBehaviourEvent>,
    audited_seen: &mut HashSet<(String, u32, u32, u32)>,
    pending_pieces: Option<&mut Vec<PieceUpload>>,
) {
    let SwarmEvent::Behaviour(SheepBehaviourEvent::Gossipsub(gossipsub::Event::Message {
        message,
        ..
    })) = event
    else {
        return;
    };
    let env: Envelope = match serde_json::from_slice(&message.data) {
        Ok(e) => e,
        Err(_) => return, // not an Envelope; ignore
    };
    // §5 heavy artifact: a verified PIECE envelope carries a tile histogram for
    // the accumulator to ingest (content-integrity is re-checked on ingest). We
    // buffer it for drain when the engine is in hand (genome registration). The
    // signature is checked here so an accumulator never ingests unsigned bytes;
    // the accumulator's own gate (hash == hash(bytes)) is the content guard.
    if env.t == proto::PIECE {
        if let Some(buf) = pending_pieces {
            if env.verify() {
                if let Ok(p) = serde_json::from_value::<PieceUpload>(env.body.clone()) {
                    buf.push(p);
                }
            }
        }
        return; // PIECE is not an engine `apply` message (heavy data path).
    }
    let now = now_ms();
    match engine {
        // Engine available: apply + route audits immediately.
        Some(eng) => apply_inbound(eng, &env, now, audited_seen),
        // Engine checked out for a render: buffer; the drain on tick-return runs
        // `apply_inbound` for each. (Audit routing is fine to defer — it only
        // affects the NEXT tick's attestations.)
        None => pending.push((env, now)),
    }
}

/// Apply one inbound envelope to the engine, plus **§6 unpredictable,
/// verifiable audit assignment**: when we observe ANOTHER peer's `Coverage`, we
/// enqueue the tile for audit **only if this node is assigned to it** under
/// [`Engine::is_assigned`] — `sha256(self_pub ‖ tile ‖ round_salt) <
/// threshold(sample_rate(submitter_rep))`. The auditor cannot choose which tiles
/// it audits (the hash binds its own pubkey + a salt it doesn't control), and
/// any node can re-verify the same assignment. The submitter (`env.from`) is the
/// peer whose Coverage we observed; their log-derived rep graduates the sample
/// rate (new/zero-rep → near-certain audit; trusted → light, floored at 5%).
fn apply_inbound(
    engine: &mut Engine,
    env: &Envelope,
    now: u64,
    audited_seen: &mut HashSet<(String, u32, u32, u32)>,
) {
    if env.t == proto::PROGRESS && env.from != engine.self_pub() {
        if let Ok(cov) = serde_json::from_value::<Coverage>(env.body.clone()) {
            // Bound-check; a malformed peer Coverage is simply not audited (the
            // engine's `apply` rejects it too).
            let in_range =
                cov.frame < crate::spec::N_FRAMES && cov.idx < crate::spec::IDXS_PER_FRAME;
            let key = (cov.sheep_id.clone(), cov.frame, cov.idx, cov.pass);
            let tile = (cov.sheep_id.as_str(), cov.frame, cov.idx, cov.pass);
            // Assigned-to-this-auditor gate (§6) — unpredictable + unselectable.
            let assigned = in_range
                && engine.is_assigned(engine.self_pub(), tile, &env.from);
            if assigned && audited_seen.insert(key) {
                engine.enqueue_audit(Coverage {
                    sheep_id: cov.sheep_id,
                    frame: cov.frame,
                    idx: cov.idx,
                    pass: cov.pass,
                    hash: String::new(),
                });
            }
        }
    }
    let _ = engine.apply(env, now);
}

/// §10 writes / §6.1 — inject a (signature-verified) browser/REST envelope into
/// the swarm through the SAME code path inbound gossip uses, with the gateway
/// ingest-audit policy applied first. Returns an [`InjectResult`] (accepted +
/// resulting standing) for the `/api/msg` response.
///
/// The 1:1 property (§10): routing is by `env.t` via [`apply_inbound`] (which
/// calls `engine.apply` exactly as gossip does) + [`publish_env`] (gossip re-
/// emit / vouch). A `PieceUpload` is additionally buffered for the accumulator,
/// the same path observed pieces take. NO per-type write semantics live here.
///
/// §6.1 gateway ingest-audit: when `ingest_audit` is on, a browser render
/// submission (`Coverage`/`PieceUpload`) is *sampled* (reputation-graduated,
/// unpredictable — reusing [`Engine::is_assigned`] with this node as auditor)
/// and, if sampled, re-rendered ([`Engine::verify_tile_hash`]) to check the
/// claimed hash BEFORE the node injects + vouches. A mismatch is rejected (not
/// applied, not gossiped). When off, the submission is optimistically forwarded
/// and the swarm peer-audits downstream. Either way the node re-signs nothing
/// extra — it re-publishes the submitter's own signed bytes, but by gossiping
/// them it stakes its node on them (§6.1 vouch).
#[allow(clippy::too_many_arguments)]
fn inject_envelope(
    swarm: &mut Swarm<SheepBehaviour>,
    engine: Option<&mut Engine>,
    pending: &mut Vec<(Envelope, u64)>,
    audited_seen: &mut HashSet<(String, u32, u32, u32)>,
    pending_pieces: Option<&mut Vec<PieceUpload>>,
    ingest_audit: bool,
    env: &Envelope,
    now: u64,
) -> InjectResult {
    // Signature is already verified by the HTTP handler, but re-check defensively
    // (this is the trust boundary; `engine.apply` also re-checks).
    if !env.verify() {
        return InjectResult {
            accepted: false,
            reason: Some("bad signature".into()),
            credits: 0,
            confirmed_tiles: 0,
            sheep_id: None,
            coverage: 0,
            backing: 0,
        };
    }

    // §6.1 gateway ingest-audit — verify-before-vouch for the disposable browser
    // identity. Scoped to render submissions (Coverage / PieceUpload); births,
    // votes, claims, attestations carry their own per-key seq / credit gates in
    // the engine and are not re-rendered here.
    if ingest_audit {
        if let Some(reason) = ingest_audit_reject(engine.as_deref(), env) {
            return InjectResult {
                accepted: false,
                reason: Some(reason),
                credits: engine.as_deref().map(|e| e.credits_of(&env.from)).unwrap_or(0),
                confirmed_tiles: engine
                    .as_deref()
                    .map(|e| e.earned_tiles_for(&env.from))
                    .unwrap_or(0),
                sheep_id: envelope_sheep_id(env),
                coverage: 0,
                backing: 0,
            };
        }
    }

    // §5 heavy artifact path: a PieceUpload is buffered for the accumulator
    // exactly like an observed gossip PIECE. It is also gossiped (the node
    // bridges the browser's piece into the swarm — vouch, §6.1).
    if env.t == proto::PIECE {
        if let Some(buf) = pending_pieces {
            if let Ok(p) = serde_json::from_value::<PieceUpload>(env.body.clone()) {
                buf.push(p);
            }
        }
        publish_env(swarm, env); // bridge/propagate to the swarm
        return InjectResult {
            accepted: true,
            reason: None,
            credits: engine.as_deref().map(|e| e.credits_of(&env.from)).unwrap_or(0),
            confirmed_tiles: engine
                .as_deref()
                .map(|e| e.earned_tiles_for(&env.from))
                .unwrap_or(0),
            sheep_id: envelope_sheep_id(env),
            coverage: engine
                .as_deref()
                .zip(envelope_sheep_id(env))
                .map(|(e, s)| e.coverage(&s))
                .unwrap_or(0),
            backing: engine
                .as_deref()
                .zip(envelope_sheep_id(env))
                .map(|(e, s)| e.backing(&s))
                .unwrap_or(0),
        };
    }

    // Apply via the SAME inbound path (apply + §6 audit-route), or buffer it for
    // the tick-return drain if a render owns the engine. Either way we vouch by
    // re-publishing on the topic for env.t.
    let accepted;
    let (credits, confirmed_tiles, sheep, coverage, backing);
    match engine {
        Some(eng) => {
            accepted = eng.apply(env, now);
            // Route §6 audit for an observed Coverage exactly as gossip does.
            apply_inbound_audit_only(eng, env, audited_seen);
            let s = envelope_sheep_id(env);
            credits = eng.credits_of(&env.from);
            confirmed_tiles = eng.earned_tiles_for(&env.from);
            coverage = s.as_deref().map(|x| eng.coverage(x)).unwrap_or(0);
            backing = s.as_deref().map(|x| eng.backing(x)).unwrap_or(0);
            sheep = s;
        }
        None => {
            // Engine mid-render: buffer the apply for the drain; report pre-apply
            // standing (monotonic). It is still vouched/gossiped now.
            pending.push((env.clone(), now));
            accepted = true;
            credits = 0;
            confirmed_tiles = 0;
            sheep = envelope_sheep_id(env);
            coverage = 0;
            backing = 0;
        }
    }

    publish_env(swarm, env);

    InjectResult {
        accepted,
        reason: if accepted { None } else { Some("rejected by engine.apply (bad seq / overspend / unknown sheep / dup)".into()) },
        credits,
        confirmed_tiles,
        sheep_id: sheep,
        coverage,
        backing,
    }
}

/// §6 audit-routing for an injected Coverage, factored from [`apply_inbound`] so
/// the inject path enqueues assigned audits identically to the gossip path
/// (without re-running `engine.apply`, which the caller already did).
fn apply_inbound_audit_only(
    engine: &mut Engine,
    env: &Envelope,
    audited_seen: &mut HashSet<(String, u32, u32, u32)>,
) {
    if env.t == proto::PROGRESS && env.from != engine.self_pub() {
        if let Ok(cov) = serde_json::from_value::<Coverage>(env.body.clone()) {
            let in_range =
                cov.frame < crate::spec::N_FRAMES && cov.idx < crate::spec::IDXS_PER_FRAME;
            let key = (cov.sheep_id.clone(), cov.frame, cov.idx, cov.pass);
            let tile = (cov.sheep_id.as_str(), cov.frame, cov.idx, cov.pass);
            let assigned = in_range && engine.is_assigned(engine.self_pub(), tile, &env.from);
            if assigned && audited_seen.insert(key) {
                engine.enqueue_audit(Coverage {
                    sheep_id: cov.sheep_id,
                    frame: cov.frame,
                    idx: cov.idx,
                    pass: cov.pass,
                    hash: String::new(),
                });
            }
        }
    }
}

/// §6.1 ingest-audit gate: for a render submission (Coverage / PieceUpload),
/// decide whether to reject it before vouching. Returns `Some(reason)` to reject,
/// `None` to accept. Reputation-graduated *sampling* (reusing the engine's
/// unpredictable `is_assigned` with this node as the auditor): if sampled, the
/// node re-renders the tile and rejects on a hash mismatch; if not sampled, it
/// accepts (the swarm peer-audits downstream). A submission for an unknown sheep
/// is left to `engine.apply` to reject (it can't be re-rendered without a genome).
fn ingest_audit_reject(engine: Option<&Engine>, env: &Envelope) -> Option<String> {
    let eng = engine?;
    let (sheep, frame, idx, pass, hash) = match env.t.as_str() {
        t if t == proto::PROGRESS => {
            let cov = serde_json::from_value::<Coverage>(env.body.clone()).ok()?;
            (cov.sheep_id, cov.frame, cov.idx, cov.pass, cov.hash)
        }
        t if t == proto::PIECE => {
            let p = serde_json::from_value::<PieceUpload>(env.body.clone()).ok()?;
            (p.sheep_id, p.frame, p.idx, p.pass, p.hash)
        }
        // Not a render submission — ingest-audit doesn't apply (engine gates it).
        _ => return None,
    };
    if hash.is_empty() {
        return None; // nothing to re-render against; engine handles it.
    }
    // Sampled? Reuse the reputation-graduated, unpredictable assignment with THIS
    // node as the auditor and the submitter's log-derived rep graduating the rate.
    let sampled = eng.is_assigned(eng.self_pub(), (sheep.as_str(), frame, idx, pass), &env.from);
    if !sampled {
        return None; // not in this round's sample — forward optimistically.
    }
    match eng.verify_tile_hash(&sheep, frame, idx, pass, &hash) {
        Some(true) => None,                        // re-render matches — vouch.
        Some(false) => Some("ingest-audit: re-rendered hash mismatch".into()),
        None => None, // unknown sheep (no genome) — leave to engine.apply.
    }
}

/// The sheep id a write envelope concerns, if any (for the standing read-back).
fn envelope_sheep_id(env: &Envelope) -> Option<String> {
    env.body.get("sheep_id").and_then(|v| v.as_str()).map(|s| s.to_string())
}
