ALTER TABLE styles ADD COLUMN description TEXT;
ALTER TABLE styles ADD COLUMN geometry_type TEXT NOT NULL DEFAULT 'polygon'
    CHECK (geometry_type IN ('polygon'));
ALTER TABLE styles ADD COLUMN source_format TEXT NOT NULL DEFAULT 'geocss'
    CHECK (source_format IN ('geocss'));
ALTER TABLE styles ADD COLUMN source_text TEXT NOT NULL DEFAULT '';
ALTER TABLE styles ADD COLUMN compiled_ir TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(compiled_ir));
ALTER TABLE styles ADD COLUMN compiled_sld TEXT NOT NULL DEFAULT '';
ALTER TABLE styles ADD COLUMN compiler_version TEXT NOT NULL DEFAULT 'legacy';
ALTER TABLE styles ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0);
ALTER TABLE styles ADD COLUMN content_hash TEXT NOT NULL DEFAULT '';
ALTER TABLE styles ADD COLUMN validation_error TEXT;
ALTER TABLE styles ADD COLUMN created_at TEXT NOT NULL DEFAULT '';
ALTER TABLE styles ADD COLUMN updated_at TEXT NOT NULL DEFAULT '';

UPDATE styles
SET source_format = CASE WHEN lower(format) = 'css' THEN 'geocss' ELSE 'geocss' END,
    source_text = definition,
    compiler_version = 'legacy-import',
    validation_error = '旧版样式尚未重新编译',
    created_at = CURRENT_TIMESTAMP,
    updated_at = CURRENT_TIMESTAMP;

UPDATE catalog_metadata SET value = '3' WHERE key = 'schema_version';
CREATE INDEX idx_styles_app_name ON styles(app_id, name);
