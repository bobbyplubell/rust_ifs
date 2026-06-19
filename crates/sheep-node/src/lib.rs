//! `sheep-node` — the proof-of-sheep v3 native node (ARCHITECTURE v3 §2–§7,
//! §10, §12-step-2).
//!
//! Two layers, cleanly split:
//!
//! - [`engine`] — a **pure, libp2p-free, clock-injected state machine** (the
//!   brain). It holds the node's keypair, the learned flock, coverage, live
//!   claims, credits, and the per-key sequence / equivocation bookkeeping, and
//!   exposes [`Engine::apply`] (ingest one inbound [`sheep_proto::Envelope`])
//!   and [`Engine::tick`] (the contribute loop; returns signed outbound
//!   envelopes). Built ON [`sheep_proto`] (the wire contract) and [`flame_core`]
//!   (the deterministic renderer).
//! - [`net`] — the **libp2p transport** (just I/O) that wraps the engine: a
//!   gossipsub + identify + request-response swarm whose run loop feeds inbound
//!   gossip to `apply` and publishes `tick`'s envelopes on the topic for each
//!   `env.t`. The engine never learns it is networked.
//!
//! [`derive_minted_genesis`] supplies a deterministic genesis sheep so a pair of
//! nodes share something to render before the mint lifecycle (step 5) lands.

pub mod accumulator;
pub mod block;
pub mod derive_minted_genesis;
pub mod engine;
pub mod hist;
pub mod http;
pub mod net;
pub mod spec;
pub mod video;

pub use accumulator::Accumulator;
pub use block::{block_units, BlockId, Unit};
pub use derive_minted_genesis::{genesis_mint, genesis_minter_pub, genesis_sheep_hex};
pub use engine::{DecayParams, Engine, FlockEntry, HallThreshold, LiveClaim, WorldConfig};
pub use http::{HttpState, ReadState};
pub use net::{
    now_ms, run, run_on_transport, run_on_transport_with, run_tcp_reporting, AssignResult, Control,
    InjectResult, ServeConfig, Snapshot,
};
