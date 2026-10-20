# cangling-map

A Rust map server workspace designed around an OGC WMS 1.3.0 adapter and a
protocol-independent rendering core.

## Architecture

```text
HTTP/KVP request
      ↓
ogc-wms: parse and validate WMS requests
      ↓
map-core: normalized MapRequest and catalog traits
      ↓
map-gdal + map-style + map-render
      ↓
PNG/JPEG response or OGC service exception
```

## Crates

- `ogc-wms`: WMS request parsing, validation, capabilities, and exceptions.
- `map-core`: protocol-neutral request, layer, CRS, and rendering contracts.
- `map-catalog`: layer catalog contracts and SQLite persistence.
- `map-render`: image composition and symbolizer execution.
- `map-gdal`: GDAL-backed raster/vector sources and reprojection.
- `map-style`: internal style model and future SLD/SE compiler.
- `map-server`: Axum application and process configuration.

## Run

```bash
cargo run -p map-server
curl 'http://127.0.0.1:3000/health'
curl 'http://127.0.0.1:3000/wms?SERVICE=WMS&REQUEST=GetCapabilities&VERSION=1.3.0'
```

Open `http://127.0.0.1:3000/` to manage catalog items. The server stores them in
`catalog.db` by default; set `CATALOG_DATABASE_URL` to use another SQLite file.

## WMTS tile cache

The WMTS service supports `filesystem`, `cangling-tile`, and `none` cache backends.
Local development uses the filesystem backend by default:

```bash
WMTS_CACHE_BACKEND=filesystem
WMTS_CACHE_PATH=./wmts-cache
```

For production, use the centralized `cangling-tile` service:

```bash
WMTS_CACHE_BACKEND=cangling-tile
CANGLING_TILE_URL=http://127.0.0.1:7601
CANGLING_TILE_TOKEN=replace-with-the-cangling-tile-write-token
```

The legacy development variable names are also accepted and take precedence:

```bash
TILE_CACHE_URL=http://127.0.0.1:7601
TILE_CACHE_TOKEN=replace-with-the-cangling-tile-write-token
```

The same remote settings can be changed at runtime from **设置 → 缓存服务**.
When enabled, the persisted Cangling Tile settings override the environment
backend immediately. When disabled or absent, the startup environment remains
the local fallback. Settings are stored in the `system_settings` table of the
catalog database (`catalog.db` by default). The access token is never returned
by the settings read API.

Vector reprojection is cached independently from rendered WMTS tiles. The
defaults favor large administrative datasets and tile-sized requests:

```bash
VECTOR_PROJECTION_CACHE_MB=2048
VECTOR_PROJECTION_CACHE_SIZE=32
VECTOR_PROJECTION_CACHE_PATH=./vector-projection-cache
VECTOR_PROJECTION_SCOPE=local
VECTOR_PROJECTION_METATILE_FACTOR=4
```

Projected FlatGeobuf datasets are stored below `VECTOR_PROJECTION_CACHE_PATH`.
The cache key includes source sidecar sizes and modification times, target CRS,
so changed source data is rebuilt automatically and valid entries survive a
server restart. Persistent entries contain the complete target-CRS dataset and
its spatial index; request bounds are applied while reading it. This produces
one reusable file per source and target CRS instead of one file per viewport.
Set the path to an empty string to disable persistent projection caching. In
that in-memory-only mode, `local` snaps projection work to a reusable metatile
envelope, while `full` projects the complete source.

`cangling-map` first reads the tile from `cangling-tile`. On a `404` cache miss it
renders the tile with GDAL, returns it to the WMTS client, and writes it back with
the configured Bearer token. A cache outage or failed write is logged but does not
prevent the freshly rendered tile from being returned.

Database IDs default to SHA-256 of the published layer `app_id`; item IDs are
SHA-256 of the published layer ID plus its style. Set
`CANGLING_TILE_DATABASE_ID` to a pre-existing 64-character database ID when a
deployment requires a fixed shared cache namespace.

The initial WMS endpoint intentionally returns a structured `OperationNotSupported`
exception until capabilities and rendering are implemented.

## Docker release

Release tags publish a GDAL-enabled multi-platform image for `linux/amd64` and
`linux/arm64`:

```text
soft.cangling.cn:22002/cangling/cangling-map:<version>
soft.cangling.cn:22002/cangling/cangling-map:latest
```

The build and runtime stages use the public OSGeo image
`ghcr.io/osgeo/gdal:ubuntu-small-3.13.0`. The version is pinned because
`cangling-gdal` targets GDAL 3.13; the upstream `latest` tag may move to a newer
development ABI.

`cangling-gdal-core` is included in this repository's Cargo workspace, so the
image can be built from a single checkout:

```bash
docker buildx build \
  -t cangling-map:local .
```

Configure the GitHub repository secrets `HARBOR_USERNAME` and
`HARBOR_PASSWORD` for registry publishing. Run
`./release.sh` from a clean worktree to execute preflight checks, increment the
patch version, create `v<version>`, and push the branch and tag.

## Planned milestones

1. SQLite-backed map catalog, WMS 1.3.0 parsing, exceptions, and `GetCapabilities`.
2. GeoTIFF `GetMap`, reprojection, resampling, PNG, and transparency.
3. Vector sources and point/line/polygon rendering.
4. SLD/SE subset and `GetFeatureInfo`.
5. Cache, dimensions, conformance testing, and OGC API - Maps facade.
