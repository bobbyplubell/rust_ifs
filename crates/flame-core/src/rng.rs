//! A tiny, fast, deterministic PRNG.
//!
//! We deliberately do *not* depend on the `rand` crate: the chaos game must
//! produce byte-identical results on native and on wasm32 from the same seed,
//! so a server can verify (by hash) the exact flame a browser rendered from
//! just `(genome, seed)`, and every client sees the same sheep.
//!
//! `splitmix64` is more than good enough for a chaos game and is trivial to
//! reimplement identically anywhere (including JS, if ever needed).

#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // Avoid a zero state producing a degenerate early stream.
        Rng {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    #[inline]
    pub fn f64(&mut self) -> f64 {
        // 53-bit mantissa worth of randomness.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [lo, hi).
    #[inline]
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f64()
    }

    /// Uniform integer in [0, n).
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        (self.next_u64() % n as u64) as usize
    }

    /// Fair coin with probability `p` of returning true.
    #[inline]
    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }
}
