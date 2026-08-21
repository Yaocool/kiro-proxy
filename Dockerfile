# syntax=docker/dockerfile:1.7
ARG RUST_VERSION=1.97.1
FROM rust:${RUST_VERSION}-bookworm AS builder-base
WORKDIR /src
ARG CARGO_BUILD_JOBS=1
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

FROM builder-base AS builder-slim
RUN --mount=type=cache,id=kproxy-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=kproxy-cargo-target,target=/src/target,sharing=locked \
    cargo build --release --locked -p kproxy -p kproxyd --no-default-features \
    && install -D -m 0755 /src/target/release/kproxy /out/kproxy \
    && install -D -m 0755 /src/target/release/kproxyd /out/kproxyd

FROM builder-base AS builder-full
RUN --mount=type=cache,id=kproxy-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=kproxy-cargo-target,target=/src/target,sharing=locked \
    cargo build --release --locked -p kproxy -p kproxyd --all-features \
    && install -D -m 0755 /src/target/release/kproxy /out/kproxy \
    && install -D -m 0755 /src/target/release/kproxyd /out/kproxyd \
    && touch /out/build-ready

FROM debian:bookworm-slim AS runtime-base
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates ncurses-term socat tini vim \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 kproxy \
    && useradd --system --uid 10001 --gid kproxy --home /var/lib/kproxy kproxy \
    && install -d -o kproxy -g kproxy -m 0700 /var/lib/kproxy
COPY deploy/entrypoint.sh /usr/local/bin/kproxy-entrypoint
ENV KPROXY_HOME=/var/lib/kproxy
EXPOSE 5580
USER kproxy:kproxy
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/kproxy-entrypoint"]
HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD ["kproxy", "--socket", "/var/lib/kproxy/admin.sock", "health"]

FROM runtime-base AS runtime-slim
COPY --from=builder-slim /out/kproxy /usr/local/bin/kproxy
COPY --from=builder-slim /out/kproxyd /usr/local/bin/kproxyd

FROM runtime-base AS runtime-full
USER root
# This constant marker serializes the memory-heavy Rust build and Chromium
# installation. Its stable contents preserve the Chromium package layer cache
# when only application source code changes.
COPY --from=builder-full /out/build-ready /tmp/kproxy-build-ready
ENV KPROXY_CHROMIUM_REVISION=1566079
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl unzip \
        fonts-liberation \
        libasound2 libatk-bridge2.0-0 libatk1.0-0 libatspi2.0-0 \
        libcairo2 libcups2 libcurl4 libdbus-1-3 libexpat1 libgbm1 \
        libglib2.0-0 libgtk-3-0 libnspr4 libnss3 libpango-1.0-0 \
        libudev1 libvulkan1 libx11-6 libxcb1 libxcomposite1 \
        libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 \
        wget xdg-utils \
    && curl --fail --location --retry 3 \
        --output /tmp/chromium-linux.zip \
        "https://storage.googleapis.com/chromium-browser-snapshots/Linux_x64/${KPROXY_CHROMIUM_REVISION}/chrome-linux.zip" \
    && echo "eafeca49c07daab07d3b8749872811f889e10d44186ebd75771b2fde4e09bca6  /tmp/chromium-linux.zip" \
        | sha256sum --check --strict \
    && unzip -q /tmp/chromium-linux.zip -d /opt \
    && mv /opt/chrome-linux /opt/chromium \
    && find /opt/chromium -type f -name '*.pak.info' -delete \
    && find /opt/chromium/locales -type f ! -name 'en-US.pak' -delete \
    && ln -s /opt/chromium/chrome /usr/local/bin/chromium \
    && chromium --version \
    && rm -rf \
        /var/lib/apt/lists/* \
        /tmp/chromium-linux.zip \
        /tmp/kproxy-build-ready
COPY --from=builder-full /out/kproxy /usr/local/bin/kproxy
COPY --from=builder-full /out/kproxyd /usr/local/bin/kproxyd
ENV KPROXY_CHROMIUM_NO_SANDBOX=1
USER kproxy:kproxy

FROM runtime-full AS final
