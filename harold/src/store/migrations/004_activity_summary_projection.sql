ALTER TABLE agent_panes ADD COLUMN summary_basis_version INTEGER NOT NULL DEFAULT 0
    CHECK (summary_basis_version >= 0);
ALTER TABLE agent_panes ADD COLUMN generated_work_summary TEXT
    CHECK (generated_work_summary IS NULL OR length(generated_work_summary) BETWEEN 1 AND 160);
ALTER TABLE agent_panes ADD COLUMN generated_summary_basis_version INTEGER
    CHECK (generated_summary_basis_version IS NULL OR generated_summary_basis_version >= 0)
    CHECK ((generated_work_summary IS NULL) = (generated_summary_basis_version IS NULL));
