CREATE TABLE catalog_metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;

INSERT INTO catalog_metadata (key, value)
VALUES ('schema_version', '1');

CREATE TABLE layers (
    id TEXT PRIMARY KEY NOT NULL,
    app_id TEXT NOT NULL,
    catalog_path TEXT NOT NULL,
    name TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    abstract_text TEXT,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('raster', 'vector')),
    source_config TEXT NOT NULL CHECK (json_valid(source_config)),
    native_crs TEXT NOT NULL,
    min_x REAL NOT NULL,
    min_y REAL NOT NULL,
    max_x REAL NOT NULL,
    max_y REAL NOT NULL,
    queryable INTEGER NOT NULL DEFAULT 0 CHECK (queryable IN (0, 1)),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    attribution TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (app_id, catalog_path),
    CHECK (min_x < max_x),
    CHECK (min_y < max_y)
) STRICT;

CREATE TABLE layer_crs (
    layer_id TEXT NOT NULL REFERENCES layers(id) ON DELETE CASCADE,
    crs TEXT NOT NULL,
    PRIMARY KEY (layer_id, crs)
) STRICT;

CREATE TABLE layer_styles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    layer_id TEXT NOT NULL REFERENCES layers(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    title TEXT NOT NULL,
    format TEXT NOT NULL,
    definition TEXT NOT NULL,
    is_default INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0, 1)),
    UNIQUE (layer_id, name)
) STRICT;

CREATE UNIQUE INDEX one_default_style_per_layer
ON layer_styles(layer_id)
WHERE is_default = 1;

CREATE INDEX layers_enabled_name ON layers(enabled, name);
