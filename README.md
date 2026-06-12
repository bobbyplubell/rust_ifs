# wasm-sheep — community-bred fractal flames

A from-scratch [fractal flame](https://flam3.com/flame_draves.pdf) (Scott Draves)
renderer in Rust, compiled to WASM, reviving the **Electric Sheep** idea: every
visitor's browser renders flames ("sheep") locally, and the community votes on
which sheep survive and breed. Rendering a sheep is what earns you a vote.

The site is fully static (GitHub Pages-deployable); all mutable state lives in
a browser-to-browser libp2p swarm. See [ARCHITECTURE.md](ARCHITECTURE.md) for
the full design: self-certifying render proofs, fraud proofs, and a
hash-chained history of generations whose fork-choice rule is heaviest
render-work — including a breeding lab where the next generation's children
are previewable before they're born.

## Architecture

One Rust core, multiple targets — so a `(genome, seed)` renders **byte-identical**
natively and in the browser. That determinism is load-bearing: it's what makes
"prove you rendered this sheep" verifiable (the hash of an honest render is
known in advance).

```
crates/
  flame-core/   pure, deterministic, no-I/O: genome, variations, chaos game,
                tone mapping, interpolation. Own splitmix64 PRNG (no `rand`)
                for cross-platform determinism. serde for genome JSON.
  flame-cli/    native `flame` binary: render / animate / dump / from-json.
                Dev tool + server-side rendering/verification.
  flame-wasm/   wasm-bindgen bindings: `render_rgba(genome_json, …)` runs the
                CPU renderer in the browser.
web/            the site: WASM gallery that renders genomes live in-browser,
                click-to-spin. `web/build.sh` builds it.
```

## The `flame` CLI

```bash
cargo build --release

# a random still from a seed
./target/release/flame render --seed 7 --width 800 --height 800 --ss 2 \
    --samples 20000000 --transforms 3 --out flame.png

# an animation loop interpolating two random genomes -> PNG frames
./target/release/flame animate --seed-a 3 --seed-b 7 --frames 30 \
    --width 400 --height 400 --ss 2 --samples 5000000 --out-dir frames
# then: ffmpeg -framerate 15 -i frames/frame_%04d.png loop.gif

# genome <-> JSON
./target/release/flame dump --seed 7 --out g.json
./target/release/flame from-json --in g.json --seed 7 --out flame.png
```

Key render knobs: `--ss` (supersample, anti-aliasing), `--samples` (quality —
more = less speckle), `--transforms`, `--seed`.

## Web gallery (WASM)

```bash
./web/build.sh                       # builds flame-wasm -> wasm into web/pkg/
python3 -m http.server -d web 8000   # serve (must be over http, not file://)
# open http://localhost:8000
```

The page fetches the genomes in `web/genomes/`, renders each live in the
browser with the WASM build of `flame-core`, and spins them on click.

## Genome shape

A `Genome` is `transforms` (each: selection weight, color, pre/post affine,
per-variation weights), an optional `final_transform`, a `Palette` (color
stops), a `Camera` (center/scale/rotate), and tone params (`brightness`,
`gamma`, `vibrancy`, `background`). It serializes to JSON; sheep are
content-addressed by the hash of that JSON.

## History

This grew out of an earlier single-file renderer (three flame-breaking bugs:
in-place affine aliasing, dead/shadowed tone-mapping code, and bad r/theta
math in the variations — all fixed in `flame-core`) and an LLM-judged breeding
experiment (a Python/LiteLLM genetic algorithm, since removed along with the
GPU renderer; see git history). The current direction replaces the LLM judge
with human votes from people who render the sheep.
