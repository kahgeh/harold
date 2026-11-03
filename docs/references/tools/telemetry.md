# Telemetry Tool

The **telemetry tool** lets a pane record a counter, a gauge or an event with one command. The chief, a PnEM or a script in a product's folder uses it. A new signal needs no change in Harold's code.

The tool keeps its names in its own Turso file, `<store folder>/tools/telemetry/telemetry.db`. The service is `harold.tools.telemetry.v1.TelemetryService`, with the calls `Declare`, `Count`, `Gauge`, `Event`, `List` and `Remove`. The proto is `harold-api/proto/tools/telemetry.proto`. The service trusts local callers, as the [ADR](../../adr/0002-tool-services-trust-local-callers.md) states. The tool types into no pane.

Harold exports each signal as `agent.<name>`, so an agent's signal never mixes with Harold's own `harold.*` and `pod.*` signals. See the [telemetry reference](../telemetry/README.md#what-leaves-harold) for the exported fields.

## Names and prefixes

A name is `<prefix>.<rest>`. Use lower case, from `a-z`, `0-9`, `.` and `_`, at most 64 characters. The prefix decides which panes may write the name:

| Prefix | Who may write |
| --- | --- |
| `chief.` | The chief's pane only. |
| `<product>.` | The chief, or a pane inside that product's folder: its PnEM, its pod coordination, or a script run there, such as `make release`. A product is a product in the pod registry. The rule is the backlog tool's `Guard::may_write_product`. |
| any other | Refused. |

The registry is the vault's `products.toml`, named by `[telemetry] pod_registry`.

## Commands

```sh
harold tool telemetry declare harold.release.attempts counter "Release attempts, by result" --attr result=ok,failed
harold tool telemetry count harold.release.attempts --attr result=failed
harold tool telemetry count harold.release.attempts 3 --attr result=ok
harold tool telemetry gauge harold.queue.depth 7
harold tool telemetry declare chief.digest event "A digest was sent" --attr items=0-50
harold tool telemetry event chief.digest --attr items=4
harold tool telemetry list harold
harold tool telemetry remove harold.queue.depth
```

Any other form gives this line: `invalid argument: usage is harold tool telemetry declare|count|gauge|event|list|remove`. The CLI checks only the shape of the command. The daemon checks names, prefixes, kinds and values.

| Command | What it does | Output line |
| --- | --- | --- |
| `declare <name> counter\|gauge\|event "<meaning>" [--attr key=v1,v2,...]` | Stores the name, its kind, its meaning and its allowed attribute values. | `declared <name>` |
| `count <name> [n] [--attr key=value]` | Adds `n` (default 1, at most 1,000,000). | `counted <name> by <n>` |
| `gauge <name> <value> [--attr key=value]` | Sets the value. Harold keeps the last value of each series in its file, so a restart sends it again. | `set <name> to <value>` |
| `event <name> [--attr key=value]` | Writes one log line, `agent event`, with `agent.name` and the declared attributes. | `recorded <name>` |
| `list [<prefix>]` | Shows the declared names. | one line for each name: name, kind, quoted meaning, `key=values` |
| `remove <name>` | Removes a declaration. Its seeding and its export stop at once. Its data stays in ClickHouse. | `removed <name>` |

Rules for the flags:

- Give each `--attr` key once. An `--attr` with no `=` gives the usage line.
- A value list in `declare` may be a range of whole numbers, such as `0-50`.
- `list` shows the declared names only. It is not counted in `harold.telemetry.calls`, and its output is not logged.

## Declare first

Every signal needs a `declare` before its first write. An undeclared name is refused.

A declared counter starts from 0 for each series, at once and at each Harold start. See [Counters in HyperDX](../telemetry/README.md#counters-in-hyperdx).

**Declaring again.** The same name, kind and attribute lists are accepted again, with no change, and the command exits 0. A different kind or attribute list is refused with `declaration_differs`. To change a declaration, `remove` it and declare it again.

**The meaning stays local.** A meaning has at most 80 characters. The tool stores it in `telemetry.db` and shows it only in `list`. Harold never exports it, not as an attribute, a log field or an instrument description.

An attribute key or value that you did not declare is refused. A write can add no series.

## Caps

| Cap | Value |
| --- | --- |
| Name length | 64 characters |
| Names for each prefix | 50, of which at most 20 are events |
| Attribute keys for a name | 3 |
| Series for a counter or a gauge | 100: the product of the value counts of its keys |
| Count in one `count` | 1,000,000 |
| Meaning length | 80 characters |
| Writes for each prefix | 60 in a sliding minute |

An event has no series to seed. Its keys and values are declared and checked in the same way.

The rate limit applies to `count`, `gauge` and `event` only. `declare`, `remove` and `list` are not limited.

## Events

An event writes one log line, `agent event`, with the fields `agent.name` and `agent.attr.<key>`. An event carries no free text. Put a note in prose in a work record's decisions, not in telemetry.

An event goes to the collector only. It is not in `harold.log`, because `tracing` cannot name a field at run time.

## Refusals

A refusal is a gRPC status with the plain text as its message and a fixed reason word in the `reason` metadata. The CLI prints the text.

| Reason word | gRPC code | Text |
| --- | --- | --- |
| `no_pane` | `PermissionDenied` | `run this from a pane: no TMUX_PANE or REX_BLOCK` |
| `wrong_prefix` | `PermissionDenied` | `harold. is for panes in harold's folder and the chief`, or `chief. is for the chief's pane` |
| `unknown_prefix` | `PermissionDenied` | `unknown prefix web.: use chief. or a product name` |
| `too_many_names` | `FailedPrecondition` | `harold. has 50 names: remove one first`, or `harold. has 20 events: remove one first` |
| `too_many_series` | `InvalidArgument` | `harold.release.attempts would have 120 series: the most is 100` |
| `undeclared_value` | `InvalidArgument` | `result=maybe is not declared: use ok or failed` |
| `wrong_kind` | `FailedPrecondition` | `harold.release.attempts is a counter, not a gauge` |
| `undeclared` | `FailedPrecondition` | `harold.release.attempts is not declared: declare it first` |
| `rate_limited` | `ResourceExhausted` | `harold. wrote 60 times this minute: wait` |
| `meaning_too_long` | `InvalidArgument` | `the meaning has 95 characters: the most is 80` |
| `declaration_differs` | `FailedPrecondition` | `harold.release.attempts is declared with other values: remove it and declare it again` |
| `invalid` | `InvalidArgument` | A bad name, range, key or value, with its own text, such as `invalid name: use a prefix, a dot and a rest, from a-z, 0-9, . and _, at most 64 characters` |
| `invalid` | `FailedPrecondition` | `the product registry cannot be read: try again later` |

The registry-unreadable refusal has the reason word `invalid` and the code `FailedPrecondition`. It has no word of its own.

A database failure is not a refusal. The call returns `Unavailable` with the text `the telemetry database is unavailable: try again later`. It has no reason word, and `harold.telemetry.calls` does not count it.

## Counter and log line

`harold.telemetry.calls` counts each declare, count, gauge, event and remove call:

- `prefix` is `chief` or a product.
- `kind` is `declare`, `count`, `gauge`, `event` or `remove`.
- `outcome` is `ok` or a reason word. `invalid` is the 12th reason word, so the counter has 13 outcome entries with `ok`.

Harold seeds the counter at 0 for every registry prefix plus `chief`, every kind and every outcome. Each refusal series has its 0 point. A refused call also writes the `telemetry call refused` log line (`info`) with `prefix`, `kind` and `reason`, and no value or text.

The dashboard tile "Agent signals: calls and refusals" shows the refusals of the last 24 hours. More than 10 in a day is an info note: a script uses a wrong prefix or loops.

## Mix-up check, not security

The CLI sends the caller's pane. It reads `TMUX_PANE` first and `REX_BLOCK` second (`caller_pane_from`, `harold/src/cli.rs`). Any local process can set these variables.

The prefix check stops a script or an agent from writing another role's signals by mistake. It does not stop a local process that lies. A local process that lies can write any prefix.

A run outside a pane, such as cron or CI, is refused with `no_pane`.

## Reading the numbers

Run the named query `agent-counter.sql` to see a declared counter, this week against last week. See the [named queries](../telemetry/queries/README.md).
