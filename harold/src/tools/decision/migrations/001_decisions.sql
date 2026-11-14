CREATE TABLE decisions (
    id TEXT PRIMARY KEY,
    decided_on TEXT NOT NULL,
    organisation TEXT NOT NULL,
    role TEXT NOT NULL,
    kind TEXT NOT NULL,
    details_owner TEXT NOT NULL,
    accountable TEXT NOT NULL,
    title TEXT NOT NULL,
    link TEXT NOT NULL DEFAULT '',
    check_on TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('open','due','evaluated')),
    process_grade TEXT NOT NULL DEFAULT '',
    outcome_grade TEXT NOT NULL DEFAULT '',
    details TEXT NOT NULL,
    asked_at_ms INTEGER NOT NULL DEFAULT 0,
    added_at_ms INTEGER NOT NULL,
    evaluated_at_ms INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX decisions_status_check_on ON decisions(status, check_on);
