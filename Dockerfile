# syntax=docker/dockerfile:1.7
ARG RUST_VERSION=1.97.1
FROM rust:${RUST_VERSION}-bookworm AS builder-slim
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release --locked --workspace

FROM builder-slim AS builder-full
RUN cargo build --release --locked -p kproxyd --features sso

FROM debian:bookworm-slim AS runtime-base
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates socat tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 kproxy \
    && useradd --system --uid 10001 --gid kproxy --home /var/lib/kproxy kproxy \
    && install -d -o kproxy -g kproxy -m 0700 /var/lib/kproxy
COPY deploy/entrypoint.sh /usr/local/bin/kproxy-entrypoint
COPY --from=builder-slim /src/target/release/kproxy /usr/local/bin/kproxy
ENV KPROXY_HOME=/var/lib/kproxy
EXPOSE 5580
USER kproxy:kproxy
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/kproxy-entrypoint"]
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD ["kproxy", "--socket", "/var/lib/kproxy/admin.sock", "health"]

FROM runtime-base AS runtime-slim
COPY --from=builder-slim /src/target/release/kproxyd /usr/local/bin/kproxyd

FROM runtime-base AS runtime-full
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends chromium fonts-liberation \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder-full /src/target/release/kproxyd /usr/local/bin/kproxyd
ENV KPROXY_CHROMIUM_NO_SANDBOX=1
USER kproxy:kproxy

FROM runtime-slim AS final
