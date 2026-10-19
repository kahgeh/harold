# Backlog and Queue

The backlog tool is Harold's second [tool](README.md). It holds two kinds of list that agents used to keep in markdown files:

- **each product's backlog**, kept by that product's manager: up to 5 ranked items and the notes around them;
- **the chief's queue**: the one thing the user is on now, the parked items, and the done ones.

Agents change the lists with `harold tool backlog ...`, one small command at a time, and anyone can read them. The tool moves each `backlog.md` and `queue.md` in once, without losing a line, and can write the markdown back out at any time.

The service is `harold.tools.backlog.v1.BacklogService`. The proto is `harold-api/proto/tools/backlog.proto`. It trusts local callers, as the [ADR](../../adr/0002-tool-services-trust-local-callers.md) says; the [write check](#who-may-write) only prevents mix-ups. The tool types into no pane: the host gives it no `typing`.

## A product's backlog

The products are the ones in Harold's pod registry, the vault's `products.toml` named by `[telemetry] pod_registry`. The tool keeps no list of its own. A product's folder is its `repo` there. A write for a product that isn't in the registry is refused with `unknown_product`; a product dropped from the registry keeps its backlog readable.

Each product's backlog holds ranked items and notes.

### Items

A product manager writes an item as JSON and passes it to `harold tool backlog put <product>`:

```json
{
  "id": "search-filter",
  "title": "Let the list be filtered by a typed word",
  "value": "a long list is hard to scan; a filter finds an entry in seconds",
  "size": "small",
  "risk": "the filter could hide an entry the user expects to see",
  "reason": "three people asked for it this week",
  "paths": "app/src/list.rs, app/src/filter.rs",
  "after": "nothing before it",
  "evidence": ["notes/2026-02-28.md:12", "harold.log:3685"]
}
```

`id` and `title` are required strings. `evidence` needs at least one pointer: an item without it is sent with an empty list and refused with `invalid evidence`. Every other field may be left out, which stores it empty. The rules, counted in characters, not bytes:

| Field | Rule |
| --- | --- |
| `id` | 1 to 48 characters from `A-Z`, `a-z`, `0-9` and `-`; unique in the product. |
| `title` | One line, not empty, under 2,000 characters. |
| `value`, `risk`, `reason`, `paths`, `after` | Text, each under 2,000 characters (1,999 at most). |
| `size` | Empty, `small`, `medium` or `large`. |
| `evidence` | 1 to 20 pointers, each under 500 characters. |

A product holds at most 5 items, ranked 1 to 5:

- A new id goes last. Putting an id that is already there replaces the item and keeps its rank.
- A 6th item is refused with `backlog_full`.
- A broken rule is refused with `invalid <field>`, naming the first field that fails, such as `invalid evidence`, and nothing is stored.
- `rank` puts the listed ids first, in order, and the rest after them in the order they had.
- `remove` closes up the ranks after the item.

An evidence pointer is a reference, such as `harold.log:3685`, a commit, a file and line, or "who, when". Raw log or transcript text doesn't belong in one. The tool can't tell the difference; it checks only the lengths.

### Notes

A backlog note has a `kind`, a `title` (one line, under 2,000 characters), a `body` (markdown, under 20,000 characters), and the times it was made and last changed. There are four kinds:

| Kind | What it holds | Changes |
| --- | --- | --- |
| `rule` | The user's standing rules at the top of the backlog. | Add, update, remove. |
| `history` | The dated sections. | Append only: an update or removal is refused with `append_only`. |
| `held` | "In flight with another session". | Add, update, remove. |
| `left_out` | "Looked at and left out", one backlog note per entry. | Add, update, remove. |

Removing an item with a reason also adds a `history` backlog note, in the same transaction. Its title is `removed <id> (YYYY-MM-DD)`, with the local date, and its body is `removed <id>: <reason>`. The date in the title is what lets an export of it import again. An empty reason adds no backlog note.

Backlog note ids are integers, shared by every product. A new backlog note takes the highest id plus 1, so after the backlog note with the highest id is removed, the next backlog note gets that id again. History backlog notes can't be removed, so this happens only with `rule`, `held` and `left_out` backlog notes.

## The chief's queue

There is one queue, the chief's. It holds:

- **at most one Now item:** the thing the user is on;
- **the parked items,** at positions 1 to n with no gaps;
- **the done items,** each with the time it was done. Done items are kept; nothing in the queue is deleted.

Each item has an id, `N1`, `N2` and so on, and a `text` (markdown, under 4,000 characters). A new id is `N<k>` with k one more than the highest ever stored, so an id never repeats. The queue also has one `note` (under 4,000 characters), the free text at its top, such as the session and handoff lines.

How the items move:

- `add` parks a new item at the end. `--at <n>` parks it at position n, which must be 1 to parked + 1, or the command is refused with `invalid at`. `--now` makes it Now and parks the old Now at position 1.
- `now <id>` makes a parked item Now and parks the old Now at position 1. A done item is refused with `already_done`. An item that is already Now stays as it is.
- `order <id>...` puts the listed parked ids first, in order, and the other parked after them in the order they had. An id that isn't parked is refused with `not_parked`.
- `done <id>` marks a Now or parked item done. Done on the Now item leaves no Now. An item already done is refused with `already_done`.
- `update <id>` replaces the text of any item, done ones included.

## Who may write

Every write carries the caller's pane, which the CLI reads from `TMUX_PANE`. Reads need no pane.

A write to a product's backlog (items, notes, an applied import) is checked in this order:

1. No pane: refused with `no_pane`.
2. The registry is set but can't be read: refused with `registry_unreadable`. The product isn't in the registry: refused with `unknown_product`.
3. The pane is the chief's: allowed. The chief's pane is the one agent pane whose folder is `[notify] away_messages_only_from`.
4. The pane isn't an agent's, so it has no folder: refused with `wrong_product`.
5. The pane's folder is the product's `repo` or inside it: allowed. Otherwise: refused with `wrong_product`.

Step 5 resolves both paths first, so a trailing slash or a symlink (macOS `/var` is `/private/var`) still counts as inside. A path that can't be resolved is used as given, unless it holds `..`, which is refused. A `repo` that is empty or relative has nothing to be inside of, so it is refused. The paths are then compared by components, not as text: a pane in `~/Dev/p/harold-tasks` is not inside `~/Dev/p/harold`, and a pane in `~/Dev/p/harold/sub` is.

A write to the queue (every `queue` change, an applied queue import) is refused with `no_pane` without a pane, and with `not_chief` from any pane but the chief's.

This guards against mix-ups. It isn't security. Harold trusts local callers, and a local process can claim any pane. It stops one product's manager writing another's backlog by mistake.

## The CLI

The product managers and the chief use `harold tool backlog`. Each command prints its output on success, or one line on stderr and exits 1.

For one product:

```sh
harold tool backlog put <product> < item.json        # stored search-filter at rank 1
harold tool backlog rank <product> <id>...           # ranked 5
harold tool backlog remove <product> <id> [--reason <text>]   # removed search-filter
harold tool backlog list <product> [--json]          # one line per item, then "5 items"
harold tool backlog note add <product> <kind> < note.json     # added note 12
harold tool backlog note update <product> <note-id> < note.json   # updated note 12
harold tool backlog note remove <product> <note-id>  # removed note 12
harold tool backlog notes <product> [--kind <kind>] [--last <n>]
```

- `list` prints `<rank>. <id>: <title>` for each item, then `<n> items`. With `--json` it prints a JSON array of the items with every field, `rank` and `updated_at_ms`.
- `note add` and `note update` read `{"title": "...", "body": "..."}` from stdin; both fields are required.
- `notes` prints each note as a line `<id> <kind> <title>`, then its body, with a blank line between notes, in id order. `--kind` keeps one kind; `--last <n>` keeps the newest n.

For the queue (the chief only, except `list`):

```sh
harold tool backlog queue add [--now | --at <n>]  # text on stdin; added N7 parked 3, or added N7 now
harold tool backlog queue now <id>                # now N7, 4 parked
harold tool backlog queue order <id>...           # ordered 4
harold tool backlog queue update <id>             # text on stdin; updated N7
harold tool backlog queue done <id>               # done N7
harold tool backlog queue note                    # text on stdin; queue note set
harold tool backlog queue list [--json] [--done-since <YYYY-MM-DD>]
```

The text on stdin is taken as it is, without the newlines at its end. `queue list` prints:

```text
Now: N4 Draft the release note for the filter change
Parked:
  1. N3 Reply to the question about CSV quoting
  2. N2 Book the room for Thursday's review
Done today:
  N1 Merged the dark theme colours
```

Each item shows its first line only. With no Now item the first line is `Now: nothing`. The done items are listed oldest first. "Done today" means done since local midnight. `--done-since <date>` lists the items done since that date's midnight instead, under `Done since <date>:`. The CLI works out both midnights with the Mac's current UTC offset, so across a daylight-saving change `--done-since` is an hour off. With `--json` it prints `{"now", "parked", "done", "note"}`, each item with its `id`, `state`, `position`, full `text`, `done_at_ms` and `updated_at_ms`.

Moving the files in and out is under [Import and export](#import-and-export).

### Lines for a refusal or a failure

| Line | When |
| --- | --- |
| `refused: pane <pane> is not in <product>'s folder` | `wrong_product`. |
| `refused: pane <pane> is not the chief's pane` | `not_chief`. |
| `refused: not run in a tmux pane` | `no_pane`: `TMUX_PANE` is not set. |
| `the Harold daemon does not serve the backlog` | The tool did not start, so the service is missing (`Unimplemented`). |
| `the Harold daemon is not reachable` | The daemon is not running. |
| `the Harold daemon did not answer within 3 seconds` | Any call but import and export, after 3 seconds. |
| `the Harold daemon did not answer within 30 seconds` | An import or export, after 30 seconds. |
| `invalid JSON: <what>` | `put`, `note add` or `note update` got empty or broken JSON, such as `invalid JSON: id is missing`. Nothing is sent. |
| `cannot read <file>: <error>` | An import file can't be read. Nothing is sent. |
| `cannot read stdin: <error>` | `queue add`, `queue update` or `queue note` can't read stdin. Nothing is sent. For `put` and the note commands the same failure prints `invalid JSON: cannot read stdin: <error>`. |
| `the Harold daemon refused the call` | A refusal that carries no message. |
| `invalid argument: usage is harold tool backlog put\|rank\|remove\|list\|note\|notes\|queue\|import\|export` | Any other use: a missing or unknown argument, a flag given twice, `--now` with `--at`, `--at 0`, a note id that isn't a number, or a date that isn't `YYYY-MM-DD`. |
| the refusal's message | Any other refusal, such as `backlog_full` or `invalid evidence`. |

The CLI takes any word starting with `--` as a flag, so an item id that starts with `--` can be stored through `put` but can't be named on the command line for `rank` or `remove`.

### Refusals

| Status | Message | When |
| --- | --- | --- |
| `PermissionDenied` | `no_pane`, `wrong_product`, `not_chief` | The [write check](#who-may-write) refused the caller. |
| `NotFound` | `unknown_product` | A write, or a dry-run backlog import, names a product that isn't in the registry. |
| `NotFound` | `not_found` | The item, note or queue id isn't there. |
| `FailedPrecondition` | `backlog_full` | A 6th item, by `put` or by import. |
| `FailedPrecondition` | `append_only` | An update or removal of a `history` note. |
| `FailedPrecondition` | `not_empty` | An import into a product or queue that already holds something. |
| `FailedPrecondition` | `not_parked` | `queue order` names an id that is Now or done. |
| `FailedPrecondition` | `already_done` | `queue now` or `queue done` on a done item. |
| `InvalidArgument` | `invalid <field>` | A rule is broken, such as `invalid risk`, `invalid evidence`, `invalid kind`, `invalid at`, `invalid text` (too long, or empty on `queue add` and `queue update`), or `invalid ids` (an empty or repeated id list). Nothing is stored. |
| `InvalidArgument` | `unparsed line <n>` | An import line has no place; `n` counts from 1. Nothing is stored. |
| `Unavailable` | `registry_unreadable` | The registry is set but can't be read. Writes, the product list (`ListProducts`) and a backlog dry-run import are refused; reading items, notes, the queue and an export still answer. |
| `Unavailable` | `the backlog database is unavailable` | A database call failed. |

## Import and export

```sh
harold tool backlog import <product> <backlog.md> [--apply]
harold tool backlog queue import <queue.md> [--apply]
harold tool backlog export <product>
harold tool backlog queue export
```

An import moves a whole file in once. Without `--apply` it is a dry run: it runs every check, stores nothing, and prints the counts it would store:

```text
items 5, notes: rule 2, history 4, held 3, left_out 3
now 1, parked 4, done 2
```

With `--apply` it stores everything in one transaction and prints the same counts after `imported `, such as `imported now 1, parked 4, done 2`.

The checks run in this order:

1. **Who:** an applied import is a write, so it is checked like any [write](#who-may-write). A dry run needs no pane, but a backlog dry run still refuses a product that isn't in the registry with `unknown_product`.
2. **Every line has a place:** a line the importer can't place is refused with `unparsed line <n>`, rather than dropped.
3. **The limits:** each item and backlog note must meet its rules (`invalid <field>`); two items with one id are `invalid id`; more than 5 items are `backlog_full`.
4. **Empty:** a product that has any item or note, or a queue that has any item or a note, is refused with `not_empty`. A second `--apply` is refused this way, so nothing is stored twice.

Import and export wait 30 seconds for the daemon; every other command waits 3. A timed-out `--apply` may still have been stored, because the daemon can finish after the CLI has given up. `harold tool backlog list <product>` or `queue list` shows whether it was; if it was, another `--apply` is refused with `not_empty`.

The tool never writes or deletes `backlog.md` or `queue.md`. Once an import is applied, the product manager or the chief renames the file, so no one edits the old copy. `export` prints the markdown whenever someone wants it.

### The `backlog.md` layout

Only lines at column 0 that start `# ` or `## ` are headings; an indented `  ## 5. x` is body text. `\r\n` reads as `\n`.

| Part of the file | Becomes |
| --- | --- |
| The `# ` title, as the first line that isn't blank | Nothing; it is skipped. A `# ` line anywhere else is `unparsed line <n>`. |
| Each paragraph before the first `## ` heading | A `rule` note. Its title is its first line, cut to 80 characters; its body is the paragraph as written. |
| `## <n>. <id>: <title>` and its lines | An item, ranked in the order the items appear. |
| `## In flight with another session...` (a heading that starts with that text) | `held` notes, one per block. |
| `## Looked at and left out...` | `left_out` notes, one per block. |
| Any other `## ` heading that holds a `YYYY-MM-DD` date | A `history` note. Its title is the heading after `## `; its body is the section's lines as written, without the blank lines at its start and end. |
| A `## ` heading with no date | `unparsed line <n>`. |

An item's lines are `- <field>: <value>`, for `value`, `size`, `risk`, `reason`, `paths`, `after` and `evidence`, in that order, each at most once. The value starts on the field's line. An indented line continues the field above it, joined with a newline, up to a blank line. `evidence` is split at each `; `. Any other line, a field out of order or repeated, or a field with no value is `unparsed line <n>`.

In the held and left-out sections, a block starts at each column-0 `- ` bullet, or at a column-0 line after a blank line. Indented lines continue it, and so do blank lines followed by one. The backlog note's title is the block's first line without its `- `, cut to 80 characters; its body is the block as written.

The export writes `# Backlog: <product>`, then the rule notes' bodies, the items by rank (each field with a value, as `- <field>: <value>`, with the evidence joined by `; `), the history notes as `## <title>` and body, then `## In flight with another session (not ranked)` and the held notes, and `## Looked at and left out` and the left-out notes, each kind only when it has backlog notes. Backlog notes go by id, with one blank line between blocks.

Every line of the source that isn't blank and isn't a heading appears in the export, in the same order within its section. A backlog note added through `note add` comes back the same way only if it keeps the file's shape: a `history` backlog note's title needs a date, and a `rule` backlog note's body needs to be one paragraph, since a blank line would split it into two rules on the next import. The export writes only the body of a `rule`, `held` or `left_out` backlog note, not its title. On the next import the title is the body's first line, cut to 80 characters (without its `- ` for held and left-out notes), so a title set by hand is lost unless it matches that.

### The `queue.md` layout

| Part of the file | Becomes |
| --- | --- |
| A first `# ` title, such as `# Queue`, as the first line that isn't blank; its text isn't checked | Nothing; it is skipped. A `# ` line anywhere else is `unparsed line <n>`. |
| The lines between it and the first `## ` heading | The queue's note, as written, without the blank lines at its start and end. |
| `## Now` | One `- ` bullet and the indented lines after it: the Now item. Its text is the first line without `- `, then the indented lines as written. |
| `## Parked` | `<n>. ` lines and the indented lines after each: the parked items, in order. The text drops the `<n>. `. |
| `## Done today` or `## Done` | `- ` bullets and their indented lines: the done items. The text drops the `- `. |

In any section a column-0 `Nothing.` is skipped. Any other line that isn't part of an item, any other `## ` heading, or a section given twice is `unparsed line <n>`. The ids are given from `N1` up, in the order Now, parked, done.

The importer reads one bullet under `## Now`. A second top-level bullet there is refused with `unparsed line <n>`, naming that bullet's line. To import such a file, fold the extra bullet into `## Parked`, or merge it into the Now item as indented lines, then import again.

The done items are dated at local midnight of the first `YYYY-MM-DD` in the note, with the UTC offset the CLI sends. A note with no date dates them at the time of the import. So imported done items show under "Done today" only when that date is today.

The export writes `# Queue`, the note, `## Now` (`- <text>`), `## Parked` (`<n>. <text>`, numbered 1 to n), and `## Done` with every done item (`- <text>`). An empty section holds `Nothing.`.

## Log lines

Seven fixed lines, with only the fields `tool`, `product`, `item_id` or `note_id`, `count` and `reason`. No item, note or queue text is ever logged.

| Line | Fields | Notes |
| --- | --- | --- |
| `backlog item stored` | `tool`, `product`, `item_id` | |
| `backlog item removed` | `tool`, `product`, `item_id` | |
| `backlog ranked` | `tool`, `product`, `count` | `count` is the number of items. |
| `backlog note added` | `tool`, `product`, `note_id` | |
| `queue changed` | `tool`, `count` | After each `queue add`, `now`, `order`, `update`, `done` and `note`; `count` is the number of parked items after it. |
| `backlog imported` | `tool`, `product`, `count` | After an applied import; `count` is the number of items and notes stored. A queue import has no `product`, and its `count` is the number of queue items. |
| `backlog write refused` | `tool`, `product`, `reason` | `reason` is `no_pane`, `unknown_product`, `wrong_product`, `not_chief` or `registry_unreadable`. `product` is there only when it is in the registry, since the request's own string may be anything. |

A note update or removal logs nothing. A tool that can't start logs `tool failed to start` with `tool=backlog reason=store_open_failed`.

## Data folder and rollback

The data folder is `<parent of the store path>/tools/backlog/`. It holds `backlog.db`, a Turso database with four tables: `items`, `notes`, `queue_items` and `queue_note`.

The core store is untouched. To roll back:

- Run an older Harold. It ignores `tools/backlog/`.
- Delete `backlog.db` to empty every backlog and the queue. Harold makes a fresh one at the next start.
