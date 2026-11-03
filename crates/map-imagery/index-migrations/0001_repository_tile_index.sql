-- Repository tile presence is deliberately stored outside imagery.db. The
-- index is large, rebuildable metadata and should not inflate task backups.
CREATE TABLE imagery_tile_index_pages (
    repository_id TEXT NOT NULL,
    zoom          INTEGER NOT NULL,
    page          INTEGER NOT NULL,
    state         TEXT NOT NULL DEFAULT 'partial'
                  CHECK(state IN ('partial', 'full')),
    bits          BLOB,
    updated_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(repository_id, zoom, page),
    CHECK(
        (state = 'full' AND bits IS NULL)
        OR (state = 'partial' AND length(bits) = 4096)
    )
);

CREATE INDEX idx_imagery_tile_index_repository_zoom
    ON imagery_tile_index_pages(repository_id, zoom, page);

CREATE TABLE imagery_index_metadata (
    key        TEXT PRIMARY KEY NOT NULL,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
