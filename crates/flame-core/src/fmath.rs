//! Deterministic f64 transcendentals — thin wrappers over the `libm` crate.
//!
//! WHY THIS MODULE EXISTS: byte-identical native/wasm output is the protocol's
//! root of trust. Every claim in the swarm ("I rendered this", "this vote is
//! fraudulent") is checked by re-rendering and comparing hashes, so the chaos
//! game must produce the *same bits* on every target. Plain IEEE-754 ops
//! (`+ - * /`, `sqrt`, `abs`, rounding, clamps) are exactly specified and safe
//! to take from std, but transcendentals (`sin`, `ln`, `pow`, ...) are only
//! accuracy-bounded: the system libm on native and the implementation wasm
//! engines use may disagree by ULPs, which snowballs through the iteration
//! into entirely different histograms. Routing every transcendental through
//! the pure-Rust `libm` crate on **all** targets pins one implementation
//! everywhere.
//!
//! Rules (binding on all of `flame-core` forever):
//! - All f64 transcendentals go through this module. No `.sin()`, `.ln()`,
//!   `.powf()`, `.atan2()` etc. on floats anywhere else in the crate.
//! - No `f32` anywhere in the math path.
//! - Any change here is a protocol break (the golden test is the alarm).

#[inline]
pub fn sin(x: f64) -> f64 {
    libm::sin(x)
}

#[inline]
pub fn cos(x: f64) -> f64 {
    libm::cos(x)
}

/// Returns `(sin(x), cos(x))`, matching the order of `f64::sin_cos`.
#[inline]
pub fn sincos(x: f64) -> (f64, f64) {
    libm::sincos(x)
}

/// Natural logarithm (std `f64::ln`).
#[inline]
pub fn log(x: f64) -> f64 {
    libm::log(x)
}

#[inline]
pub fn exp(x: f64) -> f64 {
    libm::exp(x)
}

/// `x^y` (std `f64::powf`).
#[inline]
pub fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

#[inline]
pub fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

#[inline]
pub fn tan(x: f64) -> f64 {
    libm::tan(x)
}

#[inline]
pub fn hypot(x: f64, y: f64) -> f64 {
    libm::hypot(x, y)
}

#[inline]
pub fn sinh(x: f64) -> f64 {
    libm::sinh(x)
}

#[inline]
pub fn cosh(x: f64) -> f64 {
    libm::cosh(x)
}

#[cfg(test)]
mod tests {
    /// Enforce the module rules mechanically: no std float transcendentals and
    /// no f32 anywhere in flame-core outside this file. A failure here means
    /// someone reintroduced a nondeterminism hazard.
    #[test]
    fn no_std_transcendentals_outside_fmath() {
        let forbidden = [
            ".sin()", ".cos()", ".sin_cos()", ".tan()", ".ln(", ".log(",
            ".log2(", ".log10(", ".exp()", ".exp2(", ".exp_m1(", ".ln_1p(",
            ".powf(", ".powi(", ".atan2(", ".atan(", ".asin(", ".acos(",
            ".hypot(", ".sinh()", ".cosh()", ".tanh()", "f32",
        ];
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().unwrap() == "fmath.rs" || path.extension() != Some("rs".as_ref()) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (lineno, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or(""); // ignore comments
                for pat in forbidden {
                    assert!(
                        !code.contains(pat),
                        "{}:{}: `{}` — use crate::fmath (determinism; see fmath.rs)",
                        path.display(), lineno + 1, pat,
                    );
                }
            }
        }
    }
}
