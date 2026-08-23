# Investigate Codex agent routing

- [x] Read relevant project lessons and locate setup/routing files.
- [x] Inspect documented Codex hook and inbound routing assumptions.
- [x] Inspect live tmux/process state for Claude/Codex pane detection.
- [x] Check Harold config and logs for inbound delivery errors.
- [x] Summarize root cause, evidence, and verification.
- [x] Update tmux agent process detection for Codex.
- [x] Update docs/tests for Codex-aware detection.
- [x] Run focused verification and completion review.
- [x] Replace exact/prefix pane-current-command matching with descendant process command matching.
- [x] Simplify agent process config to one command contains list.
- [x] Re-run completion review and deploy.
- [x] Fix pre-existing clippy warning in outbound notification deduplication.

## Review

Root cause: Harold's inbound routing still discovers live agents using a Claude Code process-name heuristic. `harold/src/inbound/tmux.rs` only accepts panes whose `pane_current_command` is a dotted numeric Node version such as `20.11.0`. Current Codex panes report `codex-aarch64-a`, so they are invisible to routing and would also fail the later liveness check.

Evidence:

- `tmux list-panes -a -F '#{pane_id}|#{session_name}:#{window_index}.#{pane_index}|#{pane_current_command}'` showed current Codex panes as `codex-aarch64-a`.
- The same scan filtered through Harold's semver heuristic only returned `%17|greenfields-of-cambridge:0.5|2.1.145`.
- `~/bin/harold/harold --diagnostics` reported `live panes : ["greenfields-of-cambridge:0.5"]`, excluding the active Codex panes including `harold:0.3`.
- Codex hooks are present in `~/.codex/config.toml` and `~/.codex/hooks/turn_complete.py`, so the most direct failure is inbound discovery/routing, not missing Codex hook registration.

Fix: use each tmux pane's `pane_pid` to inspect the pane process and descendant process commands. A pane is considered an agent when a command contains one of the configured `[agents].command_contains` fragments. Defaults recognize Claude Code and Codex with `command_contains = ["claude", "codex"]`, so future agent binary renames can be handled in config.

Verification:

- `cargo test -p harold`
- `git diff --check`
- `cargo clippy -p harold -- -D warnings -A clippy::collapsible-if` (allows a pre-existing unrelated lint in `harold/src/outbound/mod.rs`)
- `cargo clippy -p harold -- -D warnings`
- `cargo test -p harold inbound::tmux -- --nocapture`
- `cargo test -p harold`
- `git diff --check`
- `cargo clippy -p harold -- -D warnings -A clippy::collapsible-if` (allows a pre-existing unrelated lint in `harold/src/outbound/mod.rs`)
- `HAROLD_CONFIG_DIR=/Users/kahgeh/bin/harold/config /Users/kahgeh/Dev/p/harold/target/debug/harold --diagnostics` showed live panes including Claude/Codex sessions and routed "ask harold to check logs" to `harold:0.3`.
