# ── Stage 1: Build Rust binary ────────────────────────────────────────────────
FROM rust:1.93-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

RUN cargo build --release --bin openxet-server

# ── Stage 2: Build frontend ──────────────────────────────────────────────────
FROM oven/bun:1 AS frontend

WORKDIR /app
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile

COPY web/ .
RUN bun run build

# ── Stage 3: Runtime ─────────────────────────────────────────────────────────
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/openxet-server /app/openxet-server
COPY --from=frontend /app/dist /app/web/dist

ENV OPENXET_HOST=0.0.0.0
ENV OPENXET_PORT=8080
ENV OPENXET_DATA_DIR=/data
ENV OPENXET_FRONTEND_DIR=/app/web/dist

EXPOSE 8080
VOLUME /data

ENTRYPOINT ["/app/openxet-server"]
