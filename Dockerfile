# syntax=docker/dockerfile:1

# ============================================================================
# mega-downloader — single self-contained image (frontend + backend in one).
#
# Builds the React UI, builds the Rust engine/server, then copies BOTH into a
# slim runtime image. `mega-downloader` serves the compiled UI *and* the
# REST/WebSocket API from the one process/port, so it deploys as-is in Docker
# or Kubernetes with a single Service/Deployment.
#
#   docker build -t mega-downloader .
#   docker run --rm -p 8787:8787 \
#     -v mega-data:/data \
#     -e BIND_ADDR=0.0.0.0:8787 \
#     mega-downloader
#
# Runtime conventions (all overridable via env):
#   BIND_ADDR      host:port to listen on      (default 0.0.0.0:8787 in image)
#   DB_PATH        SQLite database path        (default /data/mega-downloader.db)
#   DOWNLOAD_DIR   where downloaded files go   (default /data/downloads)
#   MEGA_UI_DIR    compiled frontend directory (default /app/ui-dist, baked in)
# ============================================================================

# --- Stage 1: build the React UI ---------------------------------------------
FROM node:22-alpine AS ui-builder
WORKDIR /build/ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
RUN npm run build
# Vite output lands in /build/ui/dist

# --- Stage 2: build the Rust engine/server -----------------------------------
# 1.x pinned to a Debian bookworm base so the resulting binary is glibc-linked
# and runs on the slim bookworm runtime image. SQLite is bundled by
# sqlx/libsqlite3-sys, so no system libsqlite is needed.
FROM rust:1-bookworm AS rust-builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY migrations/ ./migrations/
RUN cargo build --release -p server \
    && cp target/release/mega-downloader /mega-downloader

# --- Stage 3: slim runtime image ---------------------------------------------
# Debian slim is deliberate: the engine's TLS (rustls) needs system CA
# certificates to reach MEGA + Real-Debrid over HTTPS.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=rust-builder /mega-downloader /app/mega-downloader
COPY --from=ui-builder /build/ui/dist /app/ui-dist

# Data lives under /data so a volume can be mounted there.
ENV BIND_ADDR=0.0.0.0:8787 \
    MEGA_UI_DIR=/app/ui-dist \
    DB_PATH=/data/mega-downloader.db \
    DOWNLOAD_DIR=/data/downloads
RUN mkdir -p /data && chmod 777 /data

EXPOSE 8787
VOLUME ["/data"]
ENTRYPOINT ["/app/mega-downloader"]
