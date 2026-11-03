CREATE TABLE moves (
    record TEXT NOT NULL,
    from_status TEXT NOT NULL,
    to_status TEXT NOT NULL,
    at_ms INTEGER NOT NULL
);

CREATE INDEX moves_record ON moves(record);
