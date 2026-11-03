CREATE TABLE names (
    name TEXT PRIMARY KEY,
    prefix TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('counter','gauge','event')),
    meaning TEXT NOT NULL,
    attrs_json TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    last_value REAL
);

CREATE INDEX names_by_prefix ON names (prefix);
