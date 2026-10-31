# 22. Harold works with one multiplexer, chosen at start, and Rex has its own implementation of each port

Date: 2026-10-31
Status: Accepted (rex-support). Builds on 0001.

## Context

Harold finds, reads and types into agent panes with tmux commands only. The user moves agents to Rex, a terminal multiplexer with its own `rex` command. The user wants a quick cut-over, with the option to switch back. Other people keep tmux. Stored pane ids, snapshots and tmux tests must not change.

## Decision

Harold works with one multiplexer at a time. The setting `[agent_monitor] multiplexer` (`"tmux"` by default, or `"rex"`) picks it. Harold reads the setting once, at start. A change needs a restart. Any other value stops Harold at start.

Each port (`AgentInventoryPort`, `PaneCapturePort`, `VisibleScreenPort` and `Ports`) gets a Rex implementation next to the tmux one. At start, Harold builds the inventory and the screen reader for the chosen multiplexer. The typing steps of `Ports` name one pane id, so they act by the form of that id: a `block:` id goes to Rex and a `%` id goes to tmux. Only one multiplexer is watched, so only the chosen form reaches a typing step. A pane id of the other multiplexer's form is refused at the commands and the daemon's calls.

A pane id keeps its multiplexer's own form: `%` and digits for tmux, `block:` and the block id for Rex. The pane label is `<session>:<window>.<pane>` for both, and Harold stores it in the existing `tmux_target` field.

Harold calls the user-installed `rex` program as a subprocess, as it calls `tmux`. It finds `rex` from the `[agent_monitor] rex` setting, then the running server's own copy, then `PATH`, then the Rex app bundle. Harold never starts a Rex server. The dash (`tmx-agent-dash`) decides by the form of the pane id.

## Consequences

- No store migration. Existing ids and tmux tests stay the same.
- Harold never sees agents in the other multiplexer, so both cannot be watched at once. The user stops the pods in one multiplexer before a switch.
- A later multiplexer needs one more implementation behind each port and one more setting value.
- Harold depends on the `rex` command line, not on a Rust crate, so no new dependency enters the build.
- A Rex screen read uses a tail read, so its cost stays flat for a long scrollback. It is watched with `agent screen read slow`.
- Names such as `tmux_target` stay until a later rename (glossary, Not yet consistent).
