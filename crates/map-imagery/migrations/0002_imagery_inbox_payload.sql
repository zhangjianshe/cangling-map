ALTER TABLE imagery_inbox ADD COLUMN source TEXT;
ALTER TABLE imagery_inbox ADD COLUMN leader_term INTEGER NOT NULL DEFAULT 0;
ALTER TABLE imagery_inbox ADD COLUMN payload TEXT NOT NULL DEFAULT '{}';
ALTER TABLE imagery_inbox ADD COLUMN status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE imagery_inbox ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE imagery_inbox ADD COLUMN next_attempt_at TEXT;
ALTER TABLE imagery_inbox ADD COLUMN applied_at TEXT;
ALTER TABLE imagery_inbox ADD COLUMN last_error TEXT;

CREATE INDEX idx_imagery_inbox_pending
    ON imagery_inbox(status, next_attempt_at, processed_at, event_id);
