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

The dash has three sections: the [question block](#question-block), its summary,
and the agent list. Search, the command palette and the block's notes are text
modes. One table gives every key. Where a cell is "—", the key does nothing in that
section, so a key never means two things.

| Key | Question block (choosing) | Summary | Agent list | Search, palette, note |
| --- | --- | --- | --- | --- |
| `j` / ↓ | Next row: the real options, then the answer actions | — | Next visible row | ↓ moves in the palette; `j` types |
| `k` / ↑ | Previous row | — | Previous visible row | ↑ moves in the palette; `k` types |
| `l` / → | Pick the highlighted option and go to the next decision; nothing on an answer action | — | — | Types `l` |
| `h` / ← | Previous decision, picks kept | Back to the last decision | — | Types `h` |
| `n` / `N` | Next or previous waiting question (wraps) | Same | Next or previous waiting agent (wraps) | Types |
| Enter | Pick the highlighted row: an option moves on, an answer action acts | Record the answer | Switch the invoking tmux client to the selected pane | Search: keep the query · palette: run · note: done |
| Esc | Leave the block, picks kept | Back to the last decision | Clear an accepted filter, or nothing | Search: clear the query and leave · palette: close · note: discard and leave |
| Tab | Focus to the list | Focus to the list | Focus to the block, while it shows | Search and palette: nothing · note: focus to the list, note kept |
| `s` | — | Mark or unmark the answer "send now" | — | Types |
| `o` | Open the question's link, if it is `http://` or `https://` | Same | — | Types |
| Ctrl+L | Clear the screen, redraw, and retry the Harold connection now | Same | Same | Same: the note's text and the query are kept, and nothing is typed |
| `f` / `/` | — | — | Search editing / command palette | Types |
| `g` / `G` | — | — | First / last visible row | Types |
| `c` / `t` | — | — | Call / talk now, on the chief's row | Types |
| `q` | — | — | Quit | Types |
| Ctrl+Q, Ctrl+C | Quit | Quit | Quit | Quit |

- **Quit keys:** Ctrl+Q and Ctrl+C quit from every mode, including a note and the
  held or swallowed keys described under [Reconnects](#reconnects). `q` quits only
  from the agent list; anywhere else it does nothing or types itself.
- **Chords:** Ctrl+Q, Ctrl+C and Ctrl+L are the only Control or Alt chords that act.
  Every other chord, such as Ctrl+R or Alt+Q, does nothing in any mode, and is never
  typed or taken as the bare letter.
- **Text modes:** in search, the palette query and a note, every printable key,
  `h`, `j`, `k`, `l`, `n`, `N`, `r`, `x`, `o` and `q` included, types itself, and
  `Backspace` removes the last Unicode scalar value. Outside the text modes, a key
  not in the table, such as `r` or `x`, does nothing.
- **`c` and `t`** act only while the chief's row is selected. `c` starts a call when
  none is on and the voice is not muted, and ends one during a call. `t` asks to talk
  now while the call is speaking.
- **The palette** filters its commands as you type; ↑/↓ choose and Enter runs one,
  such as `Voice: mute` / `Voice: unmute` or `Messaging: pause` / `Messaging: resume`.

Search is a local, case-insensitive filter over provider, work summary, tmux
target, and working directory. It never sends a search request to Harold and it
does not pause incoming snapshots. The search line reports visible and total
row counts while a filter is active. Press `f` to start editing the search query.

The masthead shows the confirmed voice state as `● VOICE ON` or `○ VOICE MUTED`,
taken from the Harold snapshot rather than from local guesses. Voice commands are
offered in the palette only while the connection is LIVE and a snapshot has
arrived; otherwise the palette shows `Commands unavailable until connected`.
Before the first snapshot the masthead shows `○ VOICE —`.

`Messaging: pause` is the kill switch for Harold's away channel: it stops all
iMessage or Telegram traffic, in both directions, without stopping Harold.
`Messaging: resume` turns it back on. Both are offered under the same conditions
as the voice commands. While Harold reports messaging as paused, the masthead of
every connected dash shows `■ MSG PAUSED` after the voice state; nothing is shown
while messaging is running. The masthead title shows Harold's release version
(`source` for a source build; nothing before the first snapshot, after a
disconnect, or from an older Harold that sends no version); `(DASH v…)` follows
when the dash was built from another release. As the line gets tight the `DASH`
part goes first, then `HAROLD / TMUX` shortens to `TMX DASH`, then `TMX DASH`
goes, then `REV`, then the version, so transport, voice and the paused marker
stay visible. A failed request is reported as `MESSAGING ERROR: …`. See Harold's
[operation reference](../docs/references/operation/README.md#pausing-messaging)
for what the switch does.

## Now line

When the chief of pods has a Now item in its queue, the dash shows it on one dim line under the masthead: `NOW  ` and the item's first line, cut to the terminal's width. The dash reads it from Harold's backlog tool with the question queue, every 2 seconds; when that poll's question read timed out, the Now read is skipped and the line hides until the next poll. The line is hidden while the queue has no Now item, when Harold does not serve the backlog tool, the read fails or the stream to Harold is lost, and at the smallest height (18 rows), where the agent list keeps its six rows. It is never focused and takes no keys; the question block sits below it with the same focus and keys. See the [backlog reference](../docs/references/tools/backlog.md) for how the queue is kept.

## Question block

When the chief of pods has questions for you, the dash shows a yellow block above the agent list. It reads the queue from Harold's question tool every 2 seconds, and before each key that goes to the block. The block is hidden while no question waits. A new question never takes the focus from the agent list. See the [question queue reference](../docs/references/tools/questions.md) for how questions get in and what Harold does with the answers.

The block shows the product and spec, the intro, the current decision with its options, and a key hint. Its title reads `<id> · n of m waiting`. The option the chief suggests is marked `(suggested)` and the highlight starts on it.

### Focus and keys

`Tab` moves the focus between the agent list and the block while the block shows. The block has a bright border with the focus and a dim one without. Footer hints show `tab question` while the block shows, and the footer gives the keys of whichever section has the focus. Control and Alt chords never reach the block. The [Keys](#keys) table gives every key in the block and its summary.

Each decision lists the question's own options, then a thin rule and three dim answer actions, which have no option key:

```text
  a  libSQL's Hrana — Existing libSQL clients work unchanged.  (suggested)
  b  Postgres wire — …
  z  Something else — Type your answer.
  ─────────────
     later
     revise…
     reject…
```

- `j`/`k` move the highlight over the options and then the actions. The highlight starts on the suggested option, never on an action, and each decision keeps its own highlight.
- `Enter` on an option picks it and goes to the next decision; after the last decision the summary shows. A free option opens a text field first; when it is already the decision's pick, the field holds the text typed before.
- `l` picks the highlighted option and moves on as `Enter` does. On a free option that is already the pick, it moves on and keeps the text; on a free option that is not the pick, it opens the text field. On an answer action it does nothing.
- `h` goes back a decision. Every pick is kept, and `Enter` or `l` on a different option replaces that decision's pick.
- **later** sets the question aside and shows the next waiting one. **revise…** opens a note saying what to change, and **reject…** a note saying why. `Enter` on a note records it as a revise or reject answer with the picks made so far; a blank note is not recorded.
- `Esc` gives the focus back to the list from any decision, and the picks stay.

The summary lists each decision with its pick, then a `send now` row that reads `no    (s to mark)`. `s` toggles it to `yes   (s to unmark)`, and the hint line then says `s don't send now`. A marked answer goes to the chief at once; an unmarked one goes about 3 seconds after your last answer. The mark belongs to the one answer on the summary and starts at `no` on every summary; going back with `h` or `Esc` and returning keeps the mark you set until the answer is recorded. A revise or reject note has no mark, since every letter types there. `Enter` records the answer with its mark, and `h` or `Esc` goes back to the last decision. In a text field, type the text, `Backspace` removes the last character, `Enter` accepts it (blank text is not accepted) and `Esc` discards it and leaves the field. Text stops at 1,999 characters, since Harold refuses 2,000.

A free option does not end the walk. The text you type becomes that decision's pick, and you go on to the next decision. The answer is recorded as free text when any pick is free.

`n` and `N` move to the next or previous waiting question, wrapping over the listed order, set-aside questions included. With one question listed they do nothing. The picks, decision and highlights of the question you leave are kept while the dash runs, and come back when you return to it, as do the picks of a question set aside with **later**. Kept picks belong to the version of the question they were made on: when Harold lists a higher version, or refuses an answer because the version changed, they are dropped, and the question starts at its first decision. Kept picks of a question that is answered or dismissed are gone.

Harold types a line of answers into the chief's pane about 3 seconds after the newest answer, or at once when an answer is marked send now. Questions still waiting do not hold it back. Deferred questions come back to the queue after a line is typed with no question waiting, or when a **later** leaves none waiting and no answer unsent.

### Notices

A notice shows under the block until the next key:

- `<id> changed, your picks were cleared`: the chief put the question again. Your picks are dropped and you start at its first decision.
- `<id> dismissed: <reason>`: the chief withdrew the question, with its reason (`withdrawn` when none is known). The block moves to the next question.
- `Harold can't save answers right now`: Harold could not store the answer or the deferral. Your picks stay; try again.

When no question is left, a notice stands alone in the block. It goes at the next key, wherever the focus is, and otherwise after the next read that still finds no question, a few seconds on. Then the block hides. With the focus on the list, that key also does what it does on the list. With the focus on the block, the key only clears the notice, and the focus returns to the list. If the read hides the block while it has the focus, the focus returns to the list, and the next key acts on the list. A key that would still reach a block whose question just changed is dropped, since it was typed for what was there before.

### Reconnects

If the queue cannot be read, or the stream to Harold is lost, the block hides but keeps your picks and any half-typed note. They come back when the next read succeeds and the question is unchanged. If the block had the focus, keys are ignored until `Tab` or `Esc` returns them to the list. The footer says so.

If the question changes under a half-typed note or the summary, the block keeps showing the new question and holds its keys until you press `Esc` (or `Tab`). The footer says `esc to continue`. This stops letters typed for the old question from acting on the new one. `Esc` clears the notice and starts the shown question from its first decision.

While keys are ignored or held, the quit keys still quit and Ctrl+L still retries. Ctrl+L leaves a half-typed note as it was. Ignored keys stay ignored until the block comes back. A hold ends with the reconnect: the block hides and its keys are ignored until the next read shows it again. Then keys act on the question shown, and if that question changed meanwhile, the block shows its notice and holds its keys again until `Esc` or `Tab`.

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
footer hints (the long footer needs 127 columns and 24 rows; below that it switches to a shorter set, on two lines when one cannot hold it). Selection follows a stable agent
incarnation when possible. A restarted agent in the same pane is a new
incarnation and does not inherit the old selection.

### Waiting agents

Rows of waiting agents are marked with `◆`. `n` and `N` move the selection to the
next or previous waiting agent, oldest first, and wrap around. If a search filter
hides that agent, the filter is cleared.

- An agent starts waiting when it goes from Busy to Idle while its pane is not the
  pane you are looking at.
- It stops when you visit its pane, when it goes Busy again, when it exits, or
  when you press `Enter` on it. With focus tracking on, `Enter` does not clear
  it directly: it stops once the jump makes its pane active, on the next poll.
  With tracking off, `Enter` clears it.
- Waiting state is per dash and kept in memory only. Agents that are already
  Idle when the dash starts do not wait, and a second dash keeps its own list.

### Focus tracking

Once a second the dash runs `tmux list-clients` and decides which pane you can
see. Suspended clients are ignored; a focused client is preferred, otherwise the
most recently active client is used, and that client's active pane counts as
seen. Focus is sampled once a second, so leaving a pane less than about a second
before its agent finishes can keep that agent from waiting. The top border of the
agent list shows `(focus tracking off)` when the query fails, when no client is
eligible, and for the first second after start; while it is off, an agent stops
waiting only on `Enter` or when it goes Busy.

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
endpoint plus a sanitized error. Press Ctrl+L to retry immediately; the message reads `Press Ctrl+L to retry now`. Before the
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

Ctrl+Q and Ctrl+C quit from every mode. `q` quits from the agent list; in search,
the palette and a note it types itself, and in the question block it does nothing.
`Esc` never quits: while editing it always clears the query and leaves editing, and
in the agent list it clears an accepted filter or does nothing when no filter exists. `SIGINT` and `SIGTERM` also initiate shutdown.
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
