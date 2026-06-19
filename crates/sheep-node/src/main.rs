//! `sheep-node` — a minimal proof-of-sheep v3 native peer (ARCHITECTURE v3
//! §12-step-2). Generates (or loads) an ed25519 key, builds an [`Engine`], and
//! drives it over a real libp2p TCP swarm via [`sheep_node::run`].
//!
//! Usage:
//! ```text
//! sheep-node [--listen <multiaddr>] [--bootstrap <multiaddr>]... [--key <hex>]
//!            [--http-addr <ip:port>] [--data-dir <path>]
//!            [--ingest-audit on|off]
//! ```
//! - `--listen`    multiaddr to listen on (default `/ip4/0.0.0.0/tcp/0`).
//! - `--bootstrap` peer multiaddr to dial; repeatable.
//! - `--key`       64-char hex of a 32-byte ed25519 secret (literal override).
//! - `--key-file`  path to a persisted identity key (64-char hex). Loaded if it
//!                 exists, else generated and written there — so a seed's peer id
//!                 is STABLE across restarts (durable bootstrap multiaddrs).
//!                 `--key` (literal) takes precedence if both are given.
//! - `--http-addr` `ip:port` to serve the read-only watch face on. **Presence of
//!                 this flag makes the node a "server/accumulator"** (§1.1): it
//!                 ingests pieces into an accumulator + serves flock/sheep/video/
//!                 hall over HTTP. Without it, the node is a plain peer/worker.
//! - `--data-dir`  on-disk dir for the regenerable video cache (default
//!                 `./sheep-data`; only used when `--http-addr` is set).
//! - `--ingest-audit on|off` (§6.1) the gateway ingest-audit policy for the
//!                 browser/REST write face. `on` (default, public-deployment
//!                 posture) sample-audits browser render submissions
//!                 (verify-before-vouch) before the node injects + gossips them;
//!                 `off` (personal-node) optimistically forwards. Only meaningful
//!                 with `--http-addr` (the write face).

use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use libp2p::Multiaddr;
use sheep_node::{run, DecayParams, HallThreshold, ServeConfig, WorldConfig};

/// Read an env var as `T`, falling back to `default` when unset or unparseable.
/// Used to build the per-world [`WorldConfig`] from the deploy `*.env` knobs.
fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<T>().ok())
        .unwrap_or(default)
}

/// Build the §2.2/§3 per-world [`WorldConfig`] from the `SHEEP_*` env knobs the
/// deploy `sandbox.env` / `gallery.env` files set (matching their exact NAMES).
/// Any unset/garbled knob falls back to the engine's own defaults, so a bare
/// `sheep-node` with no env behaves exactly as before.
fn world_config_from_env() -> WorldConfig {
    let d = DecayParams::DEFAULT;
    let h = HallThreshold::DEFAULT;
    let base = WorldConfig::DEFAULT;
    WorldConfig {
        decay: DecayParams {
            time_unit_ms: env_or("SHEEP_DECAY_TIME_UNIT_MS", d.time_unit_ms),
            base: env_or("SHEEP_DECAY_BASE", d.base),
            linear: env_or("SHEEP_DECAY_LINEAR", d.linear),
            quad: env_or("SHEEP_DECAY_QUAD", d.quad),
            exp_scale: env_or("SHEEP_DECAY_EXP_SCALE", d.exp_scale),
            half_life: env_or("SHEEP_DECAY_HALF_LIFE", d.half_life),
        },
        hall: HallThreshold {
            min_lifespan_ms: env_or("SHEEP_HALL_MIN_LIFESPAN_MS", h.min_lifespan_ms),
            min_peak_backing: env_or("SHEEP_HALL_MIN_PEAK_BACKING", h.min_peak_backing),
        },
        vote_cost: env_or("SHEEP_VOTE_COST", base.vote_cost),
        mint_cost: env_or("SHEEP_MINT_COST", base.mint_cost),
        breed_cost: env_or("SHEEP_BREED_COST", base.breed_cost),
        // New deploy knob (added to the *.env files): how many LIVE starter
        // sheep a seed mints at boot. Default 4 (a watchable starter flock).
        bootstrap_flock: env_or("SHEEP_BOOTSTRAP_FLOCK", base.bootstrap_flock),
        // §5 accumulator RAM budget (MB) for the resident merged-frame working
        // set; the rest spills to `data_dir/accum/`. Default 128 (bounds RAM
        // independent of flock size). Only the serving/accumulator node uses it.
        accum_ram_mb: env_or("SHEEP_ACCUM_RAM_MB", base.accum_ram_mb),
    }
}

fn parse_key_hex(s: &str) -> Option<SigningKey> {
    if s.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for i in 0..32 {
        bytes[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(SigningKey::from_bytes(&bytes))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut listen: Multiaddr = "/ip4/0.0.0.0/tcp/0".parse().unwrap();
    let mut bootstrap: Vec<Multiaddr> = Vec::new();
    let mut signing_key: Option<SigningKey> = None;
    let mut key_file: Option<PathBuf> = None;
    let mut http_addr: Option<SocketAddr> = None;
    let mut data_dir: PathBuf = PathBuf::from("./sheep-data");
    // §6.1 gateway ingest-audit: ON by default (public-deployment posture).
    let mut ingest_audit: bool = true;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                if let Some(v) = args.next() {
                    listen = v.parse().map_err(|e| format!("bad --listen: {e}"))?;
                }
            }
            "--bootstrap" => {
                if let Some(v) = args.next() {
                    bootstrap.push(v.parse().map_err(|e| format!("bad --bootstrap: {e}"))?);
                }
            }
            "--key" => {
                if let Some(v) = args.next() {
                    signing_key =
                        Some(parse_key_hex(&v).ok_or("bad --key: want 64-char hex")?);
                }
            }
            "--key-file" => {
                if let Some(v) = args.next() {
                    key_file = Some(PathBuf::from(v));
                }
            }
            "--http-addr" => {
                if let Some(v) = args.next() {
                    http_addr = Some(v.parse().map_err(|e| format!("bad --http-addr: {e}"))?);
                }
            }
            "--data-dir" => {
                if let Some(v) = args.next() {
                    data_dir = PathBuf::from(v);
                }
            }
            "--ingest-audit" => {
                if let Some(v) = args.next() {
                    ingest_audit = match v.as_str() {
                        "on" | "true" | "1" => true,
                        "off" | "false" | "0" => false,
                        other => return Err(format!("bad --ingest-audit: {other} (want on|off)").into()),
                    };
                }
            }
            other => return Err(format!("unknown arg: {other}").into()),
        }
    }

    // §1.1 capability gate: `--http-addr` makes this a server/accumulator.
    // §6.1: the write face applies ingest-audit per `--ingest-audit` (default on).
    let serve = http_addr.map(|http_addr| ServeConfig {
        http_addr,
        data_dir,
        ingest_audit,
    });

    // Key resolution: `--key` literal wins; else `--key-file` (load-or-create →
    // stable peer id across restarts, for durable bootstrap multiaddrs); else an
    // ephemeral random key.
    let signing_key = match (signing_key, key_file) {
        (Some(k), _) => k,
        (None, Some(path)) => load_or_create_key(&path)?,
        (None, None) => {
            let mut seed = [0u8; 32];
            getrandom_seed(&mut seed);
            SigningKey::from_bytes(&seed)
        }
    };

    // §2.2/§3 per-world personality (decay/hall/costs) + the §deploy bootstrap-
    // flock size, read from the deploy `*.env` knobs (defaults when unset).
    let world = world_config_from_env();

    eprintln!(
        "sheep-node: pub={} listen={listen} bootstrap={bootstrap:?} role={} \
         decay(unit={}ms quad={} half_life={}) costs(v={} m={} b={}) bootstrap_flock={}",
        hex_lower(&signing_key.verifying_key().to_bytes()),
        if serve.is_some() { "server/accumulator" } else { "worker" },
        world.decay.time_unit_ms,
        world.decay.quad,
        world.decay.half_life,
        world.vote_cost,
        world.mint_cost,
        world.breed_cost,
        world.bootstrap_flock,
    );

    run(signing_key, listen, bootstrap, serve, world).await
}

/// Fill `buf` with OS randomness. Uses `SystemTime` + address entropy as a
/// dependency-free seed (ephemeral keys only; step-2 triviality per the brief).
fn getrandom_seed(buf: &mut [u8; 32]) {
    // Prefer real OS entropy — a persisted `--key-file` identity is long-lived.
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, buf))
        .is_ok()
    {
        return;
    }
    // Fallback (no /dev/urandom): time + address entropy. Fine for ephemeral keys.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stack_addr = buf.as_ptr() as usize as u128;
    let mut x = nanos ^ (stack_addr << 64) ^ 0x9e3779b97f4a7c15;
    for b in buf.iter_mut() {
        // SplitMix64-ish byte stream — fine for a throwaway demo key.
        x = x.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(0x9e37_79b9);
        *b = (x >> 33) as u8;
    }
}

/// Load a persisted ed25519 identity key (64-char hex) from `path`, or generate
/// one and write it there (0600 on unix) so the peer id is STABLE across
/// restarts — required for durable bootstrap multiaddrs (§12 deploy).
fn load_or_create_key(path: &std::path::Path) -> Result<SigningKey, Box<dyn Error + Send + Sync>> {
    if path.exists() {
        let s = std::fs::read_to_string(path)?;
        return match parse_key_hex(s.trim()) {
            Some(k) => Ok(k),
            None => Err(format!("bad key file {}: want 64-char hex", path.display()).into()),
        };
    }
    let mut seed = [0u8; 32];
    getrandom_seed(&mut seed);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, hex_lower(&seed))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(SigningKey::from_bytes(&seed))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}
