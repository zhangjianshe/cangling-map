CREATE TABLE dem_datasets (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    revision TEXT NOT NULL,
    vertical_datum TEXT NOT NULL,
    attribution TEXT,
    min_zoom INTEGER NOT NULL DEFAULT 0,
    max_zoom INTEGER NOT NULL DEFAULT 18,
    min_x REAL NOT NULL, min_y REAL NOT NULL, max_x REAL NOT NULL, max_y REAL NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CHECK (min_x < max_x AND min_y < max_y)
) STRICT;

CREATE TABLE dem_assets (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dataset_id TEXT NOT NULL REFERENCES dem_datasets(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    native_crs TEXT NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    resolution_m REAL NOT NULL,
    nodata REAL,
    unit TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    min_x REAL NOT NULL, min_y REAL NOT NULL, max_x REAL NOT NULL, max_y REAL NOT NULL,
    UNIQUE(dataset_id, path)
) STRICT;

CREATE VIRTUAL TABLE dem_asset_bounds USING rtree(id, min_x, max_x, min_y, max_y);
CREATE TRIGGER dem_assets_insert AFTER INSERT ON dem_assets BEGIN
    INSERT INTO dem_asset_bounds VALUES (new.id, new.min_x, new.max_x, new.min_y, new.max_y);
END;
CREATE TRIGGER dem_assets_delete AFTER DELETE ON dem_assets BEGIN
    DELETE FROM dem_asset_bounds WHERE id = old.id;
END;
