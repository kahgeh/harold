CREATE TABLE names (
    name TEXT PRIMARY KEY,
    prefix TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('counter','gauge','event')),
    meaning TEXT NOT NULL,
    attrs_json TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    -- A gauge's last values: a JSON map from a series key to a value.
    last_value TEXT
);

CREATE INDEX names_by_prefix ON names (prefix);
