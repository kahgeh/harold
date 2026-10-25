# Tmux Agent Dash

`tmx-agent-dash` is a keyboard-driven terminal view of the agent-pane projection
published by Harold. It shows transport and agent monitor health separately, keeps the
last committed rows visible through degraded or stale periods, and can switch
the invoking tmux client to the pane under the cursor.

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

The cursor moves over every row of the agent list, and the chief's todos (its
questions and instructions) are rows of that list (see [Todos](#todos)). There are
no sections to move between. Search, the command palette, a free-text field and a
note are text modes. One table gives every key by the row under the cursor. Where a
cell says "Nothing", the key does nothing there, so a key never means two things.

| Key | Agent row | Todo header | Option row | Action row | Step row | Text field, search, palette, note |
| --- | --- | --- | --- | --- | --- | --- |
| `j` / `k`, ↓ / ↑ | Next or previous row, todo rows included | Same | Same | Same | Same | ↓ / ↑ move in the palette; `j` and `k` type |
| `l` / → | Nothing | Open the todo (on another todo's row, open that todo) | Nothing | Nothing | Nothing | Types `l` |
| `h` / ← | Nothing | Close the todo, when open | Close the todo, cursor to its header | Same | Same | Types `h` |
| Space | Nothing | Nothing | Pick the option; a free option opens its field | Nothing | Tick the step, or untick it | Types a space |
| Enter | Switch the invoking tmux client to the pane under the cursor | Open or close the todo | Nothing | `send` sends the answer at once; `later` sets the question aside; `revise…` or `reject…` opens a note; `done` tells the chief you are done; `ignore` tells it you will not | Nothing | Search: keep the query · palette: run · field: keep the text and pick the option · note: send it |
| Esc | Clear an accepted filter, or nothing | Close the todo, when open | Close the todo, cursor to its header | Same | Same | Search: clear the query and leave · palette: close · field or note: discard and leave |
| `n` / `N` | Next or previous waiting agent (wraps) | Next or previous waiting todo (wraps) | Same | Same | Same | Types |
| `o` | Nothing | Open the todo's link, if it is `http://` or `https://` | Same | Same | Same | Types |
| `gg` / `G` | First / last row | First / last todo | Same | Same | Same | Types |
| `c` / `t`, `f`, `/`, `s`, `q` | `c` / `t`: call / talk now, on the chief's row · `f`: search · `/`: command palette · `s`: notes search (while connected, when Harold serves the knowledge tool) · `q`: quit | Nothing | Nothing | Nothing | Nothing | Types |
| Ctrl+L, Ctrl+Q, Ctrl+C | Retry the Harold connection now (clearing and redrawing the screen) · quit · quit | Same | Same | Same | Same | Same: the text and the query are kept, and nothing is typed |

- **Quit keys:** Ctrl+Q and Ctrl+C quit from every mode, including a note and the
  held or swallowed keys described under [Reconnects](#reconnects). `q` quits only
  from an agent row; on a todo row it does nothing, and in a text mode it types
  itself.
- **Chords:** Ctrl+Q, Ctrl+C and Ctrl+L are the only Control or Alt chords that act.
  Every other chord, such as Ctrl+R or Alt+Q, does nothing in any mode, and is never
  typed or taken as the bare letter.
- **Text modes:** in search, the palette query, a free-text field and a note, every
  printable key, `h`, `j`, `k`, `l`, `n`, `N`, `r`, `x`, `o` and `q` included, types
  itself, and `Backspace` removes the last Unicode scalar value. Outside the text
  modes, a key not in the table, such as `r`, `x`, `s` or the Tab key, does nothing.
- **`c` and `t`** act only while the cursor is on the chief's row. `c` starts a call when
  none is on and the voice is not muted, and ends one during a call. `t` asks to talk
  now while the call is speaking.
- **The palette** filters its commands as you type; ↑/↓ choose and Enter runs one,
  such as `Voice: mute` / `Voice: unmute` or `Messaging: pause` / `Messaging: resume`.
- **The footer** gives the keys for the row under the cursor. On an agent row it lists
  the list's keys (`j/k`, `n/N`, `f`, `/`, `Enter`, `c`, `t`, `Esc`, `^L`, `q`). On a
  todo row, under a `TODO` label, it lists the row's own keys first, then `j/k` move,
  `n/N` todo, `o` link (only when the todo has a web link), `Esc` close, `^L` retry and
  `^Q` quit. The first keys depend on the row: `l` open on a closed todo and on another
  todo's row, `h` close on an open header, `Space` tick and `h` close on a step,
  `Space` pick and `h` close on an option, `Enter` choose and `h` close on a question's
  action row, `Enter` done and `Enter` ignore on the instructions' `done` and `ignore`.
  A closed todo and another todo's row list neither `o` nor `Esc`. A narrow terminal
  puts the rest on a second line. While a note or an option's text field is open, every
  key types into it, so the footer lists only `Enter` (`send` for a note, `keep` for a
  text field), `Esc` discard, `^L` and `^Q`.

Search is a local, case-insensitive filter over provider, work summary, `session:window.pane`
name, and working directory. It never sends a search request to Harold and it
does not pause incoming snapshots. The search line reports visible and total
row counts while a filter is active. Press `f` to start editing the search query.

The masthead shows the confirmed voice state as `● VOICE ON` or `○ VOICE MUTED`,
taken from the Harold snapshot rather than from local guesses. Voice commands are
offered in the palette only while the connection is LIVE and a snapshot has
arrived; otherwise the palette shows `Commands unavailable until connected`.
Before the first snapshot the masthead shows `○ VOICE —`.

`Messaging: pause` is the messaging pause for Harold's away channel: it stops all
iMessage traffic, in both directions, without stopping Harold.
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

## Notes search

`s` opens the vault notes over the agent list, while the dash is connected to Harold. Vault notes are the organisation's Markdown notes, which Harold's [knowledge tool](../docs/references/tools/knowledge.md) indexes by meaning; they are not backlog notes. The dash reads them through the tool's gRPC service on Harold's own address. It needs `[knowledge] vault` set in Harold: the dash asks once when its stream opens, and when Harold does not serve the tool `s` does nothing.

- The box shows the 10 vault notes changed most recently, newest first, each with its kind and title.
- **Typing searches.** Any printable key starts a search, and the list becomes the closest 10 vault notes after you pause for 300 ms. A query under 2 characters shows the recent list again. The first search after a pause waits for the model to load, and the box reads `Looking…`; if the model could not load it reads `Search is resting: the model didn't load.`
- `j` / `k` (and ↓ / ↑) move the cursor, `gg` / `G` go to the first and last row, while the keys are for moving. To search for a word that begins with `j`, `k` or `g`, press `/` first: then every key types, and only the arrows move.
- **Enter** opens the vault note's file in `$EDITOR` (`vi` when it is unset), in a new tmux window named `note`. The dash stays where it is.
- **Esc** clears the query, then leaves the list and goes back to the agents. Ctrl+Q, Ctrl+C and Ctrl+L act as everywhere. `q` types a `q` here and does not quit.
- There is no note pane and no related list. When the connection to Harold ends, the list closes.

## Now line

When the chief of pods has a Now item in its queue, the dash shows it on one dim line under the masthead: `NOW  ` and the item's first line, cut to the terminal's width. The dash reads it from Harold's backlog tool with the question tool, every 2 seconds; when that poll's question read timed out, the Now read is skipped and the line hides until the next poll. The line takes a row only when one is spare beside the agent list's six rows. It is hidden while the queue has no Now item, when Harold does not serve the backlog tool, the read fails or the stream to Harold is lost, and at the smallest height (18 rows), where the agent list keeps its six rows. The cursor never lands on it and it takes no keys. See the [backlog reference](../docs/references/tools/backlog.md) for how the queue is kept.

## Todos

When the chief of pods has something for you, a todo, it appears as rows of the agent list, right under the chief's row. A todo is one of two kinds: a question to answer (id `Q1`) or instructions to follow by hand (id `I1`), and every header and row names its kind. The dash reads the todos from Harold's question tool every 2 seconds, and before each key that lands on a todo row. No row shows while no todo waits. A new todo never moves the cursor. See the [question tool reference](../docs/references/tools/questions.md) for how todos get in and what Harold does with the answers.

### Closed and open

Closed todos take one row, counting questions and instructions together. With an instructions todo and a question waiting, the list reads:

```text
  chief-of-pods
▶ ○ IDLE   chief-of-pods          ◆  Put I1 and Q1 for you
    ▸ 2 todos waiting
  harold
  ● BUSY   harold-main               Building dash-one-version
```

Press `j` to move the cursor to the `▸ 2 todos waiting` row, then `l` or `Enter` to open it. The first todo in the chief's order opens in full and the row becomes its header; every other todo stays one row under it. For instructions, the header is `▾ I1 · instructions · 1 of 2 todos · 0 of 4 ticked · harold · Install Harold 0.15.0 on studio-mac`, and the steps follow it, each with its detail dim underneath. `Space` ticks a step (`[✓]`) and unticks it again, in any order; ticking sends nothing and Harold keeps the ticks, so a restart or a second dash shows the same ones. `done` tells the chief you are done, `ignore` that you will not do them:

```text
    ▾ I1 · instructions · 1 of 2 todos · 2 of 4 ticked · harold · Install Harold 0.15.0 on studio-mac
        Do these on studio-mac, then tick each one here.

          [✓] 1  Open Terminal
          [✓] 2  Run make deploy in ~/code/harold/main
                 It asks for your password once.
          [ ] 3  Turn on harold in System Settings › Privacy & Security › Accessibility
                 harold shows in the list after its first start.
          [ ] 4  Run harold status and check it says running 0.15.0

          done
          ─────────────
          ignore
    ▸ Q1 · question · harold · durable-server
```

When every step is ticked, a row `✓ All 4 ticked. Press done to tell the chief.` points to `done`. `Enter` on `done` sends at once; with steps left unticked, the chief is told how many were ticked. `Enter` on `ignore` sends at once too, and the instructions leave your todos. When a todo is answered, the next one opens in its place with the cursor on its header.

A question's header is `▾ Q1 · question · 1 of 2 todos · harold · durable-server` (`1 of 1 todo` for one), and the question follows it. Every decision shows at once, each with its ask and its options, and long text wraps within the list's width:

```text
    ▾ Q1 · question · 1 of 2 todos · harold · durable-server
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
    ▸ I1 · instructions · 0 of 4 ticked · harold · Install Harold 0.15.0 on studio-mac
```

The cursor stops on the header, on each option or step, on each action row (`send`, `later`, `revise…`, `reject…`, or `done` and `ignore`) and on each other todo's row. It skips the intro, the asks, the details, the impact lines and the blank rows. `j` and `k` move through all of them like any other rows: `j` from the chief's row goes to the header, then over each option or step and action row and each other todo's row, then on to the next agent row. `l` or `Enter` on another todo's row opens that todo. `h` (or `Esc`) closes the todo again with the cursor on its header; the picks are kept, and `l` on the header opens it as it was. The yellow marks the header and the row under the cursor.

With several todos waiting, the open todo shows in full one at a time, and `n` and `N` step between them. The detail panel, on a wide terminal, shows the asking agent while the cursor is on its todo rows.

### Picking and sending

Each decision opens with its suggested option picked (`●`); an option you have not picked shows `○`, and a decision with no suggested option, or whose suggested option is a free one, starts with no pick (a free suggested option still shows `(suggested)`, with `○`). To pick another option, move to it with `j` or `k` and press `Space`: the `●` moves to it. Moving never changes a pick. You can change any pick until the answer is sent, so this is also how you undo one.

An option's impact shows on the line below its label, dim; an option with no impact has no second line. A free option such as `z  Something else` takes text of your own: `Space` on it opens a text field, holding any text typed there before. Type, then press `Enter` to keep the text and pick the option (blank text is not accepted), or `Esc` to discard it and leave the field. The option's row then shows the text after its label, cut to fit. Text stops at 1,999 characters, since Harold refuses 2,000.

To answer, move to `send` and press `Enter`. The dash re-reads the questions first. The answer goes to Harold with every decision's pick, and Harold types it into the chief's pane at once. If a decision has no pick, nothing is sent and the cursor moves to that decision's first option. After a send, the next waiting todo shows in its place, open, with the cursor on its header; with none left, the todo rows go and the cursor moves to the chief's row.

The other action rows:

- **later** sets the question aside, keeping its picks, and shows the next waiting one.
- **revise…** opens a note saying what to change, and **reject…** a note saying why. `Enter` on a note sends it as a revise or reject answer, with the picks made so far and at once like `send`; a blank note is not sent. `Esc` discards the note.

`n` and `N` move to the next or previous waiting question, wrapping over the listed order, set-aside questions included. With one question listed they do nothing. The picks of the question you leave stay, as do the picks of a question set aside with **later** and of a closed question. Every pick is saved in Harold as you make it (a `Space` on an option, or `Enter` on a free option's text; text half-typed is not saved until `Enter`), so a dash restart, a reconnect or a second dash shows the same saved picks. A question that opens at a question version the dash holds no picks for starts from its saved picks, and a decision with no saved pick starts from its suggested option. Saved picks belong to the question version they were made on, but the chief putting a question again keeps the picks on decisions whose ask and options did not change: the notice says so. The saved picks of a question that is answered or dismissed stay in Harold until the id is put again.

Questions set aside come back after a line is typed with no question waiting, when a **later** leaves none waiting and no answer unsent, or when the chief dismisses the last waiting question and no answer is unsent.

### When the chief's row is hidden or scrolled off

- **Filtered out:** when the search filter hides the chief's row, the todo rows stay, at the top of the list, and the closed row reads `▸ 2 todos waiting · from the chief`. The filter hides agent rows, never todo rows.
- **No chief's row:** when Harold names no chief pane, or that pane is not in the list, the todo rows show at the top of the list the same way.
- **Scrolled off:** the todo rows scroll with the list like any rows. When the header row is out of view, the list's top or bottom edge shows `▲ 2 todos waiting` or `▼ 2 todos waiting` in yellow, on the side where it is.
- **The smallest height** (18 rows, where the list has six): an open todo, a long step list included, scrolls within those rows and keeps the cursor in view, as the list does.

### Notices

A notice shows under the todo's rows until the next key:

- `<id> changed, picks kept for unchanged decisions`: the chief put the question again. A decision keeps its saved pick when its ask and options are unchanged.
- `<id> changed, your picks were cleared`: the chief put the question again and no saved pick fits. Your picks are dropped.
- `<id> changed, ticks kept for unchanged steps`: the chief put the instructions again. A step keeps its tick when its text is unchanged.
- `<id> dismissed: <reason>`: the chief withdrew the question, with its reason (`withdrawn` when none is known). The rows move to the next todo.
- `Harold can't save answers right now`: Harold could not store the answer or set the question aside. Your picks stay; try again.
- `Harold can't save your picks right now`: Harold could not store a pick. The pick stays on screen, and your next pick or `send` carries it.

When no question is left, a notice stands alone as one row. It goes at the next key, and otherwise after the next read that still finds no question, a few seconds on. Then the rows go. That key also does what it does on the list. A key that would still reach a question that just changed is dropped, since it was typed for what was there before, so `Enter` on `send` never sends picks made on an older question version.

### Reconnects

If the questions cannot be read, or the stream to Harold is lost, the todo rows hide but keep your picks and any half-typed note. They come back when the next read succeeds and the question is unchanged. If the cursor was on a todo row, keys are ignored while the rows are away, and the footer reads `Esc back to the list (the questions are away)`; `Esc` returns the keys to the list, where the cursor already is. If the rows come back first, the cursor returns on its own to the row the open note belongs to (the `revise…` or `reject…` row, or the option with the text field) and typing resumes, with no `Esc` needed when the question is unchanged. If you pressed `Esc` to the list first and the questions then come back unchanged with a note still open, the cursor returns to the note's row all the same, and the first key after that is dropped. A note stays visible under its own row wherever the cursor is.

If the question changes under a half-typed note or text field, the rows keep showing the new question and hold their keys until you press `Esc`. The footer reads `esc to continue`. This stops letters typed for the old question from acting on the new one. `Esc` clears the notice and leaves the cursor where it is when that row still exists in the new question (a changed question keeps its open state, so the cursor stays on the `revise…` or `reject…` row or the option where you were typing); it moves to the header only when that row is gone.

While keys are ignored or held, the quit keys still quit and Ctrl+L still retries. Ctrl+L leaves a half-typed note as it was. A hold ends with the reconnect: the rows hide and their keys are ignored until the next read shows them again. Then keys act on the question shown, and if that question changed while a note or text field was open, its notice shows and the keys are held again until `Esc`.

## Reading status

Each row has Harold's latest classified agent state:

- `BUSY` means Harold currently classifies the agent as working.
- `IDLE` means Harold currently classifies the agent as idle.
- `UNKNOWN` means Harold cannot yet classify the agent conclusively.

The summary strip under the masthead shows the `BUSY` and `IDLE` counts, `UNKNOWN n` only when some agent is Unknown, and `SERVER TIME` as the server's clock in the Mac's local time, `HH:MM:SS` (`--:--:--` before the first snapshot).

Transport and agent monitor health describe different system-level conditions:

- `LIVE` means the Harold stream delivered the current accepted snapshot.
- `CONNECTING` and `UNAVAILABLE` describe connection startup or retry.
- `STALE` means the stream disconnected after a snapshot; the last committed
  rows remain visible and are labelled stale.
- `MONITOR DEGRADED` takes precedence when any Harold agent monitor component is
  degraded. The transport may still be live, and the last committed rows remain
  visible.
- `MONITOR UNKNOWN` appears when there are no agent monitor observations, or when any
  component is unknown and none is degraded.
- `MONITOR HEALTHY` requires one or more agent monitor observations with every
  component healthy.

Rows are grouped by project (the first word of the tmux session name) and
ordered by name (the last component of the working directory), then window and pane
number; state never changes a row's position. Each row shows the name (the
window and pane number, such as `:1.2`, is added only when two names in a group collide, and a long name is
truncated before that number, never the number itself; panes of different
sessions in one group are told apart by the session's second word, or the full
`session:window.pane` name), and a provider tag: `cc` for Claude Code, `cx` for Codex, otherwise the
first two letters of the provider name. The detail panel for the agent under the cursor still
shows the full `session:window.pane` name and working directory. Narrower terminals drop some
footer hints (the long footer needs 127 columns and 24 rows; below that it switches to a shorter set, on two lines when one cannot hold it). The cursor follows a stable agent
incarnation when possible. A restarted agent in the same pane is a new
incarnation and does not inherit the old cursor.

### Waiting agents

Rows of waiting agents are marked with `◆`. `n` and `N` move the cursor to the
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

If the pane under the cursor disappears or tmux rejects the switch, the dash keeps
running, preserves its rows and cursor, and shows a sanitized
`NAVIGATION FAILED` status. Move the cursor to another live row or wait for the next Harold
snapshot.

### The layout is replaced by a resize message

Resize the terminal to at least 60 columns by 18 rows. Wider and taller terminals
add the complete work summary detail of the agent under the cursor when it fits without
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
[screen-testing ledger](https://github.com/kahgeh/harold/blob/a373e28/tasks/tmux-agent-dashboard/screen-testing.md),
as of that commit.
