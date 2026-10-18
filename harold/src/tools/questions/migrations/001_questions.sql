CREATE TABLE questions (
    id TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    position INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('waiting','deferred','answered','dismissed','sent')),
    body TEXT NOT NULL,
    answer TEXT,
    reason TEXT NOT NULL DEFAULT '',
    batch_id INTEGER,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE batches (
    id INTEGER PRIMARY KEY,
    text TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    typed_at_ms INTEGER
);
