# Timed prompts

A timed prompt is a text Harold types into an agent's pane at a set local time each day. Harold types it only when the agent is idle and its input box is empty. Otherwise it waits, and after a set time it gives up for that day.

## Problem

A scheduled job outside Harold can send a text to a pane at 07:50, but it cannot see the pane. It cannot tell whether the agent is in the middle of a turn, or whether you have half a sentence typed in the input box. A text sent then is added to the end of your sentence and submitted with it.

Harold already lists the tmux panes that run agents and knows for each whether its agent is busy or idle. A timed prompt uses that, and reads the pane's screen once more before typing.

## Configuration

Add one `[[timed_prompts]]` block to `local.toml` for each prompt. The list is empty by default.

```toml
[[timed_prompts]]
name = "digest"
at = "07:50"
folder = "/Users/you/Dev/chief-of-pods"
text = "digest"
# give_up_after_minutes = 120
```

| Key | Meaning | Valid values |
|---|---|---|
| `name` | Names the prompt in the log and in Harold's stored record | 1 to 64 characters from letters, digits, `-` and `_`; no two entries share a name |
| `at` | Time of day, in the Mac's local time, 24-hour | `HH:MM`, `00:00` to `23:59` |
| `folder` | The folder whose agent gets the text | An absolute path starting with `/`; one trailing `/` is ignored |
| `text` | What is typed, followed by Enter. Harold adds nothing to it; two of the [limits](#limits) say when tmux types less | 1 to 2,000 characters on one line, with no control characters |
| `give_up_after_minutes` | How long after `at` the prompt stays due | 1 to 1439; 120 when left out |

`folder` is compared as text with the working directory tmux reports for a pane. Symbolic links are not resolved, and a subfolder is another folder.

Harold refuses to start on an invalid entry, and `harold --check-config` fails. Each error names the entry by its position in the list and the key, for example:

```
timed_prompts[0].at must be a time of day "HH:MM" from 00:00 to 23:59, got "9am"
```

Harold reads the configuration only at startup. A changed list takes effect at the next restart. Renaming an entry makes it a new prompt with no record, so it can be typed again on the day it is renamed.

Changing an entry's `at` after Harold has a record for today takes effect the next day. The record is kept by name and date, so the prompt is not due again that day at its new time. For example, Harold started at 15:00 with `at = "09:00"` logs `timed prompt skipped` with `window_passed`. Changing `at` to `15:10` and restarting then types nothing that day.

## When a prompt is due

Every 5 seconds Harold reads the Mac's local date and time and goes through the list of prompts once. This page calls one such pass a round. A prompt is due when all three hold:

- the local time is at or after its `at`;
- the local time is before the end of its window, which is `at` plus `give_up_after_minutes`, or midnight if that comes first;
- Harold has no record of handling it on this local date.

Harold compares the clock with `at` in each round and counts nothing, so sleep and a clock change need no special handling. A Mac that wakes at 09:40 finds a 09:00 prompt still due. A Mac that wakes at 11:30 finds its window ended and skips it for the day.

After a restart a prompt is still due by the same three rules, and the first round comes about 14 seconds after Harold starts. A prompt due at the moment Harold starts is typed about 14 seconds later. Harold waits because the list of panes it starts with is the one from before it stopped. The wait gives the [agent monitor](../agent-monitor/README.md), the part of Harold that lists the tmux panes running agents and reads their screens, time for two listings of the panes and one reading of their screens when each read works at its first try. A listing that fails is retried by the next one, so each failed listing adds one more listing, which the wait does not cover. Its length is twice the sum of `inventory_interval_ms` and `inventory_timeout_ms`, plus `screen_interval_ms`, plus 2 seconds, with all three keys from `[agent_monitor]`: 14 seconds with the shipped settings. After the first round, a round runs every 5 seconds.

## The seven checks

For each due prompt, in the order of the list, Harold checks these in order and stops at the first that fails. The prompt then stays due and is checked again in the next round.

| # | Check | Reason logged when it fails |
|---|---|---|
| 1 | Harold's last listing of tmux panes worked | `pane_list_failing` |
| 2 | At least one agent pane has `folder` as its working directory | `no_agent_in_folder` |
| 3 | Exactly one does, except in the chief's folder, where the chief's pane is the one (the first agent pane of the chief's tmux session) | `several_agents_in_folder` |
| 4 | That agent is Claude Code (its provider uses the `claude-v1` screen adapter) | `input_box_unreadable` |
| 5 | Harold's current state for that agent is idle | `agent_busy` or `agent_state_unknown` |
| 6 | A fresh read of the pane's screen finds the input box | `input_box_not_found` |
| 7 | The input box is empty, and in Vim INSERT mode when Claude Code's editor is Vim | `input_box_holds_text`, `input_box_not_insert` |

When all seven pass, Harold types `text` into the pane as literal keys and then sends Enter. Harold adds nothing before or after the text. Two of the [limits](#limits) say when tmux types less than the text.

Two more things make a prompt wait:

- A pane Harold typed into counts as busy until Harold has seen its agent in a state other than idle or the pane is gone, for at most the next 12 rounds. When Harold never sees the agent leave idle, for example because it answered in under 5 seconds, a second text can go into the pane about 65 seconds after the first at the earliest. Two entries for one folder and one time are typed in list order, the second once the agent is idle again.
- If typing the text fails, nothing is recorded and Harold tries again in the next round. The reason logged is `typing_failed`.

If the text is typed and Enter fails, Harold records the prompt as failed and does not try again that day. Trying again would put the text in the box twice.

## Reading the input box

Check 6 reads the pane's visible screen once with `tmux capture-pane -e`, which keeps the codes for dim text and reverse video. Claude Code draws its input box as a row starting with `❯` between two rows of `─`:

```
────────────────────────────────
❯ 
────────────────────────────────
  -- INSERT -- ⏵⏵ bypass permissions on
```

The input box is found when the lowest row that starts with `❯` has a row starting with `─` directly above it and directly below it. It is empty when that row holds nothing but spaces after the `❯`. Harold does not count the text drawn dim, which is a suggested prompt. It does not count a reverse-video space, or one reverse-video letter that dim text follows directly: that is the cursor. Other reverse-video text is typed text, so a box with one typed letter holds text. A line that waits for the chief therefore goes in as soon as the chief waits at its prompt, with a suggested prompt in the box.

### Vim

Harold reads `editorMode` in `settings.json` in `$CLAUDE_CONFIG_DIR`, or in `~/.claude` when that is not set, at each read. A file that is missing or cannot be read counts as not `vim`.

When `editorMode` is `vim`, an empty box is ready only when a row below the lower rule holds `-- INSERT --`. In NORMAL mode that row is missing, and a typed key is a Vim command. So Harold does this in check 7:

1. Harold sends the one key `i`. It then reads the screen up to 5 times, 200 ms apart (1 s in all), and stops at the first of these results.
2. If the footer shows `-- INSERT --` and the box is empty, Harold types the text and sends Enter. The log line for the typed text has `insert_pressed=true`.
3. If the box holds only `i`, the box was in INSERT mode with no footer drawn, and the `i` went in as text. Harold removes the `i` (see below) and reads the screen again. If the box is then empty and the footer shows `-- INSERT --`, Harold types the text. Else Harold waits with `input_box_not_insert`.
4. If the 5th read still shows neither result, Harold waits with `input_box_not_insert`.

Harold removes its own `i` with `BSpace` or `x`. In the try that sent the `i`, an `i` that shows as text means the box was in INSERT mode already, so Harold sends `BSpace`, with or without the footer. On a later try, Harold sends `BSpace` when the footer shows `-- INSERT --`. Without that footer, the box is in NORMAL mode, where `BSpace` only moves the cursor, so Harold sends `x`. Each removal writes the log line `stray insert key removed`.

Harold remembers when it sent `i` with no text typed after it. On the next try, if that was less than 60 s ago and the box holds only `i`, Harold removes the `i` as above and goes on with the checks. After 60 s, a lone `i` is the user's text, and the line waits with `input_box_holds_text`. Typing a line clears the memory.

Harold sends `i` at most once in each try, and all of this runs while Harold holds the pane's lock. A box that holds text waits with `input_box_holds_text` in either mode.

Everything else fails a check, so doubt always means not typing:

| The screen shows | Fails |
|---|---|
| Text after the `❯`, other than dim text and the cursor (a reverse-video letter is text) | Check 7 (`input_box_holds_text`) |
| In Vim, an empty box with no `-- INSERT --` row below it, after the `i` key did not help | Check 7 (`input_box_not_insert`) |
| An input box more than one row tall | Check 6 |
| A permission question, a menu or a picker in place of the input box | Check 6 |
| Nothing, because the capture failed or took more than 2 seconds | Check 6 |

Harold never clears or changes text in the input box.

## Log lines

Harold writes one line when a prompt is typed, one when it is skipped, and one each time the reason it is waiting changes. Reasons are compared by name, so a change in the `agents` count alone writes no new line. Each carries the prompt's name. None carries its text.

| Moment | Level | Line | Fields |
|---|---|---|---|
| Typed | info | `timed prompt typed` | `name`, `pane`, `late_s` (seconds after `at`), `insert_pressed` (`true` when Harold pressed `i` first, for a Vim input box in NORMAL mode) |
| Waiting, for a reason other than the last one logged for this prompt | info | `timed prompt delayed` | `name`, `reason`, and `agents` (the count) for `several_agents_in_folder` |
| Harold took its own `i` out of the box | info | `stray insert key removed` | `pane_id`, `state` (`insert` when Harold sent `BSpace`, `normal` when it sent `x`) |
| Window ended | warn | `timed prompt skipped` | `name`, `reason`: the last reason it waited for, or `window_passed` when Harold never saw it due |
| Enter failed after the text was typed | warn | `timed prompt failed` | `name`, `pane`, `error` |
| The event could not be stored | error | `timed prompt record not stored` | `name`, `error` |
| `/bin/date` could not be read, or took more than 2 seconds | warn | `timed prompts cannot read the local time from /bin/date` | none |

A prompt whose window runs to midnight and is still waiting then is logged as skipped in the first round of the next date.

Harold keeps the last reason a prompt waited for in memory only. After a restart that comes past the end of the window, the reason is `window_passed` even if the prompt waited before the restart. A prompt waiting at midnight gets no `timed prompt skipped` line and no stored event for that date when Harold is restarted and its first round comes on the next date.

The `/bin/date` line is written once for each stretch of rounds in which the time cannot be read, not in every round. While the time cannot be read, nothing is typed and nothing is skipped.

## What Harold stores

When a prompt is typed, skipped or failed, Harold appends one `TimedPromptHandled` event to its event store. A wait is not stored.

```json
{"name": "digest", "day": "2026-10-14", "outcome": "typed", "reason": null, "pane_id": "%2"}
```

| Field | Value |
|---|---|
| `name` | The entry's `name` |
| `day` | The local date the prompt was due on. For a prompt still waiting at midnight, that is the earlier date, not the date the event is written on |
| `outcome` | `typed`, `skipped` or `failed` |
| `reason` | The reason for `skipped`; null otherwise |
| `pane_id` | The pane typed into; null for `skipped` |

The store's projection keeps the last `day` for each prompt in the `settings` table, under the key `timed_prompt.<name>.last_day`. Harold reads those rows at startup. That is what stops a prompt from being typed twice on one date across a restart.

If the event cannot be stored after a prompt was typed, Harold logs the error and still does not type the prompt again while it keeps running. A restart on the same day would type it again.

## Limits

- **Claude Code only.** Harold must read the input box to know it is empty, and it knows how for Claude Code's screen only. A prompt whose folder holds an agent whose provider uses another screen adapter, such as Codex or OpenCode, waits with `input_box_unreadable` and is skipped when its window ends. A custom agent whose provider is configured with `screen_adapter = "claude-v1"` passes check 4, and Harold reads its screen as Claude Code's.
- **A gap of about half a second.** If you start typing between Harold's read of the screen and its typing, the text is added to yours. Nothing in tmux closes that gap.
- **A text that starts with `-` is never typed.** Harold runs `tmux send-keys -t <pane> -l <text>` with nothing between `-l` and the text, and by tmux's rules a text that starts with `-` is read as options. Most such texts are refused, and the prompt then waits with `typing_failed` and is skipped when its window ends. A text made only of letters tmux accepts as options, such as `-R` or `-l`, is accepted with nothing typed. Harold then presses Enter and records the prompt as typed.
- **A `;` at the end of `text` is not typed.** By tmux's rules a `;` at the end of an argument ends the tmux command. A `text` that is only `;` types nothing.
- **One time a day for each entry.** There are no days of the week, dates or intervals.
- **Nothing is sent to the phone** when a prompt is typed or skipped. Pausing messages to the phone has no effect on timed prompts.
- **Not on the dash.** The log is where to see what happened to a prompt.

## The restart reminder

Harold also types one line of its own into the chief's pane: a reminder to hand off and restart. It is not a timed prompt: it has no time of day, and it follows the chief's Claude Code session instead.

```toml
[restart_reminder]
enabled = true
after_hours = 24
after_compactions = 2
```

- **When it is due.** At the end of a chief turn, once the session is more than `after_hours` old or has compacted at least `after_compactions` times. The session's start is the first `timestamp` among the transcript's lines for that session id, and a compaction is a line with `isCompactSummary: true`. Only those fields and the line's `sessionId` are read, never message text. The Claude Code turn hook sends the transcript's path (`TurnCompleteRequest.transcript_path`); the Codex hook sends none, so a Codex chief gets no reminder.
- **The line.** `🔁 Harold: this chief session started 18 Oct 10:04 and has compacted 4 times. Hand off and restart at your next stopping point.` The start is in local time. It is typed with the same idle checks as the `ANSWERS` line, as the writer `restart_reminder`, and does not wait on a busy pane beyond about 3 seconds, which covers the moment before Harold's snapshot shows the pane idle: Harold tries again at the next turn end. The state is saved after the line is typed, so a Harold that stops in between may type it once more after the restart.
- **Once per session.** `restart-reminder.json` in Harold's data folder (mode 0600) keeps, for each session id, the transcript's path, the offset read so far, the start, the compaction count and when the reminder was typed. Harold reads a transcript in full once, then only the bytes added after the offset. Sessions not seen for 30 days are dropped. A new session starts fresh.
- **Log lines.** `chief restart reminder typed` carries `session.id`, `age_hours` and `compactions`. `chief restart reminder not typed` carries `session.id` and `reason`: `pane_busy`, `no_chief`, `transcript_unreadable` or `no_transcript_path`. It is written once for each session and reason.

| Line | Fields |
|---|---|
| `chief restart reminder typed` | `session.id`, `age_hours` (the session's age), `compactions` (how many times it compacted) |
| `chief restart reminder not typed` | `session.id`, `reason` |
