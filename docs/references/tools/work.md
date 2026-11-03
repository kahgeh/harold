# Work tool

The work tool keeps the chief's work records. A work record is one piece of work: its priority, its status, what it waits for, its next step, its decisions and its links. The tool stores the work records in Turso files of its own. Harold moves the open work records to a new file by itself, every `rotation_days` days.

The tool is a module of Harold ([ADR 0001](../../adr/0001-tools-are-modules-hosted-by-harold.md)). Its gRPC service is `harold.tools.work.v1.WorkService`, in `harold-api/proto/tools/work.proto`, on Harold's loopback address. It trusts local callers ([ADR 0002](../../adr/0002-tool-services-trust-local-callers.md)). The reason for the files is in [ADR 0028](../../adr/0028-work-records-live-in-rotating-turso-files-of-their-own.md).

The work tool does not change the queue, `backlog.db` or the question tool. The old queue commands work as before.

## The work record

| Field | Meaning and limits |
| --- | --- |
| `name` | 1 to 48 characters of `a-z`, `0-9` and `-`. Unique in the current file. A work record keeps its name for life, so a closed work record keeps it too |
| `title` | One line |
| `priority` | `focus`, `next` or `later`. At most two open work records are in `focus` |
| `status` | One of `idea`, `discovery`, `spec drafted`, `architect checked`, `waiting for the user`, `approved`, `building`, `merged`, `released`, `checked by the user` |
| `waits_for`, `next_step` | One line each |
| `created_in` | The name of the work file that first held the work record. The tool sets it |
| `created_at`, `updated_at` | Times that the tool sets |
| `status_since` | The time the status last changed. A rotation keeps it |
| decisions | A list. Each has `at` (a date, `YYYY-MM-DD`), `who` (`the user`, `the chief` or a session name), `mark` and `text` |
| links | A list of strings: `spec:<product>/<id>`, `question:<id>` or `note:<text>` |

A work record is **open** before `checked by the user`, and **closed** at it.

A **decision** has one of four marks: `agreed`, `draft`, `dropped` or `open`. The tool adds a decision and changes its mark. It never removes a decision. A decision has a number, from 1, which is its place in the list.

At most two open work records are in `focus`. The tool refuses a third with `focus_full`.

## The commands

Run the commands as `harold tool work <command>`. Only the chief's pane can write. A write from another pane is refused with `not_chief`. The reads, `check`, `import` without `--apply`, and `rotate --dry-run` need no pane.

A value with spaces needs quotes. For example: `--status "waiting for the user"`.

| Command | What it does |
| --- | --- |
| `list [--all]` | Prints the open work records of the current file: `focus` first, then `next`, then `later`. `--all` adds the closed ones. A last line names the work records at `waiting for the user` for more than a day |
| `show <name>` | Prints one work record with its decisions and links |
| `put <name>` | Makes a work record, or replaces the fields of an open one. It reads JSON on standard input |
| `set <name> [--priority P] [--status S] [--waits-for TEXT] [--next TEXT]` | Changes the named fields. A field that you leave out stays. `set` needs at least one flag |
| `decide <name> --mark M --who W <text>` | Adds a decision dated today (UTC). Prints its number |
| `mark <name> <number> <mark>` | Changes the mark of one decision |
| `link <name> <link>` | Adds a link |
| `unlink <name> <link>` | Removes a link |
| `check` | Compares the work records with the specs and the questions |
| `import <folder> [--queue ID,ID] [--apply]` | Reads the chief's files |
| `rotate --dry-run` | Prints what a rotation would copy. It changes nothing |
| `history <name>` | Prints the work record from the newest file that holds it |
| `search <word>` | Lists the work records that hold the word, newest file first |

### Examples

```sh
# A new work record. The name is the argument; the JSON holds the other fields.
echo '{"title": "Keep the work records in Harold", "priority": "focus", "status": "building",
       "waits_for": "nothing", "next_step": "Run the check on the six files."}' \
  | harold tool work put work-records
# created work-records

harold tool work list
# focus  work-records  [building]  next: Run the check on the six files.

harold tool work set work-records --status "waiting for the user" --waits-for "the user's answer"
# set work-records

harold tool work decide work-records --mark draft --who "the chief" "The queue stays until the import is applied."
# added decision 1 to work-records

harold tool work mark work-records 1 agreed
# marked decision 1 of work-records agreed

harold tool work link work-records spec:harold/work-records
harold tool work link work-records question:q-0412
harold tool work link work-records "note:Wait for the first rotation."

harold tool work show work-records
harold tool work history work-records
harold tool work search rotation
```

`put` accepts these keys: `title`, `priority`, `status`, `waits_for`, `next_step`, `decisions` and `links`. `decisions` is a list of objects with `who`, `mark`, `text` and an optional `at`. `links` is a list of strings. The key `name`, if present, must equal the argument. A decision with no `at` gets today's date. A replacement keeps the stored decisions, links and times. It ignores the `decisions` and `links` in the JSON. Use `decide` and `link` to change those.

### Refusals

A refusal prints one line, `refused: <word>: <reason>`, and the command exits with a non-zero code. Nothing is stored.

| Word | When |
| --- | --- |
| `not_chief` | The pane that wrote is not the chief's pane |
| `no_pane` | The write did not come from a pane |
| `focus_full` | Two open work records are in `focus` already |
| `name_in_use` | The name belongs to a closed work record of the current file |
| `not_found` | The work record, decision or link is not there |
| `read_only` | The file is read-only. Only an old file is |
| `rotation_is_harolds` | A rotation was asked for without `--dry-run`. Only Harold rotates |
| `bad_name` | The name is not 1 to 48 characters of `a-z`, `0-9` and `-` |
| `invalid <field>` | A field is wrong, such as `invalid status`, `invalid priority`, `invalid mark`, `invalid link` or `invalid word` |

The usage line is `invalid argument: usage is harold tool work list|show|put|set|decide|mark|link|unlink|check|import|rotate|history|search`. If the Harold daemon does not serve the tool, the command prints `the Harold daemon does not serve the work records`.

## The check

`harold tool work check` compares each work record of the current file with the pod files and with the question tool. When all agree, it prints nothing and exits 0. Otherwise it prints one line for each mismatch and exits 1. A clean check adds no work for the chief.

| Kind | The mismatch |
| --- | --- |
| `waiting_question` | The status is `approved` or later, and a linked question still waits |
| `merged_status_behind` | The linked spec has `merged/<id>.md`, and the status is before `merged` |
| `withdrawn_open` | The linked spec has `withdrawn/<id>.md`, and the work record is open |
| `approved_status_behind` | The linked spec has `approvals/<id>.md`, and the status is before `approved` |
| `closed_open_decision` | The work record is closed, and a decision is still `open` |
| `missing_link` | A link names a spec or a question that does not exist, or a `spec:` link has no `/<id>` |

The tool reads the pod files from each product's `.superpowers/pod/` folder, through the pod registry (`[telemetry] pod_registry`) that Harold already reads. It reads the question states from the question tool. When the registry or the question tool cannot be read, the check gives no line for the facts it cannot read.

Each line starts with the name of the work record. For example: `work-records is building but spec harold/work-records is merged.`

The chief runs the check at each digest.

## Import

`harold tool work import <folder>` brings the chief's work files in. It reads the `*.md` files directly inside the folder: at most 200 files, each at most 64 KB. It never writes the folder.

The chief's file form is:

```text
# <name>: <title>

Priority: focus
Status: building
Waits for: nothing
Next step: Finish the import.

Decisions:
- agreed, the user, 2026-10-28: The work records live in Turso files of their own.

Links:
- spec:harold/work-records
- question:q-0412
- note:The queue stays until the import is applied.
```

### The exact grammar

The parser is strict. It does not guess. The chief converts its own files to this form.

- **Blank lines** are skipped. A leading byte-order mark and a carriage return before the line end are ignored. Each line is trimmed of spaces at both ends before it is read.
- **The heading** is the first line that is a valid `# <name>` heading. It is `# <name>` alone, or `# <name>` with a separator and the title. The separator is `:`, `-`, `–` or `—`. The name is 1 to 48 characters of `a-z`, `0-9` and `-`. A separator `-` needs a space before it, because `# foo-title` is one name. The title may be empty. A non-blank line before the heading is unplaced. A second line that starts with `# ` is unplaced.
- **The field lines** are `Priority:`, `Status:`, `Waits for:` and `Next step:`. The key must have exactly this spelling and capital letters. Each field may appear once. The order is free.
- **Priority** is `focus`, `next` or `later`. The case of the value does not matter.
- **Status** is one of `idea`, `discovery`, `spec drafted`, `architect checked`, `waiting for the user`, `approved`, `building`, `merged`, `released` or `checked by the user`. The case of the value does not matter.
- **`Priority:` and `Status:` are required.** A file without one of them gives no work record.
- **`Waits for:` and `Next step:`** take one line each. A missing line is an empty value.
- **`Decisions:` and `Links:`** each stand alone on their line, may appear once, and take no value on the same line. `Links: none` is unplaced. Under them, each item is one line that starts with `- `. An item that follows a field line (not `Decisions:` or `Links:`) is unplaced.
- **A decision item** is `- <mark>, <who>, <date>: <text>`. The mark is `agreed`, `draft`, `dropped` or `open`. The case of the mark does not matter. The `who` is not empty, may hold spaces and commas, and holds no `:`. The date has the shape `YYYY-MM-DD` with a month from 01 to 12 and a day from 01 to 31. The parser does not check that the day exists in the month. The text is not empty.
- **A link item** is `- spec:<product>/<id>`, `- question:<id>` or `- note:<text>`. A `spec:` link has exactly one `/`, and the product and the id are not empty. The product, the id and the question id hold no white space. A `note:` text is not empty and not only spaces.

An unplaced line is any non-blank line that fits none of these rules. A whole file can be unplaced too. A dry run prints each one as `unplaced <file>:<line>: <reason>: <the line>`. For line 0, which means the whole file, it prints `unplaced <file>: <reason>`. The reasons are:

| Reason | Meaning |
|---|---|
| `no_heading` | A non-blank line before the heading. A file without a valid heading also gives one more entry at line 0, after the entries for its lines |
| `bad_heading` | The line starts with `# `, but the name or the separator is not valid |
| `second_heading` | A `# ` line after the heading |
| `unknown_line` | The line is neither a known field nor a `- ` item |
| `bad_priority`, `bad_status` | The value is not one of the words above |
| `second_value` | A field, `Decisions:` or `Links:` appears twice |
| `bad_decision` | A decision item does not follow the form, or has a wrong mark or date |
| `bad_link` | A link item is not one of the three link forms |
| `item_outside_section` | A `- ` line when no `Decisions:` or `Links:` section is open |
| `inline_value_after_section` | A value on the `Decisions:` or `Links:` line |
| `missing_priority`, `missing_status` | The file has no valid `Priority:` or `Status:` value. A bad value gives `bad_priority` or `bad_status` and also this entry. The entry is at the heading line |
| `file_too_large` | The file is larger than 64 KB |
| `not_utf8` | The file is not UTF-8 text |
| `unreadable_file` | The file cannot be opened or read |
| `not_a_file` | The entry is not a plain file, for example a folder or a link |
| `hidden_or_upper_case_name` | The name starts with `.`, or the extension is not exactly `.md` in lower case, such as `.MD`. The file is skipped and reported |
| `name_used_by_another_file` | Two files, or a file and a queue item, give the same name. This entry is at line 0 and prints the name after the reason |

- **Without `--apply`:** the command prints what it would store, and each unplaced line as shown above. It stores nothing. It exits with code 1 when any line is unplaced, and with code 0 when none is.
- **With `--apply`:** the command stores all the work records in one transaction, or none. It refuses while any line is unplaced (`unplaced_lines <count>`). It also refuses a name that the current file holds (`name_in_use <name>`). It refuses a work record that would be a third in `focus` (`focus_full <name>`). Nothing is lost silently.
- **Import is for the chief's files only.** No queue item comes in unless you name it.
- **`--queue N12,N34`:** imports only the named queue items. Each becomes a work record named `queue-<id>` in lower case. Its status is `idea` and its priority is `later`. The first line of its text is the next step, and the rest of the text is not copied. The queue keeps the full item. The command refuses an id that is not an open queue item (the Now item or a parked item) with `not_open_queue_item <id>`. The queue keeps every item.

```sh
harold tool work import ./work --queue N12,N34       # a dry run: prints what it would store
harold tool work import ./work --queue N12,N34 --apply
```

## Rotation

Harold moves the open work records to a new file by itself. The chief and the user do nothing.

**The setting.** `[work] rotation_days` is a whole number from 1 to 365. The default is 14. `0` turns the rotation off. Any other value is refused when the settings load. Put it in `config/local.toml`:

```toml
[work]
rotation_days = 14
```

Harold reads the setting at start and at each daily check, so a change needs no restart. A change never rewrites, renames or moves a file by itself. The cases are:

- A smaller value: a current file already older than the new value rotates at the next check.
- A larger value: the current file stays until it reaches the new age.
- `0`: no more rotation. The old files stay readable.

**When.** Harold checks at start and once a day. It rotates when the day count since the current file's date (in its name, in UTC) reaches `rotation_days`. Nothing is typed to the chief or the user.

**What moves.** Every open work record, with all its decisions and links, is copied to a new file. The new file is named for the day of the rotation. `name`, `created_in`, `created_at` and `status_since` stay as they were. Closed work records stay only in the old file, which keeps every work record as it stood on the rotation day.

**Safety.**

1. Harold writes the new file under a temporary name.
2. It counts the open work records, the decisions and the links in the old file and in the new file.
3. Only when the counts match does it rename the new file and use it.
4. A failed check leaves the old file current and unchanged. Harold tries again at the next daily check.
5. After a rotation, the old file is open for reads only.

**A dry run.** `harold tool work rotate --dry-run` prints what a rotation would copy now:

```text
a rotation would copy 7 open work records, 12 decisions and 9 links to work-2026-11-17.db
records: work-records, pnem-switcher, ...
```

## Reads

`list` and `show` read the current file only.

`history <name>` and `search <word>` read the files newest first. For each name, the first file that holds it gives the answer, so no merge step is needed. `history` prints the file name and the work record. `search` matches the word, without regard to case, in the name, the title, the next step and the text of the decisions. It prints one line for each work record: the file, the name, the status and the title.

## The files

The work files are in `tools/work/` in Harold's data folder: the folder of the store path, then `tools/work/`. Each file is named `work-<YYYY-MM-DD>.db` by the day it starts. For example, on `laptop-mac`: `<data folder>/tools/work/work-2026-11-03.db`.

The newest name is the current file. There is no pointer file. A temporary file of a rotation in progress does not count as a work file.

Each file has three tables: `records`, `decisions` and `links`, and one index on `status` and `priority`. `backlog.db` and the product backlogs are never touched.

## Telemetry

The work tool sends five metrics. The [telemetry reference](../telemetry/README.md) has their attributes.

| Metric | Type | Answers |
| --- | --- | --- |
| `harold.work.records` | gauge, by `priority` and `status` | How big is the open work, and how much is in focus? It is set at each write and at the daily check |
| `harold.work.status.duration` | histogram, seconds, by `status` and `priority` | How long does a work record stay at each status? It is recorded when a work record leaves a status |
| `harold.work.waiting_long` | gauge, by `priority` | How many open work records are at `waiting for the user` for more than a day? `list` names them |
| `harold.work.check.mismatches` | counter, by `record` and `kind` | Do the work records agree with the specs and the questions? It is added to at each check |
| `harold.work.rotations` | counter, by `outcome` (`done`, `failed`) | Does the rotation run by itself? `failed` counts each failed try, retries too |

Harold records 0 at start for each `outcome` and for each priority and status. The dashboard "Harold" has three info cards in the group "Work records". They are "Work by priority and status, and long waits", "Time at each status, p50 and p95, by week" and "Work check mismatches by day". A count above 0 of `harold.work.check.mismatches` at two digests in a row means that you must look at those work records.

## Log lines

| Line | Level | Fields |
| --- | --- | --- |
| `work rotation done` | `info` | `open_records`, `decisions`, `links` (the counts that were copied), `file` (the old current file), `new_file` |
| `work rotation failed` | `warn` | `file`, `reason` (`count_mismatch`, `write_failed` or `rename_failed`), `since` (the UTC time when the failure began) |
| `work rotation failed again` | `debug` | the same fields as `work rotation failed` |

Harold logs `work rotation failed` once when a failure begins for a file. A daily retry that fails again logs `work rotation failed again` at `debug` only. A rotation that then works logs `work rotation done`, which ends the failure.

## The alert

The alert `work-rotation-failed` counts the log lines `ServiceName = 'harold' AND Body = 'work rotation failed'` over 24 hours, and fires above 0. Its severity is warn. It goes to the chief's pane like the other alerts. Because the line is logged once for each failure, the alert fires once for each failure.

- **Action:** read the `file` and `reason` of the line, and look in the data folder `tools/work/`.
- **Expected rate:** never.
- **Definition files:** `alert-work-rotation-failed.json` and `webhook-work-rotation-failed.json` in `docs/references/telemetry/hyperdx/`. The alert reads the HyperDX logs source.
