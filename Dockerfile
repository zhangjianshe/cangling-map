# syntax=docker/dockerfile:1.7

ARG GDAL_BASE_IMAGE=ghcr.io/osgeo/gdal:ubuntu-small-3.13.0

FROM ${GDAL_BASE_IMAGE} AS builder
ARG RUST_VERSION=1.88.0
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential ca-certificates clang curl libclang-dev pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}"
ENV PATH=/root/.cargo/bin:${PATH}
WORKDIR /workspace
COPY . /workspace/cangling-map
WORKDIR /workspace/cangling-map
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/workspace/cangling-map/target \
    cargo build --release --locked --bin map-server \
    && cp target/release/map-server /usr/local/bin/cangling-map

FROM ${GDAL_BASE_IMAGE}
RUN groupadd --system --gid 10001 cangling \
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
    STYLE_ASSET_ROOT=/data/style-assets
VOLUME ["/data", "/cache"]
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/cangling-map"]
