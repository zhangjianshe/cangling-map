# syntax=docker/dockerfile:1.7

ARG GDAL_BASE_IMAGE=ghcr.io/osgeo/gdal:ubuntu-small-3.13.0@sha256:e0fedbcc5b51bd12617d6a6cf8b7c3244d98cbd29b266925fd9061f40081b351
ARG RUST_IMAGE=rust:1.88.0-bookworm

FROM ${RUST_IMAGE} AS rust-toolchain

# Keep GDAL, PROJ and the system ABI identical in build and runtime stages.
FROM ${GDAL_BASE_IMAGE} AS runtime-base
ENV http_proxy= \
    https_proxy= \
    HTTP_PROXY= \
    HTTPS_PROXY=
RUN unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates fontconfig libvulkan1 \
    && rm -rf /var/lib/apt/lists/*

# Install the font collection outside the final image; copy only the font used
# by the renderer, rather than all CJK weights and serif families.
FROM runtime-base AS fonts
RUN unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    && apt-get update \
    && apt-get install -y --no-install-recommends fonts-noto-cjk \
    && test -r /usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc \
    && rm -rf /var/lib/apt/lists/*

FROM runtime-base AS builder
RUN unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential clang git libclang-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup
ENV PATH=/usr/local/cargo/bin:${PATH} \
    RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    CARGO_NET_GIT_FETCH_WITH_CLI=true \
    CARGO_NET_RETRY=5 \
    CARGO_HTTP_MULTIPLEXING=false
WORKDIR /workspace
COPY . /workspace/cangling-map
WORKDIR /workspace/cangling-map
# Separate native and emulated architecture caches when building both platforms.
ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=cangling-map-ubuntu-small-${TARGETARCH},target=/workspace/cangling-map/target \
    unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    && cargo build --release --locked --bin map-server \
    && cp target/release/map-server /usr/local/bin/cangling-map

FROM runtime-base
COPY --from=fonts /usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc /usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc
COPY --from=fonts /usr/share/doc/fonts-noto-cjk/copyright /usr/share/doc/fonts-noto-cjk/copyright
RUN fc-cache -f \
    && groupadd --system --gid 10001 cangling \
    && useradd --system --uid 10001 --gid cangling --home-dir /data --create-home cangling \
    && mkdir -p /data/style-assets /cache/wmts /cache/vector-projection \
    && chown -R cangling:cangling /data /cache
COPY --from=builder /usr/local/bin/cangling-map /usr/local/bin/cangling-map
COPY data/placenames.sqlite /usr/share/cangling-map/placenames.sqlite
USER cangling
WORKDIR /data
ENV MAP_SERVER_ADDRESS=0.0.0.0:3000 \
    CATALOG_DATABASE_URL=sqlite:///data/catalog.db?mode=rwc \
    IMAGERY_DATABASE_URL=sqlite:///data/imagery.db?mode=rwc \
    PLACENAMES_DATABASE_URL=sqlite:///usr/share/cangling-map/placenames.sqlite?mode=ro \
    WMTS_CACHE_BACKEND=filesystem \
    MAP_CACHE_ROOT=/cache \
    MAP_FONT_PATH=/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc \
    STYLE_ASSET_ROOT=/data/style-assets
VOLUME ["/data", "/cache"]
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/cangling-map"]
