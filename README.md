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

Keep cache tokens in the process environment or a local secret manager; do not
commit them to the repository.

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

## Planned milestones

1. SQLite-backed map catalog, WMS 1.3.0 parsing, exceptions, and `GetCapabilities`.
2. GeoTIFF `GetMap`, reprojection, resampling, PNG, and transparency.
3. Vector sources and point/line/polygon rendering.
4. SLD/SE subset and `GetFeatureInfo`.
5. Cache, dimensions, conformance testing, and OGC API - Maps facade.
