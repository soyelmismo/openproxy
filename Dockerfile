# syntax=docker/dockerfile:1.7
#
# Multi-stage Dockerfile for openproxy.
#
# Stage 1 (builder):
#   - Rust 1.85 on Debian Bookworm.
#   - Node.js 22 + pnpm for the dashboard frontend (TypeScript + esbuild).
#   - Builds the frontend bundle (crates/openproxy-web/src/static/dist/).
#   - Compiles the openproxy release binary.
#
# Stage 2 (runtime):
#   - Debian Bookworm slim + ca-certificates + tini.
#   - Non-root `openproxy` user, data dir at /var/lib/openproxy.
#   - tini as PID 1 for proper signal handling.
#
# The image is built for linux/amd64 and linux/arm64 via `docker buildx`
# (see .github/workflows/ci.yml `docker` job). armv7 is intentionally
# NOT built for Docker (poor upstream support) — only the binary zip
# covers armv7-unknown-linux-gnueabihf.
#
# Usage:
#   docker build -t openproxy .
#   docker run -p 8787:8787 -v $(pwd)/config.toml:/etc/openproxy/config.toml:ro openproxy

# ------------------------------------------------------------------------------
# Stage 1 — builder
# ------------------------------------------------------------------------------
FROM rust:1.85-bookworm AS builder

# Install Node.js 22 from NodeSource and enable pnpm via corepack.
# `ca-certificates` and `curl` are needed to fetch the NodeSource setup
# script; `pkg-config` and `libssl-dev` are belt-and-suspenders for any
# crate that probes OpenSSL at build time (rusqlite uses `bundled`, so
# it does NOT need a system libsqlite3).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        gnupg \
        pkg-config \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && corepack enable \
    && corepack prepare pnpm@9 --activate \
    && rm -rf /var/lib/apt/lists/*

# Verify toolchain versions for build debugging.
RUN rustc --version && cargo --version && node --version && pnpm --version

WORKDIR /build

# Copy the entire workspace. We don't try to cache Cargo.toml separately
# because the workspace has path dependencies that invalidate layer
# caches whenever any crate's manifest changes — a single COPY is
# simpler and lets BuildKit's layer cache do its job.
COPY . .

# Build the dashboard frontend. `pnpm install --frozen-lockfile` requires
# the pnpm-lock.yaml to match package.json exactly. The build emits
# `crates/openproxy-web/src/static/dist/app.js` and friends.
WORKDIR /build/crates/openproxy-web
RUN pnpm install --frozen-lockfile \
    && pnpm typecheck \
    && pnpm typecheck:tests \
    && pnpm build \
    && test -f src/static/dist/app.js

# Build the openproxy release binary. `openproxy-server` produces the
# `openproxy` binary. The frontend dist/ is left in place — the
# `openproxy-web` crate reads static files from CARGO_MANIFEST_DIR at
# runtime (see crates/openproxy-web/src/handlers.rs::serve_static), so
# the build needs the dist/ tree to exist on disk for the dashboard to
# work in case the operator also runs openproxy-web from this image.
WORKDIR /build
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p openproxy-server \
    && cp target/release/openproxy /usr/local/bin/openproxy

# ------------------------------------------------------------------------------
# Stage 2 — runtime
# ------------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# `ca-certificates` is required for any HTTPS call to upstream LLM
# providers (rustls verifies against the system root store on Linux).
# `tini` is a tiny PID 1 that reaps zombies and forwards signals so
# `openproxy` shuts down cleanly on `docker stop`. `wget` is for the
# HEALTHCHECK against /v1/health.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        tini \
        wget \
    && rm -rf /var/lib/apt/lists/*

# Non-root user. `-r` creates a system user (no home dir, low UID).
# `/var/lib/openproxy` is the default data dir; the SQLite DB lives there.
RUN useradd -r -s /bin/false openproxy \
    && mkdir -p /var/lib/openproxy /etc/openproxy \
    && chown -R openproxy:openproxy /var/lib/openproxy /etc/openproxy

# Copy the binary and the example config. Operators mount their own
# config at /etc/openproxy/config.toml (see docker-compose.yml).
COPY --from=builder /usr/local/bin/openproxy /usr/local/bin/openproxy
COPY config.example.toml /etc/openproxy/config.example.toml

USER openproxy
WORKDIR /var/lib/openproxy

# openproxy binds 127.0.0.1:8787 by default (see config.example.toml).
# When running in Docker the operator overrides `server.bind` to
# `0.0.0.0:8787` so the port is reachable outside the container.
EXPOSE 8787

# Persistent state: the SQLite database, encryption key file (if used),
# and any future on-disk cache. Operators mount a named volume here.
VOLUME ["/var/lib/openproxy"]

# Liveness probe against the unauthenticated /v1/health endpoint.
# `--spider` makes wget not download the body; we only care about the
# HTTP status. `--timeout=3 --tries=1` keeps the check snappy.
# Endpoint: crates/openproxy-server/src/router.rs::health.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD wget --quiet --spider --timeout=3 --tries=1 http://127.0.0.1:8787/v1/health || exit 1

# tini as PID 1 → openproxy as the foreground process. The default
# config path matches docker-compose.yml's volume mount.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["openproxy", "--config", "/etc/openproxy/config.toml"]
