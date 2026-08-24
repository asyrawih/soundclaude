# syntax=docker/dockerfile:1

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
FROM rust:1.97-bookworm AS builder

WORKDIR /build

# Build the dependency graph against stub sources first. This layer is only
# invalidated when a manifest or the lockfile changes, so editing application
# code does not trigger a full rebuild of reqwest, tokio, and friends.
COPY Cargo.toml Cargo.lock ./
COPY crates/soundclaude/Cargo.toml crates/soundclaude/Cargo.toml
COPY crates/soundclaude-cli/Cargo.toml crates/soundclaude-cli/Cargo.toml
COPY crates/soundclaude-server/Cargo.toml crates/soundclaude-server/Cargo.toml

RUN mkdir -p crates/soundclaude/src crates/soundclaude/examples \
             crates/soundclaude-cli/src crates/soundclaude-server/src \
 && touch crates/soundclaude/src/lib.rs \
 # Not needed by the plain `cargo build` below, which ignores examples — but the
 # declared [[example]] target does need a file the moment anyone adds
 # --all-targets, so the stub keeps that from becoming a surprise.
 && echo 'fn main() {}' > crates/soundclaude/examples/playlist.rs \
 && echo 'fn main() {}' > crates/soundclaude-cli/src/main.rs \
 && echo 'fn main() {}' > crates/soundclaude-server/src/main.rs \
 && cargo build --release --locked \
 && rm -rf crates/soundclaude/src crates/soundclaude/examples \
           crates/soundclaude-cli/src crates/soundclaude-server/src \
           target/release/scdl target/release/soundclaude-server \
           target/release/deps/soundclaude* target/release/.fingerprint/soundclaude*

COPY crates crates

RUN cargo build --release --locked --bin scdl --bin soundclaude-server \
 && strip target/release/scdl target/release/soundclaude-server

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates is required: every request to soundcloud.com is TLS.
# curl exists only so HEALTHCHECK has something to call.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

RUN useradd --system --create-home --uid 10001 soundclaude \
 && mkdir -p /var/cache/soundclaude /downloads \
 && chown -R soundclaude:soundclaude /var/cache/soundclaude /downloads

COPY --from=builder /build/target/release/soundclaude-server /usr/local/bin/soundclaude-server
COPY --from=builder /build/target/release/scdl /usr/local/bin/scdl

USER soundclaude
WORKDIR /downloads

ENV PORT=8080 \
    SOUNDCLAUDE_CACHE=/var/cache/soundclaude/client_id.json \
    RUST_LOG=soundclaude_server=info,soundclaude=info

EXPOSE 8080

# start-period is generous: the server scrapes a client_id before it serves.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${PORT}/health" || exit 1

CMD ["soundclaude-server"]
