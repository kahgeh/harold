# Evidence Tool

The **evidence tool** checks, after a release, that the evidence of each merged spec is in ClickHouse. It writes the result in the pod folder of the product. It is a [tool](README.md) with no database: its state is the evidence files. The rules are in [ADR 0038](../../adr/0038-harold-reads-clickhouse-for-the-evidence-check-and-writes-the-evidence-and-confirmed-files.md).

The service is `harold.tools.evidence.v1.EvidenceService`, with one call, `Check`. It trusts local callers (ADR 0002). The proto is `harold-api/proto/tools/evidence.proto`.

## The command

```sh
harold tool evidence check harold                    # every spec of the product that is not final
harold tool evidence check harold evidence-check     # one spec, even in a final state
```

The command prints one line for each spec: `<id> <state> <query> <present|absent> <reason>, ...`. When a read fails, the line is `<id> <state> read failed: <outcome>`, and the file keeps its old results. Any other argument gives this refusal: `invalid argument: usage is harold tool evidence check <product> [<id>]`. An unknown product gives `unknown product <name>`.

## What the check does

1. For each product of the registry that is not paused, it takes each spec that has a `merged/<id>.md` file and at least one evidence query on the base branch. The [evidence queries](../telemetry/queries/evidence/README.md) are the files `docs/references/telemetry/queries/evidence/<id>-<n>.sql`.
2. It reads each query with `git show <base branch>:<path>`. A file that is not merged does not count. A file over 64 KiB counts as a `bad_query`.
3. It sends the query with `param_since`, the merge time in UTC, through the shared ClickHouse read (`harold/src/clickhouse_read.rs`). The user is `hyperdx_ro`, the query has `readonly=1` and `max_execution_time=5`, and the address must be loopback. The settings are `[factory_flow] clickhouse_url` and `[factory_flow] users_file`. The password is read at each read and dropped after it.
4. It applies the state rule and writes `evidence/<id>.md`.

## The line to the chief

When a spec's file says `state: missing` and has no `told:` line, the tool types one line into the chief's pane:

```
EVIDENCE harold evidence-check missing: evidence-check-2 (no_use_yet)
```

The form is `EVIDENCE <product> <id> missing: <query> (<reason>)`. For more than one absent query, the line names each, separated by commas. The line holds names and fixed reason words only. It is not an alert and does not go to the phone.

- The tool types the line with the writer of the other tools, `Writer::Tool("evidence")`, and the chief lookup of the restart reminder. It types only into an idle pane with an empty input box, so the line never cuts into a turn. A busy pane gets a few short retries, about 3 seconds in all, and then the line waits.
- After the line is typed, the tool adds `told: <time>` to the evidence file.
- With no chief found, or with a busy pane, nothing is typed and the file has no `told:` line. The tool tries again at the end of the next turn of the chief and at the next daily check.
- The chief gives the line to the PnEM, who writes the fix spec `<id>-evidence`.
- The list **evidence missing** of `harold tool release next` and the line `N installed without evidence` of the factory flow page show the same specs. A spec whose evidence becomes `present` leaves both at the next check.

## When the check runs

The pass runs once at the start of Harold and then each 24 hours. A release install restarts Harold, so each release gets a check at once. A spec in a final state (`present` or `missing`) is not checked again by the pass.

## The state

| State | When |
| --- | --- |
| `present` | Every evidence query returns `present = 1`. |
| `missing` | One query still returns 0 seven days after the release that holds the merge commit. |
| `waiting` | Any other case. |

The 7 days start at the time of the first `v*` tag that holds the merge commit. While no tag holds it, the state stays `waiting`. For a product with no `v*` tag at all, the 7 days start at the merge.

A failed read does not change the state. The `outcome` of each read is one of `ok`, `unreachable`, `refused`, `timeout`, `not_set_up`, `bad_query` and `bad_row`. A row is bad when it is not exactly one row, when `present` is not 0 or 1, or when `reason` is not 1 to 40 characters from `a` to `z` and `_`.

## The files

`evidence/<id>.md`, written again at each check:

```
# Evidence: <id>

checked: 2026-11-20T09:00:00+11:00
since: 2026-11-12T14:05:00+11:00
released: 2026-11-13T10:00:00+11:00
state: waiting
query: evidence-check-1 present gauge_seen
query: evidence-check-2 absent no_ok_read
```

`released:` is empty while no tag holds the merge. The file holds no message text. A rewrite keeps the lines that a later step added, such as `told:`.

`told: <time>` is the line that the evidence tool adds after it types the `EVIDENCE` line for a `missing` spec into the chief's pane. Each rewrite of the file keeps it. So the daily check and a manual `harold tool evidence check <product> <id>` give the chief one line for each spec, and no second line. The tool sends one line for each spec in all: a spec that goes from `missing` to `present` and back to `missing` gets no new line. If the write of `told:` fails after the line is typed, Harold keeps the spec in memory, writes `told:` again at the next check and types no second line.

`confirmed/<id>.md` is written once, for a spec of kind `fix`, when its state becomes `present`. It has the lines `confirmed:` and `evidence:`. Each file goes to a temporary name first and is then renamed, so a reader never sees half a file.

## Telemetry

| Signal | What it shows |
| --- | --- |
| `pod.evidence.missing` (gauge, `spec.id`, `pod.product`) | 1 when the state is `missing`, else 0. Pod stats exports it from the evidence files. |
| `harold.evidence.reads` (counter, `outcome`) | One for each read of one evidence query. Each outcome starts at 0. |
| `harold.evidence.lines` (counter, `outcome`) | One for each try to type the line: `typed`, `no_chief` or `not_typed`. Each outcome starts at 0. |
| `harold.feature.used` with `spec.id=evidence-follow-up` | One count each time a line is typed. |
| log `evidence line typed`, `evidence line not typed` | `spec.id`, and `reason` for the second. `info` and `warn`. |
| `harold.feature.used` with `spec.id=evidence-check` | One count each time a spec's state first becomes `present` or `missing`. |
| log `evidence query read` | `spec.id`, `evidence.query`, `outcome`, `elapsed_ms`. No SQL. `info` for `ok`, `warn` for the others. |
| log `evidence file written` | `spec.id`, `state`, `confirmed`. |

The card **Evidence reads** on the dashboard "Harold: the cards" shows the outcomes of the last 7 days beside **Factory flow reads**.
