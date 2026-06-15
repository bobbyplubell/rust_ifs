//! The GA "personality" — the externalized knobs that distinguish one world
//! from another (e.g. the fast/wild Sandbox vs the slow/refined Gallery; see
//! ARCHITECTURE §5 and `coordinator/deploy/`).
//!
//! These were hardcoded constants in `ga.rs` and `flame_core::breed`; this
//! module lifts them to the environment with the **current values as defaults**,
//! so behavior is byte-identical when the envs are unset. The same coordinator
//! binary runs either world purely by its env.
//!
//! Knob groups:
//! - **mutation** — `GA_MUTATION_RATE` (per-site probability) + `GA_MUTATION_MAGNITUDE`
//!   (how many mutation passes to apply; >1 compounds the jitter for wilder children).
//! - **fresh-blood immigrants** — `GA_IMMIGRANTS` (random genomes injected per gen).
//! - **selection pressure** — `GA_SURVIVORS` (top-voted sheep kept outright) and
//!   `GA_FLOCK_SIZE` (target flock size after a tick); fewer survivors = harsher
//!   selection.

/// Default per-site mutation probability. Matches the old
/// `flame_core::breed::BREED_MUTATION_RATE` that `breed()` used.
pub const MUTATION_RATE_DEFAULT: f64 = 0.15;
/// Default mutation magnitude: a single mutation pass (= the old `breed()`).
pub const MUTATION_MAGNITUDE_DEFAULT: u32 = 1;
/// Default fresh-blood immigrants injected per generation (old `IMMIGRANTS`).
pub const IMMIGRANTS_DEFAULT: u32 = 2;
/// Default count of top-voted sheep that survive a gen outright (old `SURVIVORS`).
pub const SURVIVORS_DEFAULT: u32 = 3;
/// Default target flock size after a tick (old `FLOCK_SIZE`).
pub const FLOCK_SIZE_DEFAULT: u32 = 8;

/// The active GA personality, read once at boot. `Copy` so it threads cheaply
/// through the GA functions.
#[derive(Clone, Copy, Debug)]
pub struct GaConfig {
    /// Per-site mutation probability handed to `flame_core::breed::mutate`.
    pub mutation_rate: f64,
    /// Number of mutation passes applied to each bred child. `1` = the historic
    /// `breed()`; higher values compound the jitter (wilder mutants). Clamped to
    /// `>= 1` so children are always at least lightly mutated.
    pub mutation_magnitude: u32,
    /// Fresh-blood random immigrants injected each generation.
    pub immigrants: u32,
    /// How many top-voted sheep survive a generation outright (selection pressure).
    pub survivors: u32,
    /// Target flock size after a tick (used when seeding a thin/missing genome dir).
    pub flock_size: u32,
}

impl Default for GaConfig {
    fn default() -> Self {
        GaConfig {
            mutation_rate: MUTATION_RATE_DEFAULT,
            mutation_magnitude: MUTATION_MAGNITUDE_DEFAULT,
            immigrants: IMMIGRANTS_DEFAULT,
            survivors: SURVIVORS_DEFAULT,
            flock_size: FLOCK_SIZE_DEFAULT,
        }
    }
}

impl GaConfig {
    /// Read the GA personality from the environment, defaulting each knob to its
    /// historic hardcoded value so an unset env reproduces today's behavior.
    pub fn from_env() -> Self {
        let cfg = GaConfig {
            mutation_rate: env_f64("GA_MUTATION_RATE", MUTATION_RATE_DEFAULT)
                .clamp(0.0, 1.0),
            mutation_magnitude: env_u32("GA_MUTATION_MAGNITUDE", MUTATION_MAGNITUDE_DEFAULT)
                .max(1),
            immigrants: env_u32("GA_IMMIGRANTS", IMMIGRANTS_DEFAULT),
            survivors: env_u32("GA_SURVIVORS", SURVIVORS_DEFAULT).max(1),
            flock_size: env_u32("GA_FLOCK_SIZE", FLOCK_SIZE_DEFAULT).max(1),
        };
        tracing::info!(
            "GA config: mutation_rate={} mutation_magnitude={} immigrants={} survivors={} flock_size={}",
            cfg.mutation_rate,
            cfg.mutation_magnitude,
            cfg.immigrants,
            cfg.survivors,
            cfg.flock_size,
        );
        cfg
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_historic_values() {
        let cfg = GaConfig::default();
        assert_eq!(cfg.mutation_rate, 0.15);
        assert_eq!(cfg.mutation_magnitude, 1);
        assert_eq!(cfg.immigrants, 2);
        assert_eq!(cfg.survivors, 3);
        assert_eq!(cfg.flock_size, 8);
    }
}
