# Manual test-server deploy image. Not built by CI — see deploy/README.md.
# Multi-stage: cockpit's dist is embedded into the binary at compile time via
# include_dir! (src/cockpit.rs), so it must exist before `cargo build`.

FROM node:20-bookworm-slim AS cockpit
WORKDIR /src/cockpit
COPY cockpit/package.json cockpit/package-lock.json ./
RUN npm ci
COPY cockpit/ ./
RUN npm run build

FROM rust:1-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake g++ pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
COPY --from=cockpit /src/cockpit/dist ./cockpit/dist
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/oms /usr/local/bin/oms
# oms.toml is resolved relative to the CWD (src/main.rs) — mounted here at runtime.
WORKDIR /app
ENTRYPOINT ["/usr/local/bin/oms"]
