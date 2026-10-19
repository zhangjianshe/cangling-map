ALTER TABLE published_layers ADD COLUMN min_scale_denominator REAL
    CHECK (min_scale_denominator IS NULL OR min_scale_denominator > 0);
ALTER TABLE published_layers ADD COLUMN max_scale_denominator REAL
    CHECK (max_scale_denominator IS NULL OR max_scale_denominator > 0);
ALTER TABLE published_layers ADD COLUMN simplify_factor REAL
    CHECK (simplify_factor IS NULL OR simplify_factor >= 0);

ALTER TABLE layer_group_members ADD COLUMN min_scale_denominator REAL
    CHECK (min_scale_denominator IS NULL OR min_scale_denominator > 0);
ALTER TABLE layer_group_members ADD COLUMN max_scale_denominator REAL
    CHECK (max_scale_denominator IS NULL OR max_scale_denominator > 0);
ALTER TABLE layer_group_members ADD COLUMN simplify_factor REAL
    CHECK (simplify_factor IS NULL OR simplify_factor >= 0);

UPDATE catalog_metadata SET value = '4' WHERE key = 'schema_version';
