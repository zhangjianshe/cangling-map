CREATE TABLE catalog_items (
    id TEXT PRIMARY KEY NOT NULL,
    app_id TEXT NOT NULL,
    catalog_path TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('raster', 'vector')),
    source_config TEXT NOT NULL CHECK (json_valid(source_config)),
    native_crs TEXT NOT NULL,
    min_x REAL NOT NULL, min_y REAL NOT NULL, max_x REAL NOT NULL, max_y REAL NOT NULL,
    attribution TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (app_id, catalog_path), CHECK (min_x < max_x), CHECK (min_y < max_y)
) STRICT;

INSERT INTO catalog_items
SELECT id, app_id, catalog_path, title, abstract_text, source_kind, source_config,
       native_crs, min_x, min_y, max_x, max_y, attribution, created_at, updated_at
FROM layers;

CREATE TABLE published_layers (
    id TEXT PRIMARY KEY NOT NULL,
    app_id TEXT NOT NULL,
    name TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    catalog_item_id TEXT NOT NULL REFERENCES catalog_items(id) ON DELETE RESTRICT,
    default_style_id TEXT,
    queryable INTEGER NOT NULL DEFAULT 0 CHECK (queryable IN (0, 1)),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (app_id, name)
) STRICT;

INSERT INTO published_layers
SELECT id, app_id, name, title, abstract_text, id, NULL, queryable, enabled, created_at, updated_at
FROM layers;

CREATE TABLE published_layer_crs (
    published_layer_id TEXT NOT NULL REFERENCES published_layers(id) ON DELETE CASCADE,
    crs TEXT NOT NULL,
    PRIMARY KEY (published_layer_id, crs)
) STRICT;

INSERT INTO published_layer_crs SELECT layer_id, crs FROM layer_crs;

CREATE TABLE styles_v2 (
    id TEXT PRIMARY KEY NOT NULL,
    app_id TEXT NOT NULL,
    name TEXT NOT NULL,
    title TEXT NOT NULL,
    format TEXT NOT NULL,
    definition TEXT NOT NULL,
    UNIQUE (app_id, name)
) STRICT;

INSERT INTO styles_v2
SELECT printf('legacy-style-%d', s.id), l.app_id, s.name, s.title, s.format, s.definition
FROM layer_styles s JOIN layers l ON l.id = s.layer_id;

CREATE TABLE published_layer_styles (
    published_layer_id TEXT NOT NULL REFERENCES published_layers(id) ON DELETE CASCADE,
    style_id TEXT NOT NULL REFERENCES styles_v2(id) ON DELETE RESTRICT,
    is_default INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0, 1)),
    PRIMARY KEY (published_layer_id, style_id)
) STRICT;

INSERT INTO published_layer_styles
SELECT s.layer_id, printf('legacy-style-%d', s.id), s.is_default FROM layer_styles s;

CREATE TABLE layer_groups (
    id TEXT PRIMARY KEY NOT NULL,
    app_id TEXT NOT NULL,
    name TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    UNIQUE (app_id, name)
) STRICT;

CREATE TABLE layer_group_members (
    group_id TEXT NOT NULL REFERENCES layer_groups(id) ON DELETE CASCADE,
    published_layer_id TEXT NOT NULL REFERENCES published_layers(id) ON DELETE RESTRICT,
    style_id TEXT REFERENCES styles_v2(id) ON DELETE RESTRICT,
    position INTEGER NOT NULL,
    opacity REAL NOT NULL DEFAULT 1.0 CHECK (opacity >= 0.0 AND opacity <= 1.0),
    PRIMARY KEY (group_id, position), UNIQUE (group_id, published_layer_id)
) STRICT;

DROP TABLE layer_styles;
DROP TABLE layer_crs;
DROP TABLE layers;
ALTER TABLE styles_v2 RENAME TO styles;
UPDATE catalog_metadata SET value = '2' WHERE key = 'schema_version';
CREATE INDEX idx_catalog_items_app_path ON catalog_items(app_id, catalog_path);
CREATE INDEX idx_published_layers_enabled_name ON published_layers(enabled, name);
