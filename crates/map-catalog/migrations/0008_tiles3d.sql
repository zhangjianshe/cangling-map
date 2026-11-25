CREATE TABLE tiles3d_datasets (
 id TEXT PRIMARY KEY, title TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 0,
 revision TEXT, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE tiles3d_versions (
 dataset_id TEXT NOT NULL REFERENCES tiles3d_datasets(id), revision TEXT NOT NULL,
 manifest TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
 PRIMARY KEY(dataset_id,revision)
);
