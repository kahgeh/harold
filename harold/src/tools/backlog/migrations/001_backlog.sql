CREATE TABLE items (
    product TEXT NOT NULL,
    id TEXT NOT NULL,
    rank INTEGER NOT NULL,
    title TEXT NOT NULL,
    value TEXT NOT NULL DEFAULT '',
    size TEXT NOT NULL DEFAULT '',
    risk TEXT NOT NULL DEFAULT '',
    reason TEXT NOT NULL DEFAULT '',
    paths TEXT NOT NULL DEFAULT '',
    after TEXT NOT NULL DEFAULT '',
    evidence_json TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (product, id)
);

CREATE TABLE notes (
    id INTEGER PRIMARY KEY,
    product TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('rule','history','held','left_out')),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE queue_items (
    id TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('now','parked','done')),
    position INTEGER NOT NULL,
    text TEXT NOT NULL,
    done_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL
);

CREATE UNIQUE INDEX one_now ON queue_items(state) WHERE state = 'now';

CREATE TABLE queue_note (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    text TEXT NOT NULL
);
