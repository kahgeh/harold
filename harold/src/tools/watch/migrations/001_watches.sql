CREATE TABLE watches (
    name TEXT PRIMARY KEY,
    folder TEXT NOT NULL,
    skill TEXT NOT NULL,
    report_to TEXT NOT NULL CHECK (report_to IN ('chief','self')),
    every_minutes INTEGER NOT NULL,
    every_run INTEGER NOT NULL CHECK (every_run IN (0,1)),
    run_budget_usd_micros INTEGER,
    state TEXT NOT NULL CHECK (state IN ('running','stopped')),
    started_at_ms INTEGER NOT NULL,
    ends_at_ms INTEGER NOT NULL,
    next_run_at_ms INTEGER NOT NULL,
    runs INTEGER NOT NULL DEFAULT 0,
    reports INTEGER NOT NULL DEFAULT 0,
    failed_in_a_row INTEGER NOT NULL DEFAULT 0,
    cost_usd_micros INTEGER NOT NULL DEFAULT 0,
    last_outcome TEXT,
    stop_reason TEXT,
    ended_at_ms INTEGER
);

CREATE TABLE runs (
    watch TEXT NOT NULL,
    folder TEXT NOT NULL,
    run INTEGER NOT NULL,
    started_at_ms INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    cost_usd_micros INTEGER
);

CREATE INDEX runs_by_start ON runs (started_at_ms);
