CREATE TABLE last_processed_event (
    namespace TEXT NOT NULL,
    partition_key TEXT NOT NULL,
    last_processed_event_version INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (namespace, partition_key)
);

CREATE TABLE delivery_outbox (
    event_id TEXT PRIMARY KEY,
    event_version INTEGER NOT NULL UNIQUE,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    trace_id TEXT NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    delivered_at_ms INTEGER
);
CREATE INDEX idx_delivery_outbox_pending
    ON delivery_outbox(delivered_at_ms, event_version);

CREATE TABLE agent_panes (
    pane_id TEXT PRIMARY KEY,
    pane_pid INTEGER NOT NULL,
    agent_pid INTEGER NOT NULL,
    agent_started_at_ms INTEGER NOT NULL,
    provider_id TEXT NOT NULL,
    tmux_target TEXT NOT NULL,
    session_name TEXT NOT NULL,
    window_index INTEGER NOT NULL,
    pane_index INTEGER NOT NULL,
    working_directory TEXT NOT NULL,
    provider_display_name TEXT NOT NULL,
    pane_observed_at_ms INTEGER NOT NULL,
    hook_state TEXT,
    hook_observed_at_ms INTEGER,
    screen_state TEXT,
    screen_classifier_id TEXT,
    screen_observed_at_ms INTEGER,
    effective_state TEXT NOT NULL,
    explicit_work_summary TEXT,
    explicit_work_summary_updated_at_ms INTEGER,
    screen_work_summary TEXT,
    screen_work_summary_updated_at_ms INTEGER,
    work_summary TEXT,
    summary_basis_version INTEGER NOT NULL DEFAULT 0 CHECK (summary_basis_version >= 0),
    generated_work_summary TEXT
        CHECK (generated_work_summary IS NULL OR length(generated_work_summary) BETWEEN 1 AND 160),
    generated_summary_basis_version INTEGER
        CHECK (generated_summary_basis_version IS NULL OR generated_summary_basis_version >= 0),
    last_transition_at_ms INTEGER NOT NULL,
    last_event_version INTEGER NOT NULL,
    CHECK ((generated_work_summary IS NULL) = (generated_summary_basis_version IS NULL)),
    CHECK (hook_state IS NULL OR hook_state IN ('busy', 'idle')),
    CHECK (screen_state IS NULL OR screen_state IN ('busy', 'idle')),
    CHECK (effective_state IN ('busy', 'idle', 'unknown')),
    CHECK ((hook_state IS NULL) = (hook_observed_at_ms IS NULL)),
    CHECK ((screen_state IS NULL) = (screen_classifier_id IS NULL)),
    CHECK ((screen_state IS NULL) = (screen_observed_at_ms IS NULL)),
    CHECK ((explicit_work_summary IS NULL) = (explicit_work_summary_updated_at_ms IS NULL)),
    CHECK ((screen_work_summary IS NULL) = (screen_work_summary_updated_at_ms IS NULL)),
    CHECK (explicit_work_summary IS NULL OR length(explicit_work_summary) <= 160),
    CHECK (screen_work_summary IS NULL OR length(screen_work_summary) <= 160),
    CHECK (work_summary IS NULL OR length(work_summary) <= 160)
);

CREATE TABLE agent_monitor_health (
    component TEXT PRIMARY KEY,
    healthy INTEGER NOT NULL CHECK (healthy IN (0, 1)),
    reason_code TEXT NOT NULL CHECK (length(reason_code) <= 160),
    observed_at_ms INTEGER NOT NULL,
    last_event_version INTEGER NOT NULL
);
