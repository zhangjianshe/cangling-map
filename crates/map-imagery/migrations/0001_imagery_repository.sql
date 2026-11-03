CREATE TABLE imagery_repositories (
    id                  TEXT PRIMARY KEY NOT NULL,
    name                TEXT NOT NULL UNIQUE,
    description         TEXT,
    source_url_template TEXT NOT NULL,
    min_zoom            INTEGER NOT NULL,
    max_zoom            INTEGER NOT NULL,
    min_x               REAL NOT NULL,
    min_y               REAL NOT NULL,
    max_x               REAL NOT NULL,
    max_y               REAL NOT NULL,
    format              TEXT NOT NULL,
    tile_database       TEXT NOT NULL,
    tile_item           TEXT NOT NULL,
    revocable           INTEGER NOT NULL DEFAULT 0,
    version             INTEGER NOT NULL DEFAULT 1,
    created_at          TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at          TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE imagery_jobs (
    id              TEXT PRIMARY KEY NOT NULL,
    repository_id   TEXT NOT NULL REFERENCES imagery_repositories(id) ON DELETE RESTRICT,
    mode            TEXT NOT NULL,
    status          TEXT NOT NULL,
    total_tiles     INTEGER NOT NULL DEFAULT 0,
    completed_tiles INTEGER NOT NULL DEFAULT 0,
    failed_tiles    INTEGER NOT NULL DEFAULT 0,
    skipped_tiles   INTEGER NOT NULL DEFAULT 0,
    leader_term     INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_imagery_jobs_repository ON imagery_jobs(repository_id, created_at DESC);
CREATE INDEX idx_imagery_jobs_status ON imagery_jobs(status, updated_at);

CREATE TABLE imagery_chunks (
    id                TEXT PRIMARY KEY NOT NULL,
    job_id            TEXT NOT NULL REFERENCES imagery_jobs(id) ON DELETE CASCADE,
    zoom              INTEGER NOT NULL,
    min_col           INTEGER NOT NULL,
    max_col           INTEGER NOT NULL,
    min_row           INTEGER NOT NULL,
    max_row           INTEGER NOT NULL,
    status            TEXT NOT NULL DEFAULT 'pending',
    assigned_instance TEXT,
    lease_id          TEXT,
    lease_expires_at  TEXT,
    attempts          INTEGER NOT NULL DEFAULT 0,
    completed_tiles   INTEGER NOT NULL DEFAULT 0,
    failed_tiles      INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX idx_imagery_chunks_job_status ON imagery_chunks(job_id, status, zoom);

CREATE TABLE imagery_tile_receipts (
    job_id       TEXT NOT NULL REFERENCES imagery_jobs(id) ON DELETE CASCADE,
    zoom         INTEGER NOT NULL,
    col          INTEGER NOT NULL,
    row          INTEGER NOT NULL,
    format       TEXT NOT NULL,
    size_bytes   INTEGER NOT NULL,
    sha256       TEXT NOT NULL,
    stored_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(job_id, zoom, col, row, format)
);

CREATE TABLE imagery_outbox (
    id             TEXT PRIMARY KEY NOT NULL,
    event_type     TEXT NOT NULL,
    aggregate_type TEXT NOT NULL,
    aggregate_id   TEXT NOT NULL,
    payload        TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending',
    attempts       INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    created_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    published_at   TEXT
);
CREATE INDEX idx_imagery_outbox_pending ON imagery_outbox(status, next_attempt_at, created_at);

CREATE TABLE imagery_inbox (
    event_id     TEXT PRIMARY KEY NOT NULL,
    event_type   TEXT NOT NULL,
    processed_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
