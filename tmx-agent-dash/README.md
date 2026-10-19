# Tmux Agent Dash

`tmx-agent-dash` is a keyboard-driven terminal view of the agent-pane projection
published by Harold. It shows transport and monitor health separately, keeps the
last committed rows visible through degraded or stale periods, and can switch
the invoking tmux client to the selected pane.

## Prerequisites

- Rust 1.97.1 (the currently verified toolchain)
- tmux 3.6a (the currently verified version) for pane navigation
- Harold with the `WatchAgentStates` gRPC service available
- The containing Harold workspace, with the shared API crate at `../harold-api`

Viewing works outside tmux, but pane navigation is disabled. Start the dash
inside the tmux client that should move when you press `Enter`. When several
clients are attached to that session, tmux moves the most recently active one.

From the containing Harold workspace root, build every release binary:

```sh
make build
```

Install the same workspace revision, including the signed on-demand dash
command at `~/bin/tmx-agent-dash`:

```sh
make deploy
```

For standalone dash development from this package directory, use Cargo:

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

The cursor moves over every row of the agent list, and the chief's questions are
rows of that list (see [Questions](#questions)). There are no sections to move
between. Search, the command palette, a free-text field and a note are text modes.
One table gives every key by the row under the cursor. Where a cell says "Nothing",
the key does nothing there, so a key never means two things.

| Key | Agent row | Question header | Option row | Action row | Text field, search, palette, note |
| --- | --- | --- | --- | --- | --- |
| `j` / `k`, ↓ / ↑ | Next or previous row, question rows included | Same | Same | Same | ↓ / ↑ move in the palette; `j` and `k` type |
| `l` / → | Nothing | Open the question | Nothing | Nothing | Types `l` |
| `h` / ← | Nothing | Close the question, when open | Close the question, cursor to its header | Same | Types `h` |
| Space | Nothing | Nothing | Pick the option; a free option opens its field | Nothing | Types a space |
| Enter | Switch the invoking tmux client to the selected pane | Open or close the question | Nothing | `send` sends the answer at once; `later` sets the question aside; `revise…` or `reject…` opens a note | Search: keep the query · palette: run · field: keep the text and pick the option · note: send it |
| Esc | Clear an accepted filter, or nothing | Close the question, when open | Close the question, cursor to its header | Same | Search: clear the query and leave · palette: close · field or note: discard and leave |
| `n` / `N` | Next or previous waiting agent (wraps) | Next or previous waiting question (wraps) | Same | Same | Types |
| `o` | Nothing | Open the question's link, if it is `http://` or `https://` | Same | Same | Types |
| `g` / `G` | First / last row | Same | Same | Same | Types |
| `c` / `t`, `f`, `/`, `q` | `c` / `t`: call / talk now, on the chief's row · `f`: search · `/`: command palette · `q`: quit | Nothing | Nothing | Nothing | Types |
| Ctrl+L, Ctrl+Q, Ctrl+C | Retry the Harold connection now (clearing and redrawing the screen) · quit · quit | Same | Same | Same | Same: the text and the query are kept, and nothing is typed |

- **Quit keys:** Ctrl+Q and Ctrl+C quit from every mode, including a note and the
  held or swallowed keys described under [Reconnects](#reconnects). `q` quits only
  from an agent row; on a question row it does nothing, and in a text mode it types
  itself.
- **Chords:** Ctrl+Q, Ctrl+C and Ctrl+L are the only Control or Alt chords that act.
  Every other chord, such as Ctrl+R or Alt+Q, does nothing in any mode, and is never
  typed or taken as the bare letter.
- **Text modes:** in search, the palette query, a free-text field and a note, every
  printable key, `h`, `j`, `k`, `l`, `n`, `N`, `r`, `x`, `o` and `q` included, types
  itself, and `Backspace` removes the last Unicode scalar value. Outside the text
  modes, a key not in the table, such as `r`, `x`, `s` or the Tab key, does nothing.
- **`c` and `t`** act only while the chief's row is selected. `c` starts a call when
  none is on and the voice is not muted, and ends one during a call. `t` asks to talk
  now while the call is speaking.
- **The palette** filters its commands as you type; ↑/↓ choose and Enter runs one,
  such as `Voice: mute` / `Voice: unmute` or `Messaging: pause` / `Messaging: resume`.
- **The footer** gives the keys for the row under the cursor. On an agent row it lists
  the list's keys (`j/k`, `n/N`, `f`, `/`, `Enter`, `c`, `t`, `Esc`, `^L`, `q`). On a
  question row, under a `QUESTION` label, it lists `j/k` move, `n/N` question, `o`
  open link, `^L` retry and `^Q` quit, and `Esc` close on every row except a closed
  header, where `Esc` does nothing. Two keys depend on the row: `l` and `Enter` open
  a closed header, `h` and `Enter` close an open header, `Space` picks and `h`
  closes on an option, `Enter` chooses and `h` closes on an action. A narrow terminal
  shortens the hints (`link` for `open link`, no label), and below that puts the rest
  on a second line. While a note or an option's text field is open, every key types
  into it, so the footer lists only `Enter` (`send` for a note, `keep` for a text
  field), `Esc` discard, `^L` and `^Q`.

Search is a local, case-insensitive filter over provider, work summary, tmux
target, and working directory. It never sends a search request to Harold and it
does not pause incoming snapshots. The search line reports visible and total
row counts while a filter is active. Press `f` to start editing the search query.

The masthead shows the confirmed voice state as `● VOICE ON` or `○ VOICE MUTED`,
taken from the Harold snapshot rather than from local guesses. Voice commands are
offered in the palette only while the connection is LIVE and a snapshot has
arrived; otherwise the palette shows `Commands unavailable until connected`.
Before the first snapshot the masthead shows `○ VOICE —`.

`Messaging: pause` is the messaging pause for Harold's away channel: it stops all
iMessage or Telegram traffic, in both directions, without stopping Harold.
`Messaging: resume` turns it back on. Both are offered under the same conditions
as the voice commands. While Harold reports messaging as paused, the masthead of
every connected dash shows `■ MSG PAUSED` after the voice state; nothing is shown
while messaging is running. The masthead title always shows the dash's own release version (`source` for a
source build), whatever Harold reports and even before the first snapshot or after a
disconnect. A dash left running through an install shows its old version until it is
restarted. As the line gets tight `HAROLD / TMUX` shortens to `TMX DASH`, then `TMX DASH`
goes, then `REV`, then the version, so transport, voice and the paused marker stay
visible. A failed request is reported as `MESSAGING ERROR: …`. See Harold's
[operation reference](../docs/references/operation/README.md#pausing-messaging)
for what the switch does.

## Now line

When the chief of pods has a Now item in its queue, the dash shows it on one dim line under the masthead: `NOW  ` and the item's first line, cut to the terminal's width. The dash reads it from Harold's backlog tool with the question queue, every 2 seconds; when that poll's question read timed out, the Now read is skipped and the line hides until the next poll. The line takes a row only when one is spare beside the agent list's six rows. It is hidden while the queue has no Now item, when Harold does not serve the backlog tool, the read fails or the stream to Harold is lost, and at the smallest height (18 rows), where the agent list keeps its six rows. It is never focused and takes no keys. See the [backlog reference](../docs/references/tools/backlog.md) for how the queue is kept.

## Questions

When the chief of pods has questions for you, they appear as rows of the agent list, right under the chief's row. The dash reads the questions from Harold's question tool every 2 seconds, and before each key that lands on a question row. No row shows while no question waits. A new question never moves the cursor. See the [question tool reference](../docs/references/tools/questions.md) for how questions get in and what Harold does with the answers.

### Closed and open

A closed question takes one row. With two questions waiting, the list reads:

```text
  chief-of-pods
▶ ○ IDLE   chief-of-pods          ◆  Asked T1 about the wire protocol
    ▸ 2 questions waiting
  harold
  ● BUSY   harold-main               Building dash-one-version
```

Press `j` to move the cursor to the `▸ 2 questions waiting` row, then `l` or `Enter` to open it. The row becomes the question's header, `▾ T1 · 1 of 2 waiting · harold · durable-server`, and the question follows it. Every decision shows at once, each with its ask and its options, and long text wraps within the list's width:

```text
    ▾ T1 · 1 of 2 waiting · harold · durable-server
        The server needs one wire protocol for its clients.

        Which protocol should clients speak?
          ● a  libSQL's Hrana  (suggested)
               Existing libSQL clients work unchanged.
          ○ b  Postgres wire
               Any Postgres client connects.
          ○ z  Something else

        Should the build run on every push?
          ● y  Yes  (suggested)
          ○ n  No

          send
          ─────────────
          later
          revise…
          reject…
```

The cursor stops on the header, on each option and on each action (`send`, `later`, `revise…`, `reject…`). It skips the intro, the asks, the impact lines and the blank rows. `j` and `k` move through all of them like any other rows: `j` from the chief's row goes to the header, then over each option and action, then on to the next agent row. `h` (or `Esc`) closes the question again with the cursor on its header; the picks are kept, and `l` on the header opens it as it was. The yellow marks the header and the row under the cursor.

With several questions waiting, the open question shows one at a time, and `n` and `N` step between them. The detail panel, on a wide terminal, shows the asking agent while the cursor is on its question rows.

### Picking and sending

Each decision opens with its suggested option picked (`●`); an option you have not picked shows `○`, and a decision with no suggested option, or whose suggested option is a free one, starts with no pick (a free suggested option still shows `(suggested)`, with `○`). To pick another option, move to it with `j` or `k` and press `Space`: the `●` moves to it. Moving never changes a pick. You can change any pick until the answer is sent, so this is also how you undo one.

An option's impact shows on the line below its label, dim; an option with no impact has no second line. A free option such as `z  Something else` takes text of your own: `Space` on it opens a text field, holding any text typed there before. Type, then press `Enter` to keep the text and pick the option (blank text is not accepted), or `Esc` to discard it and leave the field. The option's row then shows the text after its label, cut to fit. Text stops at 1,999 characters, since Harold refuses 2,000.

To answer, move to `send` and press `Enter`. The dash re-reads the questions first. The answer goes to Harold with every decision's pick, and Harold types it into the chief's pane at once. If a decision has no pick, nothing is sent and the cursor moves to that decision's first option. After a send, the next waiting question shows in its place, open, with the cursor on its header; with none left, the question rows go and the cursor moves to the chief's row.

The other actions:

- **later** sets the question aside, keeping its picks, and shows the next waiting one.
- **revise…** opens a note saying what to change, and **reject…** a note saying why. `Enter` on a note sends it as a revise or reject answer, with the picks made so far and at once like `send`; a blank note is not sent. `Esc` discards the note.

`n` and `N` move to the next or previous waiting question, wrapping over the listed order, set-aside questions included. With one question listed they do nothing. The picks of the question you leave are kept while the dash runs, as are the picks of a question set aside with **later** and of a closed question. Kept picks belong to the version of the question they were made on: when Harold lists a higher version, or refuses an answer because the version changed, they are dropped. Kept picks of a question that is answered or dismissed are gone.

Questions set aside come back after a line is typed with no question waiting, or when a **later** leaves none waiting and no answer unsent.

### When the chief's row is hidden or scrolled off

- **Filtered out:** when the search filter hides the chief's row, the question rows stay, at the top of the list, and the closed row reads `▸ 2 questions waiting · from the chief`. The filter hides agent rows, never question rows.
- **No chief's row:** when Harold names no chief pane, or that pane is not in the list, the question rows show at the top of the list the same way.
- **Scrolled off:** the question rows scroll with the list like any rows. When the header row is out of view, the list's top or bottom edge shows `▲ 2 questions waiting` or `▼ 2 questions waiting` in yellow, on the side where it is.
- **The smallest height** (18 rows, where the list has six): an open question scrolls within those rows and keeps the cursor in view, as the list does.

### Notices

A notice shows under the question's rows until the next key:

- `<id> changed, your picks were cleared`: the chief put the question again. Your picks are dropped.
- `<id> dismissed: <reason>`: the chief withdrew the question, with its reason (`withdrawn` when none is known). The rows move to the next question.
- `Harold can't save answers right now`: Harold could not store the answer or set the question aside. Your picks stay; try again.

When no question is left, a notice stands alone as one row. It goes at the next key, and otherwise after the next read that still finds no question, a few seconds on. Then the rows go. That key also does what it does on the list. A key that would still reach a question that just changed is dropped, since it was typed for what was there before, so `Enter` on `send` never sends picks made on an older version.

### Reconnects

If the questions cannot be read, or the stream to Harold is lost, the question rows hide but keep your picks and any half-typed note. They come back when the next read succeeds and the question is unchanged. If the cursor was on a question row, keys are ignored while the rows are away, and the footer reads `Esc back to the list (the questions are away)`; `Esc` returns the keys to the list, where the cursor already is. If the rows come back first, the cursor returns on its own to the row the open note belongs to (the `revise…` or `reject…` row, or the option with the text field) and typing resumes, with no `Esc` needed when the question is unchanged. If you pressed `Esc` to the list first and the questions then come back unchanged with a note still open, the cursor returns to the note's row all the same, and the first key after that is dropped. A note stays visible under its own row wherever the cursor is.

If the question changes under a half-typed note or text field, the rows keep showing the new question and hold their keys until you press `Esc`. The footer reads `esc to continue`. This stops letters typed for the old question from acting on the new one. `Esc` clears the notice and leaves the cursor where it is when that row still exists in the new question (a changed question keeps its open state, so the cursor stays on the `revise…` or `reject…` row or the option where you were typing); it moves to the header only when that row is gone.

While keys are ignored or held, the quit keys still quit and Ctrl+L still retries. Ctrl+L leaves a half-typed note as it was. A hold ends with the reconnect: the rows hide and their keys are ignored until the next read shows them again. Then keys act on the question shown, and if that question changed while a note or text field was open, its notice shows and the keys are held again until `Esc`.

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
id; state never changes a row's position. Each row shows the name (the
pane id is added only when two names in a group collide, and a long name is
truncated before that id, never the id itself; panes of different
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

The dash consumes only Harold's classified pane state and bounded work
summary. Raw pane screen content and evidence provenance do not reach or appear
in the dash.

## Troubleshooting

### Harold is unavailable

The dash keeps retrying with a capped backoff and shows the selected
endpoint plus a sanitized error. Press Ctrl+L to retry immediately; the message reads `Press Ctrl+L to retry now`. Before the
first snapshot, no rows are shown; after a disconnect, the last accepted rows
remain visible as stale.

### Navigation is unavailable

The dash requires non-empty `TMUX` and `TMUX_PANE` values from its invoking
process before it discovers a client. Run it from the tmux client you intend to
navigate. An outside-tmux dash remains usable for viewing and searching but
will show `NAVIGATION UNAVAILABLE`.

### Pane switching fails

If the selected pane disappears or tmux rejects the switch, the dash keeps
running, preserves its rows and selection, and shows a sanitized
`NAVIGATION FAILED` status. Select another live row or wait for the next Harold
snapshot.

### The layout is replaced by a resize message

Resize the terminal to at least 60 columns by 18 rows. Wider and taller terminals
add the selected agent's complete current-work detail when it fits without
clipping.

### Terminal shutdown

Ctrl+Q and Ctrl+C quit from every mode. `q` quits from the agent list; in search,
the palette, a free-text field and a note it types itself, and on a question row it does nothing.
`Esc` never quits: while editing it always clears the query and leaves editing, and
in the agent list it clears an accepted filter or does nothing when no filter exists. `SIGINT` and `SIGTERM` also initiate shutdown.
The runtime restores raw mode, the primary screen, and
cursor visibility before returning; a restoration failure is reported on
standard error.

## Current integration boundary

This repository includes deterministic renderer, runtime, terminal-lifecycle,
API-mapping, and tmux-command tests. Live isolated Harold testing has verified
real Codex and OpenCode inventory, busy-to-idle lifecycle reporting, concurrent
work summaries, summary isolation, and removal of persisted Codex input
placeholders by the final reviewed Harold implementation across an exact-fixture,
same-store restart/replay. Live Claude Code lifecycle coverage is not yet
claimed because its isolated test process requires interactive login, and the
three-provider concurrent gate therefore remains open. These claims come from
real provider processes and an isolated durable event store, not fixture data.
The exact evidence and limitations are in the
[screen-testing ledger](../tasks/tmux-agent-dashboard/screen-testing.md).
