CREATE TABLE system_settings (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

UPDATE catalog_metadata SET value = '5' WHERE key = 'schema_version';
