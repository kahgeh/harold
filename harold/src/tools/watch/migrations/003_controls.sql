-- Rebuild watches: the end time may be empty, and a watch may be paused.
-- Every old column keeps its name, so an older Harold ignores the new ones.
CREATE TABLE watches_new (
    name TEXT PRIMARY KEY,
    folder TEXT NOT NULL,
    skill TEXT NOT NULL,
    report_to TEXT NOT NULL CHECK (report_to IN ('chief','self')),
    every_minutes INTEGER NOT NULL,
    every_run INTEGER NOT NULL CHECK (every_run IN (0,1)),
    run_budget_usd_micros INTEGER,
    state TEXT NOT NULL CHECK (state IN ('running','paused','stopped')),
    started_at_ms INTEGER NOT NULL,
    ends_at_ms INTEGER,
    next_run_at_ms INTEGER NOT NULL,
    runs INTEGER NOT NULL DEFAULT 0,
    reports INTEGER NOT NULL DEFAULT 0,
    failed_in_a_row INTEGER NOT NULL DEFAULT 0,
    cost_usd_micros INTEGER NOT NULL DEFAULT 0,
    last_outcome TEXT,
    stop_reason TEXT,
    ended_at_ms INTEGER,
    run_limit_secs INTEGER,
    paused_until_ms INTEGER,
    pause_how TEXT
);

INSERT INTO watches_new (name, folder, skill, report_to, every_minutes, every_run,
    run_budget_usd_micros, state, started_at_ms, ends_at_ms, next_run_at_ms, runs, reports,
    failed_in_a_row, cost_usd_micros, last_outcome, stop_reason, ended_at_ms, run_limit_secs)
SELECT name, folder, skill, report_to, every_minutes, every_run,
    run_budget_usd_micros, state, started_at_ms, ends_at_ms, next_run_at_ms, runs, reports,
    failed_in_a_row, cost_usd_micros, last_outcome, stop_reason, ended_at_ms, run_limit_secs
FROM watches;

DROP TABLE watches;
ALTER TABLE watches_new RENAME TO watches;

CREATE TABLE reminders (
    name TEXT PRIMARY KEY,
    folder TEXT NOT NULL,
    report_to TEXT NOT NULL CHECK (report_to IN ('chief','self')),
    text TEXT NOT NULL,
    due_at_ms INTEGER,
    due_local TEXT,
    state TEXT NOT NULL CHECK (state IN ('waiting','typed','dropped','cancelled')),
    created_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER
);
