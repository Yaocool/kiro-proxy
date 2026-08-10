# syntax=docker/dockerfile:1.7
ARG RUST_VERSION=1.97.1
FROM rust:${RUST_VERSION}-bookworm AS builder-slim
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --release --locked --workspace

FROM builder-slim AS builder-full
RUN cargo build --release --locked -p kamd --features sso

FROM debian:bookworm-slim AS runtime-base
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates socat tini \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 kam \
    && useradd --system --uid 10001 --gid kam --home /var/lib/kam kam \
    && install -d -o kam -g kam -m 0700 /var/lib/kam
COPY deploy/entrypoint.sh /usr/local/bin/kam-entrypoint
COPY --from=builder-slim /src/target/release/kam /usr/local/bin/kam
ENV KAM_HOME=/var/lib/kam
EXPOSE 5580
USER kam:kam
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/kam-entrypoint"]
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD ["kam", "--socket", "/var/lib/kam/admin.sock", "health"]

FROM runtime-base AS runtime-slim
COPY --from=builder-slim /src/target/release/kamd /usr/local/bin/kamd

FROM runtime-base AS runtime-full
USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends chromium fonts-liberation \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder-full /src/target/release/kamd /usr/local/bin/kamd
ENV KAM_CHROMIUM_NO_SANDBOX=1
USER kam:kam

FROM runtime-slim AS final
