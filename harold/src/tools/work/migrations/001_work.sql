CREATE TABLE records (
    name TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    priority TEXT NOT NULL CHECK (priority IN ('focus','next','later')),
    status TEXT NOT NULL,
    waits_for TEXT NOT NULL DEFAULT '',
    next_step TEXT NOT NULL DEFAULT '',
    created_in TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    status_since_ms INTEGER NOT NULL
);

CREATE INDEX records_status_priority ON records(status, priority);

CREATE TABLE decisions (
    record TEXT NOT NULL,
    n INTEGER NOT NULL,
    at TEXT NOT NULL,
    who TEXT NOT NULL,
    mark TEXT NOT NULL CHECK (mark IN ('agreed','draft','dropped','open')),
    text TEXT NOT NULL,
    PRIMARY KEY (record, n)
);

CREATE TABLE links (
    record TEXT NOT NULL,
    link TEXT NOT NULL,
    PRIMARY KEY (record, link)
);
