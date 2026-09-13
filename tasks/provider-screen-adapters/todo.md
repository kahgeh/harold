# Provider Screen Adapters

- [x] Capture the approved behavior and architecture in `spec.md` and `design.md`.
- [x] Obtain an independent consistency review of the saved spec and design; final verdict: approved with no material ambiguity or contradiction.
- [x] User approved implementation of the saved adapter design and the proposed parser/recovery/live tests on 2026-09-09.
- [x] Write `implementation-plan.md` after approval, preserving current Sonnet integration.
- [x] Implement through RED/GREEN TDD with independent completion review; final implementation and live acceptance approved.
- [x] Run live Codex scrollback recovery acceptance (ST-033) and reconcile durable architecture, adapter reference, activity-summary reference, and hook setup documentation.

## Review Record

- Verified the exact default capture command, 2,000-row default, 10,000-row maximum, and visible-grid addition are stated consistently.
- Verified safe late-join baselining, bounded baseline retries, Busy/Idle recovery triggers, and semantic-recency behavior are explicit.
- Verified only proven submitted blocks become checkpoint anchors and that the adapter/runtime/reducer ownership diagrams match the written contract.
- `git diff --check` passed before commit.

## Implementation results — 2026-09-10

- Source and test evidence: `adapter-report.md` and `recovery-report.md`. User-approved implementation preserves the existing Sonnet feature, public protobuf, dependency manifests/lockfile, and events submodule.
- Current live Codex 0.153.4 evidence refined the old design: a nonempty composer is normal text, so submission requires a proven styled marker/separator boundary. Parser fixtures include this actual distinction.
- Review found and fixed malformed-style carryover and a Busy source-append failure losing its Idle retry. Tests also exposed acquisition completion notifying before releasing the shared gate; release now precedes notification. The prior subprocess readiness test has 10s startup headroom while preserving its 2s cleanup assertion.
- Final automatic gate: 381 workspace tests passed, 0 failed, 2 opt-in ignored. Formatting, warnings-denied workspace Clippy, offline release build, and diff check passed. The production-port real-tmux history-bound test passed separately.
- Real acceptance: [ST-033](../tmux-agent-dashboard/screen-testing.md#st-033--provider-adapters-deep-codex-recovery-pane-reuse-and-sonnet) proves exact deep source before AI, unsent-draft rejection, retained-history pane replacement isolation, new-process recovery, and real Sonnet/dashboard delivery. Its controlled Idle marker is documented and does not claim stock Codex streaming lifecycle accuracy.

## Final review and deployment — 2026-09-10

- `screen_port_review` independently approved the parser/capture/settings slice after its malformed-style fix. `screen_completion_review` independently approved runtime, checkpoint, reducer, privacy and acquisition behavior, inspected the complete live artifacts, and returned **APPROVED** for implementation and live acceptance. No review findings remain.
- Gracefully stopped exact prior deploy-owned PID 42929, installed and verified the signed release, copied the reviewed default/template configuration, and launched `/Users/kahgeh/bin/harold/harold` as PID **47190** with cwd `/Users/kahgeh/bin/harold` and explicit `/Users/kahgeh/bin/harold/config`/local overlay. `lsof` verified the surviving `127.0.0.1:50060` listener and the existing `/Users/kahgeh/bin/harold/data/events/harold-state.db`.
- Deployed Codex defaults select `screen_adapter="codex-v1"`, `screen_history_lines=2000`; existing omitted settings retain generic behavior for Claude/OpenCode as documented. Both local configuration files are byte-for-byte unchanged, preserving enabled Sonnet and all notification settings.
- Verified installed code signature and SHA-256 `c43569a335460a3ee90849eb2fc01aaac810993bb623086a66c756b8307fb411`. Backup: `/Users/kahgeh/bin/harold/backups/screen-adapters-20260910-000823`. Deployment evidence: `/tmp/harold-screen-deployment.json`.
- Deployed WatchAgentStates returned revision 3653, 11 panes, and healthy inventory/screen status. No synthetic RPC was sent to production. First captures establish fresh recovery baselines; existing pre-baseline scrollback is intentionally not adopted.
- Cleaned up the exact disposable tmux session and copied temporary Codex authentication file after acceptance. Durable docs link checks and final diff checks passed. Dependencies, events submodule, protobuf and dashboard production source remain unchanged.
- At deployment, changes were uncommitted in the existing feature branch alongside the previously requested Sonnet work. The combined publication is tracked in [AI activity summaries](../ai-activity-summaries/todo.md#publication--2026-09-13).
