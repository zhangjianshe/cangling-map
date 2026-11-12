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

# Rebuild only the missing drivers from the exact GDAL release. Their build
# dependencies and temporary GDAL core stay outside the runtime image.
FROM runtime-base AS gdal-drivers
RUN unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY \
    && apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        g++ make cmake curl pkg-config libproj-dev libhdf5-dev libnetcdf-dev \
    && rm -rf /var/lib/apt/lists/*
RUN unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY \
    && test "$(gdal-config --version)" = "3.13.0" \
    && curl --fail --location --retry 3 https://github.com/OSGeo/gdal/releases/download/v3.13.0/gdal-3.13.0.tar.gz -o /tmp/gdal.tar.gz \
    && echo '1eb8c56a8cea4d3c733d90a719540c1aab981e4eb15e03057092e69b2935ae73  /tmp/gdal.tar.gz' | sha256sum -c - \
    && mkdir /tmp/gdal-source \
    && tar -xzf /tmp/gdal.tar.gz -C /tmp/gdal-source --strip-components=1 \
    && cmake -S /tmp/gdal-source -B /tmp/gdal-build \
        -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_SKIP_RPATH=ON \
        -DBUILD_APPS=OFF -DBUILD_TESTING=OFF \
        -DBUILD_PYTHON_BINDINGS=OFF -DBUILD_JAVA_BINDINGS=OFF -DBUILD_CSHARP_BINDINGS=OFF \
        -DGDAL_BUILD_OPTIONAL_DRIVERS=OFF -DOGR_BUILD_OPTIONAL_DRIVERS=OFF \
        -DGDAL_ENABLE_DRIVER_HDF5=ON -DGDAL_ENABLE_DRIVER_HDF5_PLUGIN=ON \
        -DGDAL_ENABLE_DRIVER_NETCDF=ON -DGDAL_ENABLE_DRIVER_NETCDF_PLUGIN=ON \
        -DGDAL_USE_HDF5=ON -DGDAL_USE_NETCDF=ON \
    && cmake --build /tmp/gdal-build --target gdal_HDF5 gdal_netCDF --parallel 4 \
    && mkdir -p /out/gdalplugins \
    && find /tmp/gdal-build -name 'gdal_HDF5.so' -exec cp '{}' /out/gdalplugins/ \; \
    && find /tmp/gdal-build -name 'gdal_netCDF.so' -exec cp '{}' /out/gdalplugins/ \; \
    && test -s /out/gdalplugins/gdal_HDF5.so \
    && test -s /out/gdalplugins/gdal_netCDF.so \
    && strip /out/gdalplugins/*.so \
    && cp /tmp/gdal-source/LICENSE.TXT /out/GDAL-LICENSE.TXT

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
WORKDIR /workspace/cangling-map
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Separate native and emulated architecture caches when building both platforms.
ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=cangling-map-ubuntu-small-${TARGETARCH},target=/workspace/cangling-map/target \
    unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY \
    && cargo build --release --locked --bin map-server \
    && cp target/release/map-server /usr/local/bin/cangling-map

FROM runtime-base
RUN unset http_proxy https_proxy HTTP_PROXY HTTPS_PROXY all_proxy ALL_PROXY \
    && apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends libhdf5-310 libnetcdf22 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=gdal-drivers /out/gdalplugins/ /usr/lib/gdalplugins/
COPY --from=gdal-drivers /out/GDAL-LICENSE.TXT /usr/share/doc/cangling-map/GDAL-plugins-LICENSE.TXT
ENV GDAL_DRIVER_PATH=/usr/lib/gdalplugins
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
