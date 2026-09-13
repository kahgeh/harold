# Architecture readability

The user approved the proposed rewrite: begin with one agent-task walkthrough,
introduce components after their purpose, and link to reference pages for exact
reconciliation and repair rules. This is a bounded documentation change.

## Plan

- [x] Review the existing explanation, reference coverage, and relevant lessons.
- [x] Verify daemon, dashboard, notification, and reply boundaries against source.
- [x] Rewrite the explanation for a developer new to Harold, preserving useful link anchors.
- [x] Check local links, removed-detail coverage, and the diff.
- [x] Obtain an independent completion review and address findings.

## Acceptance

- The opening explains the user-visible purpose without internal terminology.
- One concrete task connects observation, stored state, and dashboard updates.
- Main components and their ownership are explained before edge cases.
- Notification/reply flows and restart behavior have concise explanations.
- Exact rules remain discoverable in existing reference pages.
- Only documentation and this task record change; no dependencies are needed.

## Review

- Source exploration confirmed the embedded events library, separate dashboard,
  completion/reply paths, and commit-before-publication-before-delivery ordering.
- All 11 local links and their heading anchors resolve. The existing incoming
  scrollback-recovery anchor is preserved.
- Detailed reconciliation, legacy repair, provider recovery, and generated-summary
  rules remain available in the linked references.
- `git diff --check` passed. No runtime code or dependencies changed, so runtime
  builds/tests are not relevant to this edit.
- Independent completion reviewer approved with no findings. The review checked
  newcomer readability, source accuracy, architecture constraints, and links.

The follow-up [fresh-install consolidation](../fresh-install-state/todo.md) removes
the historical repair behavior and replaces its reference coverage with the current
fresh-install schema and input-validation contract.
