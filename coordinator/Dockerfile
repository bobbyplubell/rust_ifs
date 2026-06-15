# Multi-stage build for the proof-of-sheep v2 coordinator.
#
# Stage 1 compiles the workspace member `coordinator` (which path-depends on
# flame-core) in release mode. Stage 2 is a slim runtime with ffmpeg for the
# video encode and the CA certs / sqlite runtime the binary needs.

# ---- builder ---------------------------------------------------------------
FROM rust:1-bookworm AS builder
WORKDIR /build

# Copy the whole workspace (coordinator path-depends on crates/flame-core, and
# the workspace Cargo.toml lists every member).
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
COPY coordinator ./coordinator

# rusqlite uses the `bundled` feature so SQLite is compiled in — no system lib
# needed at build or runtime.
RUN cargo build --release -p coordinator

# ---- runtime ---------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /build/target/release/coordinator /usr/local/bin/coordinator

# Genome seeds (the flock is seeded from here on first boot).
COPY web/genomes /app/web/genomes

# Regenerable caches (histograms + videos) + the SQLite db live here; mount a
# volume at /data in compose for persistence.
ENV DATA_DIR=/data
ENV GENOMES_DIR=/app/web/genomes
ENV BIND=0.0.0.0:8080
EXPOSE 8080
VOLUME ["/data"]

CMD ["coordinator"]
