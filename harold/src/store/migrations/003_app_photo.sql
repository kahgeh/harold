ALTER TABLE app_messages ADD COLUMN photo TEXT;

CREATE TABLE app_photo_uploads (
    id TEXT PRIMARY KEY,
    file TEXT NOT NULL UNIQUE,
    at_ms INTEGER NOT NULL,
    used INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_app_photo_uploads_unused ON app_photo_uploads(used, at_ms);
