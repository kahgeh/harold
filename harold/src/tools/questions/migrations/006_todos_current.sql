ALTER TABLE questions ADD COLUMN spec_file TEXT NOT NULL DEFAULT '';
CREATE TABLE checks (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    checked_at_ms INTEGER NOT NULL
);
