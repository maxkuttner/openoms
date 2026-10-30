# The published image: ghcr.io/maxkuttner/openoms, built by .github/workflows/release.yml
# for linux/amd64 and linux/arm64. Run it with compose: see
# https://maxkuttner.github.io/openoms/getting-started/installation/
#
# Multi-stage like deploy/Dockerfile: the cockpit's dist is embedded into the binary
# at compile time (src/cockpit.rs), so it must exist before `cargo build`.

FROM node:22-bookworm-slim AS cockpit
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
RUN cargo build --release && strip target/release/oms

FROM debian:bookworm-slim
ARG VERSION=dev
LABEL org.opencontainers.image.source="https://github.com/maxkuttner/openoms" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.title="openoms" \
      org.opencontainers.image.description="openoms order management system: server, cockpit and CLI"
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 oms \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin oms \
    && mkdir /config && chown oms:oms /config
COPY --from=builder /src/target/release/oms /usr/local/bin/oms
# OMS_CONTAINER: `oms init` asks only the admin password (compose supplies the
# database) and `oms update` defers to `docker compose pull`.
ENV OMS_CONFIG=/config/oms.toml \
    OMS_BIND_ADDR=0.0.0.0:3001 \
    OMS_CONTAINER=1
# A fresh named volume copies /config's ownership, so uid 10001 can write it.
VOLUME /config
EXPOSE 3001
USER oms
ENTRYPOINT ["oms"]
