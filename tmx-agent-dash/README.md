# Tmux Agent Dashboard

`tmx-agent-dash` is a keyboard-driven terminal view of the agent-pane projection
published by Harold. It shows transport and monitor health separately, keeps the
last committed rows visible through degraded or stale periods, and can switch
the invoking tmux client to the selected pane.

## Prerequisites

- Rust 1.97.1 (the currently verified toolchain)
- tmux 3.6a (the currently verified version) for pane navigation
- Harold with the `WatchAgentStates` gRPC service available
- The containing Harold workspace, with the shared API crate at `../harold-api`

Viewing works outside tmux, but pane navigation is disabled. Start the dashboard
inside the tmux client that should move when you press `Enter`. When several
clients are attached to that session, tmux moves the most recently active one.

From the containing Harold workspace root, build every release binary:

```sh
make build
```

Install the same workspace revision, including the signed on-demand dashboard
command at `~/bin/tmx-agent-dash`:

```sh
make deploy
```

For standalone dashboard development from this package directory, use Cargo:

```sh
cargo build --release
```

The resulting development executable is `../target/release/tmx-agent-dash`.

## Run

Harold's loopback endpoint is the default:

```sh
cargo run
```

Select a different endpoint explicitly:

```sh
cargo run -- --endpoint http://127.0.0.1:6000
```

The command accepts one optional `--endpoint` argument. The value must be an
absolute `http://` URI with an authority. There is no environment-variable or
configuration-file override.

The loopback default is the intended local security boundary. An explicitly
configured non-loopback endpoint is accepted, but transport authentication is
outside this MVP; use a trusted deployment boundary for remote connections.

## Keys

| Key | Action |
| --- | --- |
| `j` / Down | Select the next visible row |
| `k` / Up | Select the previous visible row |
| `g` / `G` | Select the first / last visible row |
| `l` / Right | Select the next waiting agent (wraps) |
| `h` / Left | Select the previous waiting agent (wraps) |
| `f` | Enter search editing |
| `/` | Open the command palette (type to filter, Up/Down choose, Enter run, Esc close; e.g. `Voice: mute` / `Voice: unmute`) |
| Printable text | Extend the query while editing |
| Backspace | Remove the last Unicode scalar value while editing |
| `Enter` | Accept the query while editing; otherwise switch the invoking tmux client to the selected pane |
| `Esc` | While editing, clear the query and leave editing; otherwise clear an accepted filter, or do nothing when no filter exists |
| `r` | Retry the Harold connection immediately |
| `q` | Quit outside search editing or the palette; enter `q` into the query while editing |

Search is a local, case-insensitive filter over provider, work summary, tmux
target, and working directory. It never sends a search request to Harold and it
does not pause incoming snapshots. The search line reports visible and total
row counts while a filter is active. Press `f` to start editing the search query.

The masthead shows the confirmed voice state as `● VOICE ON` or `○ VOICE MUTED`,
taken from the Harold snapshot rather than from local guesses. Voice commands are
offered in the palette only while the connection is LIVE and a snapshot has
arrived; otherwise the palette shows `Commands unavailable until connected`.
Before the first snapshot the masthead shows `○ VOICE —`.

## Reading status

Each row has Harold's latest classified agent state:

- `BUSY` means Harold currently classifies the agent as working.
- `IDLE` means Harold currently classifies the agent as waiting.
- `UNKNOWN` means Harold cannot yet classify the agent conclusively.

Transport and monitor health describe different system-level conditions:

- `LIVE` means the Harold stream delivered the current accepted snapshot.
- `CONNECTING` and `UNAVAILABLE` describe connection startup or retry.
- `STALE` means the stream disconnected after a snapshot; the last committed
  rows remain visible and are labelled stale.
- `MONITOR DEGRADED` takes precedence when any Harold monitor component is
  degraded. The transport may still be live, and the last committed rows remain
  visible.
- `MONITOR UNKNOWN` appears when there are no monitor observations, or when any
  component is unknown and none is degraded.
- `MONITOR HEALTHY` requires one or more monitor observations with every
  component healthy.

Rows are grouped by project (the first word of the tmux session name) and
ordered by name (the last component of the working directory), then pane
address; state never changes a row's position. Each row shows the name (the
pane address is added only when two names in a group collide, and a long name is
truncated before that address, never the address itself; panes of different
sessions in one group are told apart by the session's second word, or the full
tmux target), and a provider tag: `cc` for Claude Code, `cx` for Codex, otherwise the
first two letters of the provider name. The selected agent's detail panel still
shows the full tmux target and working directory. Narrower terminals drop some
footer hints (below 100 columns the footer switches to a shorter set). Selection follows a stable agent
incarnation when possible. A restarted agent in the same pane is a new
incarnation and does not inherit the old selection.

### Waiting line

The top border of the agent list carries a waiting line: `WAITING n` followed by
the waiting agents, oldest first, with `+N more` when they do not all fit. Rows
of waiting agents are marked with `◆`.

- An agent joins when it goes from Busy to Idle while its pane is not the pane
  you are looking at.
- It leaves when you visit its pane, when it goes Busy again, when it exits, or
  when you press `Enter` on it. With focus tracking on, `Enter` does not clear
  it directly: it leaves once the jump makes its pane active, on the next poll.
  With tracking off, `Enter` clears it.
- Waiting state is per dash and kept in memory only. Agents that are already
  Idle when the dash starts do not join, and a second dash keeps its own list.
- `h`/`l` (or Left/Right) step the selection through the waiting agents and
  wrap around.

### Focus tracking

Once a second the dash runs `tmux list-clients` and decides which pane you can
see. Suspended clients are ignored; a focused client is preferred, otherwise the
most recently active client is used, and that client's active pane counts as
seen. Focus is sampled once a second, so leaving a pane less than about a second
before its agent finishes can keep that agent off the line. The waiting line shows
`(focus tracking off)` when the query fails, when no client is eligible, and for
the first second after start; while it is off, an agent leaves the list only on
`Enter` or when it goes Busy.

An absent or empty work summary is displayed exactly as:

```text
No work summary reported
```

The dashboard consumes only Harold's classified pane state and bounded work
summary. Raw pane screen content and evidence provenance do not reach or appear
in the dashboard.

## Troubleshooting

### Harold is unavailable

The dashboard keeps retrying with a capped backoff and shows the selected
endpoint plus a sanitized error. Press `r` to retry immediately. Before the
first snapshot, no rows are shown; after a disconnect, the last accepted rows
remain visible as stale.

### Navigation is unavailable

The dashboard requires non-empty `TMUX` and `TMUX_PANE` values from its invoking
process before it discovers a client. Run it from the tmux client you intend to
navigate. An outside-tmux dashboard remains usable for viewing and searching but
will show `NAVIGATION UNAVAILABLE`.

### Pane switching fails

If the selected pane disappears or tmux rejects the switch, the dashboard keeps
running, preserves its rows and selection, and shows a sanitized
`NAVIGATION FAILED` status. Select another live row or wait for the next Harold
snapshot.

### The layout is replaced by a resize message

Resize the terminal to at least 60 columns by 18 rows. Wider and taller terminals
add the selected agent's complete current-work detail when it fits without
clipping.

### Terminal shutdown

Use `q` outside search editing or the palette. `Esc` never quits: while editing it always clears
the query and leaves editing, and outside editing it clears an accepted filter or
does nothing when no filter exists. `SIGINT` and `SIGTERM` also initiate shutdown.
The runtime restores raw mode, the primary screen, and
cursor visibility before returning; a restoration failure is reported on
standard error.

## Current integration boundary

This repository includes deterministic renderer, runtime, terminal-lifecycle,
API-mapping, and tmux-command tests. Live isolated Harold testing has verified
real Codex and OpenCode inventory, busy-to-idle lifecycle reporting, concurrent
current work summaries, summary isolation, and removal of persisted Codex input
placeholders by the final reviewed Harold implementation across an exact-fixture,
same-store restart/replay. Live Claude Code lifecycle coverage is not yet
claimed because its isolated test process requires interactive login, and the
three-provider concurrent gate therefore remains open. These claims come from
real provider processes and an isolated durable event store, not fixture data.
The exact evidence and limitations are in the
[screen-testing ledger](../tasks/tmux-agent-dashboard/screen-testing.md).
