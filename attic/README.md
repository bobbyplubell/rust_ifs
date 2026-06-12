# attic

Code kept for reference, not built by the workspace.

## flame-render-wgpu

The wgpu (v29) GPU renderer from the pre-swarm project (`../rust_ifs`),
archived 2026-06-12. Two paths: CPU chaos game + GPU additive blend
(`lib.rs`/`shaders.wgsl`), and the full-GPU compute chaos game
(`compute.rs`/`compute.wgsl` — 3 passes: cs_chaos -> cs_downsample ->
cs_tonemap, per-thread PCG RNG, genome packed flat at 36 floats/transform).

Relevant future use: a **WebGPU preview renderer** for the site (display
only). wgpu targets WebGPU when compiled to wasm, so this is a port, not a
rewrite. It can never produce proofs: the GPU's per-thread RNG makes a
different (equally valid) pixel stream than the deterministic CPU protocol
render — which is exactly why proofs stay on the CPU path.

Caveats from the old scratch notes: written against wgpu 29 API; the
fragment-stage tonemap can't atomicLoad the max-density buffer, so the
browser surface path needs the tonemap kept in compute -> storage texture ->
blit. Predates the libm/fmath sweep and the DE/exposure tonemap changes, so
its output won't match current CPU output bit-for-bit (fine for preview).
