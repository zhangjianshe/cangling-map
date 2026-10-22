# syntax=docker/dockerfile:1.7

ARG GDAL_BASE_IMAGE=mapway/gdal-base:v4.2.35
ARG RUST_IMAGE=rust:1.88.0-bookworm

FROM ${RUST_IMAGE} AS rust-toolchain

FROM ${GDAL_BASE_IMAGE} AS builder
ENV http_proxy= \
    https_proxy= \
    HTTP_PROXY= \
    HTTPS_PROXY=
RUN unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates clang git libclang-dev pkg-config \
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
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/workspace/cangling-map/target \
    unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    && cargo build --release --locked --bin map-server \
    && cp target/release/map-server /usr/local/bin/cangling-map

FROM ${GDAL_BASE_IMAGE}
ENV http_proxy= \
    https_proxy= \
    HTTP_PROXY= \
    HTTPS_PROXY=
RUN unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        fontconfig fonts-noto-cjk libvulkan1 \
    && test -r /usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc \
    && fc-cache -f \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 cangling \
    && useradd --system --uid 10001 --gid cangling --home-dir /data --create-home cangling \
    && mkdir -p /data/style-assets /cache/wmts /cache/vector-projection \
    && chown -R cangling:cangling /data /cache
COPY --from=builder /usr/local/bin/cangling-map /usr/local/bin/cangling-map
USER cangling
WORKDIR /data
ENV MAP_SERVER_ADDRESS=0.0.0.0:3000 \
    CATALOG_DATABASE_URL=sqlite:///data/catalog.db?mode=rwc \
    WMTS_CACHE_BACKEND=filesystem \
    MAP_CACHE_ROOT=/cache \
    MAP_FONT_PATH=/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc \
    STYLE_ASSET_ROOT=/data/style-assets
VOLUME ["/data", "/cache"]
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/cangling-map"]
