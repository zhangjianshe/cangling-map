ALTER TABLE imagery_jobs ADD COLUMN min_zoom INTEGER;
ALTER TABLE imagery_jobs ADD COLUMN max_zoom INTEGER;
ALTER TABLE imagery_jobs ADD COLUMN min_x REAL;
ALTER TABLE imagery_jobs ADD COLUMN min_y REAL;
ALTER TABLE imagery_jobs ADD COLUMN max_x REAL;
ALTER TABLE imagery_jobs ADD COLUMN max_y REAL;

UPDATE imagery_jobs
SET min_zoom=(SELECT min_zoom FROM imagery_repositories WHERE id=imagery_jobs.repository_id),
    max_zoom=(SELECT max_zoom FROM imagery_repositories WHERE id=imagery_jobs.repository_id),
    min_x=(SELECT min_x FROM imagery_repositories WHERE id=imagery_jobs.repository_id),
    min_y=(SELECT min_y FROM imagery_repositories WHERE id=imagery_jobs.repository_id),
    max_x=(SELECT max_x FROM imagery_repositories WHERE id=imagery_jobs.repository_id),
    max_y=(SELECT max_y FROM imagery_repositories WHERE id=imagery_jobs.repository_id);

ALTER TABLE imagery_chunks ADD COLUMN planned_tiles INTEGER NOT NULL DEFAULT 0;
UPDATE imagery_chunks
SET planned_tiles=(max_col-min_col+1)*(max_row-min_row+1);

-- Sparse repository-level bitmap. Each row stores 32,768 tile states and is
-- allocated only after at least one tile in that page has been stored.
CREATE TABLE imagery_tile_index_pages (
    repository_id TEXT NOT NULL REFERENCES imagery_repositories(id) ON DELETE CASCADE,
    zoom          INTEGER NOT NULL,
    page          INTEGER NOT NULL,
    bits          BLOB NOT NULL,
    updated_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(repository_id, zoom, page)
);

CREATE INDEX idx_imagery_tile_index_repository_zoom
    ON imagery_tile_index_pages(repository_id, zoom, page);
