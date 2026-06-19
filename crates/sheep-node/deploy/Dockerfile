# Multi-stage build for the proof-of-sheep v3 `sheep-node` (ARCHITECTURE v3 §1,
# §12-step-9). A "seed" is this same binary in *server mode* on a VPS: an
# always-on libp2p bootstrap + relay that also runs the HTTP read/write face.
#
# Stage 1 compiles the workspace member `sheep-node` (which path-depends on
# `sheep-proto` + `flame-core`) in release mode. Stage 2 is a slim runtime with
# `ffmpeg` (for the video encode the accumulate/serve path produces) and the CA
# certs the binary needs (TLS to peers / Let's Encrypt is terminated by Caddy,
# but ca-certificates is still wanted for any outbound TLS + libp2p).
#
# Nothing sensitive is baked in: identity keys are generated at runtime and
# persisted to the /data volume; bind addrs + bootstrap come from env/CMD.

# ---- builder ---------------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /build

# Copy the whole workspace. `sheep-node` path-depends on crates/sheep-proto +
# crates/flame-core, and the workspace Cargo.toml lists every member, so the
# lockfile + the full `crates` tree are needed for a reproducible release build.
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
COPY coordinator ./coordinator

RUN cargo build --release -p sheep-node

# ---- runtime ---------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /build/target/release/sheep-node /usr/local/bin/sheep-node

# Genome seeds. The genesis sheep is *derived* deterministically in-binary
# (crates/sheep-node/src/derive_minted_genesis.rs), so the binary does NOT
# strictly require these files to boot. They're copied so the server-mode HTTP
# face can serve the same seed genomes the v2 coordinator did and so a future
# `--genomes-dir` flag has them on disk. Harmless if unused.
COPY web/genomes /app/web/genomes

# Regenerable caches (accumulated histograms + videos) AND the persisted node
# identity key live under /data; mount a volume there in compose for
# persistence. The persisted key is what gives the seed a STABLE peer id across
# restarts — that stability is what makes the bootstrap multiaddr durable
# (see DEPLOY.md "Bootstrap list").
ENV DATA_DIR=/data
ENV GENOMES_DIR=/app/web/genomes

# Server-mode bind addresses (overridable in compose / env):
#   SHEEP_LISTEN    libp2p listen multiaddr (TCP swarm)
#   SHEEP_HTTP_ADDR HTTP read/write face bind (the browser/watch face)
# NOTE: as of this writing the binary reads --listen/--bootstrap from argv and
# does NOT yet read these env vars or expose --http-addr / a key-file path. The
# entrypoint CMD below + compose pass them as FLAGS; see DEPLOY.md "TODO
# (pending binary flags)". The ENV defaults document the intended interface.
ENV SHEEP_LISTEN=/ip4/0.0.0.0/tcp/4001
ENV SHEEP_HTTP_ADDR=0.0.0.0:8080
ENV RUST_LOG=sheep_node=info

# libp2p swarm port + HTTP face port.
EXPOSE 4001
EXPOSE 8080
VOLUME ["/data"]

# Server mode = a seed: listen on the libp2p port and run the HTTP face. The
# bootstrap peers + the persisted key path are appended by compose (they vary
# per world / per droplet), so the bare image still runs as a standalone seed.
#
# TODO: once the binary exposes --http-addr + --key <path>, fold them in here.
# Today the binary accepts: --listen <multiaddr> [--bootstrap <multiaddr>]...
# [--key <64-hex>]. The shell form lets compose override SHEEP_LISTEN.
CMD ["sh", "-c", "exec sheep-node --listen \"$SHEEP_LISTEN\""]
