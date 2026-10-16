# Telemetry

Harold can send its traces, logs and metrics to an OpenTelemetry collector, so they show in the local HyperDX. The data answers two questions: how the pods are moving, and whether a reply from the phone reaches the agent it was meant for. Four HyperDX alerts come back to Harold, which types them into the chief's pane.

With the settings left out, Harold works and logs exactly as before.

## Problem

Which build slots are busy, and what has merged, is spread across git worktrees and each pod's `status.md`. Harold writes a few lines per phone reply to `harold.log`, but nothing says how long a reply took to reach a pane, or whether it reached the right one. The lines of one turn carry four unrelated ids: one when the reply is read, one when it is routed, one when the turn ends, one when the update is sent.

The local stack already runs a collector that accepts OTLP, the standard format for traces, logs and metrics, and stores what it receives in ClickHouse, where HyperDX shows it.

## Settings

Add a `[telemetry]` section to `local.toml`:

```toml
[telemetry]
otlp_endpoint = "http://localhost:4317"
alert_listen = "127.0.0.1:4320"
pod_registry = "~/Dev/p/chief-of-pods/products.toml"
```

| Key | Turns on | When left out |
|---|---|---|
| `otlp_endpoint` | Export of spans, log records and metrics to the collector, over OTLP gRPC. Plain `http://` only: Harold is built without TLS, so an `https://` address is refused | Nothing is exported |
| `alert_listen` | The alert address, `POST /alert` on this address and port | No address is opened |
| `pod_registry` | The pod facts, read from the chief's `products.toml` | No pod folder is read and no `git` command runs |

Each key is off on its own. A malformed value stops Harold at start, and `harold --check-config` fails, with an error naming the key:

```
telemetry.otlp_endpoint must be an http:// address such as http://localhost:4317
telemetry.alert_listen must be an address and port such as 127.0.0.1:4320
telemetry.pod_registry must be the path of the chief's products.toml
```

A well-formed alert address that cannot be listened on, such as a port another program already holds, does not stop Harold. Harold logs `alert address unavailable` with `reason = bind_failed` and runs with the alert address off until it restarts. Every other part of Harold, telemetry included, runs as usual.

Every export carries the resource attributes `service.name = "harold"` and `harold.machine = <machine_name>`, so the data of PasM4MBP and PasM1Mac stays apart. Set `pod_registry` on one Mac only. Both Macs reading the same pods would count every merge twice.

`harold.log` is written as today, with telemetry on or off: the same lines, with message text still in them.

## Pod facts

Every minute Harold reads `products.toml` and, for each product, its base checkout and pod folder. It never writes there.

Each `[products.<name>]` row gives the base checkout (`repo`), the base branch (`base`) and `slots`, 3 when left out. Its other keys are ignored.

The `git` commands are read-only: they run with `GIT_OPTIONAL_LOCKS=0`, at most once a minute per product, and each gets 5 seconds. A product whose facts cannot be read logs `pod facts unavailable` with a fixed reason, such as `git gave no answer` or `pod folder unreadable`.

### Slots

**A busy slot** is a git worktree of the product's repository, other than the base checkout, that holds `.superpowers/baton/agent-pane`. Baton writes that file when it starts a worker, and removes the worktree when the worker is finished. A worktree made by hand is not a slot.

**Free slots** are `slots` minus busy slots, never below 0.

### Merges

**A merge** is a move of the base branch's tip, seen in the base checkout's reflog, to a commit that contains the old tip: a fast-forward or a merge commit. Harold checks each reflog step on its own, so two merges in one minute count as two.

- Its lines are the added and removed lines of `git diff --shortstat <old> <new>`.
- A step to a commit that does not contain the old tip, such as a rewrite, is not counted. It logs `base branch rewritten; not counted`. A rewrite between two merges in one minute leaves both merges counted.
- When Harold starts, it takes the current tips as known, so merges made while it was stopped are not counted.
- `spec.id` is set when the reflog entry reads `merge <id>`.

### Spec states

Each pod keeps its specs in `<base checkout>/.superpowers/pod/`. That folder is not in git, so git holds no dates for specs or approvals. Harold reads each state's time from file creation times and from the base branch.

| State (`state`) | A spec is in it when | Its time comes from |
|---|---|---|
| `drafted` | `specs/<id>.md` exists and `specs/<id>.url` does not | the creation time of `specs/<id>.md` |
| `waiting_for_approval` | `specs/<id>.url` exists: pod-coordination writes it when it first publishes the spec | the creation time of `specs/<id>.url` |
| `approved` | `approvals/<id>.md` exists, written by the chief | that file's creation time |
| `building` | a busy slot's worktree is named `<id>` | the creation time of its `.superpowers/baton/agent-pane` |
| `merged` | the base branch's reflog has `merge <id>` | that reflog entry's own time, when the merge happened, not the commit's time |
| `rejected` | `rejections.md` has a line `- <id>, <date>` | that date, from the start of the day |

A later state wins: a merged spec is merged even though its approval file remains. A file copied or restored loses its creation time, and Harold then uses the time it first sees the file.

**A revision** is published to the same link, so `specs/<id>.url` is not created again and "spec ready" stays at the first publish. Time spent revising counts as waiting for the user. The number of revisions comes from the `## Revision <n>` headings in `requests/<id>.md` and is carried as `spec.revisions`.

**Repositories with no reflog**, such as `turso-object-store` today, cannot show `merge <id>`. There a spec counts as merged when its busy worktree goes and the commit that worktree last had checked out is in the base branch. Harold asks `git merge-base --is-ancestor` for up to an hour after the worktree goes. This has limits:

- The remembered commit is kept in memory only. A Harold restart between the worktree's removal and the check loses that merge.
- A worktree removed with no commits of its own counts as merged, because its commit is already in the base branch. So does one whose `agent-pane` file is removed while the worktree stays.
- A squash or rebase merge makes new commits, so the remembered commit is never found and the merge is not seen.

**Spec size** is the word count of `specs/<id>.md`, leaving out lines that start with `<`, which hold drawn charts.

**Start to completion** (`pod.spec.lead_time`) is recorded once, when this Harold run sees a spec merge, in two phases (`phase`):

- `waiting_for_user`: from "spec ready" to approval;
- `build`: from approval to merge, including any wait for a free slot.

## Phone requests

**A phone request** is one reply from the user's phone that this Mac handles.

- **Start:** the reply's own time, as stored in the Messages database or sent by Telegram.
- **End:** Harold has typed it into a pane and pressed Enter, before the confirmation is sent back.
- **Latency:** end minus start.

Each counted request has one outcome:

| Outcome | When |
|---|---|
| `typed` | It was typed into a pane within 10 minutes of its start |
| `unavailable` | Harold found no pane and dropped it, gave its delivery up after 10 failed tries, or typed it more than 10 minutes after its start. A reply read late because the Messages database was blocked counts here when it is typed late |
| `wrong` | It was typed on time, and the next phone reply, within 10 minutes, was tagged for a different pane. It is counted when that tagged reply arrives |

`harold.phone_requests` counts `typed` and `unavailable`. `harold.phone_requests.wrong` counts `wrong` separately, against the earlier request's channel and `resolved_by`. Availability is typed divided by all requests. The right-agent rate is typed minus wrong, divided by all requests.

These rules decide `wrong`:

1. Only a request typed on time can later count as wrong. A late or dropped request never does.
2. "The next phone reply" is literal. Any counted reply replaces or clears the earlier one, including a reply that found no pane. A reply another Mac owns, or one that arrives while messaging is paused, leaves it alone.
3. Only a request whose pane Harold chose can count as wrong: `resolved_by` of `latest_notifier`, `ai_model` or `my_agent`. A request the user tagged (`exact_tag`, `tag_substring`) never counts as wrong.
4. A reply tagged for another Mac (`[PasM1Mac/pane] …` or `[PasM1Mac] …`) is a tag for a different pane. The Mac that typed the earlier request counts that request as wrong under rules 1 and 3, within 10 minutes. The tagged reply itself is counted only by the Mac it names, and, as rule 2 says, it leaves the earlier request in place. A reply with no machine in its tag that another Mac owns because it sent the latest notification counts nothing here.
5. A request is counted as wrong at most once. A second tagged reply within the 10 minutes, for another Mac or for another pane here, does not count it again.

`resolved_by` says how the pane was chosen:

| Value | The pane came from |
|---|---|
| `exact_tag` | the reply's tag matching an agent's tag exactly |
| `tag_substring` | the reply's tag being part of an agent's tag |
| `ai_model` | the AI model, asked to pick the pane from the reply |
| `latest_notifier` | the agent that last sent an update |
| `my_agent` | the fallback agent for this Mac |
| `none` | no pane |

`channel` says where the reply came from:

| Value | Meaning | Start |
|---|---|---|
| `imessage` | a reply read from the Messages database | the message's own time |
| `telegram` | a reply from Telegram | Telegram's time for it |
| `forwarded` | a reply another Mac forwarded to this one | when it was stored here |
| `unknown` | a reply with no open request, for example one stored before a restart | when it was stored |

**Not counted:** replies another Mac owns, which that Mac counts, and replies routed while messaging is paused. Their span is still exported, with outcome `not_owned` or `paused`. A reply Harold read but did not store carries `ignored`. These three outcomes appear on spans only, never on the metrics.

A reply whose payload failed for good never finishes, and is dropped after an hour; its span is then exported with no outcome. A reply whose delivery was given up after 10 failed tries is one Harold dropped: its request finishes at once as `unavailable`, and `phone-request-unavailable` fires for it.

## Turns

**A turn** is one prompt to an agent and its answer, ending when the agent stops: the `Stop` hook for Claude Code, its counterpart for Codex. The turn's start is the first of these that is known:

1. The time of the prompt in the agent's transcript, which the hook sends as `turn_started_at`.
2. When Harold typed the prompt itself: a phone reply or a timed prompt.
3. When the agent monitor first saw the pane busy after the previous turn ended.
4. Otherwise unknown. The span then starts at the turn's end and carries `turn.start = unknown`.

`started_by` is `phone`, `timed_prompt`, `desk` or `unknown`. A prompt Harold typed after the turn's end reached Harold belongs to the next turn, so it does not set `started_by`. A turn started by an alert typed into the chief's pane is not marked as such: `started_by` has no alert value.

**A session** (`session.id`) is the agent's own session id, which the hooks send. Where none is given, the pane id stands in and the span carries `session.source = pane`.

**One trace per turn.** A turn that starts with a phone reply is one trace, with these spans in order:

| Span | From – to | Attributes |
|---|---|---|
| `harold.phone_request` | the reply's start – typed | `channel`, `outcome`, `resolved_by`, `pane_id` |
| `harold.route`, inside it | choosing the pane, including the AI model when asked | `resolved_by` |
| `harold.reply_stored` | storing the reply, so the stored event records the trace | |
| `harold.turn` | the turn's start – the stop hook reached Harold | `pane_id`, `session.id`, `started_by`, and `session.source`, `turn.start` when they apply |
| `harold.turn_complete` | the hook's call handled and its event stored | `pane_id` |
| `harold.delivery`, and `harold.voice_note` inside it | the turn-end update sent or said | `kind`, `channel`, `outcome` |

`harold.delivery`'s `outcome` is `delivered`, `skipped`, `failed` (a send that failed before its 10th try, which is tried again, or a malformed payload, which is not) or `given_up` (its 10th failure; it is not tried again).

Every log record written inside one of these spans carries its trace id and span id, so HyperDX opens a turn's logs from its trace and finds the trace from any of its lines.

Harold keeps the trace across its steps in memory:

- The stored reply event records the trace id, and the event handler continues that request.
- For an hour after typing a prompt, Harold keeps the trace for the pane, and the next turn end on the pane joins it.
- The gRPC handler keeps each turn-end event's trace, by event id, for its delivery.

These maps live in memory only, and drop entries older than an hour. After a restart, a turn-end update still waiting to be delivered for an earlier turn starts its own trace.

## Metrics

Metrics are sent every 30 seconds. Every metric also carries `harold.machine`.

| Metric | Type, unit | Attributes |
|---|---|---|
| `pod.slots.busy`, `pod.slots.free` | gauge, `{slot}` | `pod.product` |
| `pod.merges` | counter, `{merge}` | `pod.product`, `spec.id` when the reflog names one |
| `pod.merged_lines` | counter, `{line}` | `pod.product`, `change` (`added`, `removed`) |
| `harold.phone_requests` | counter, `{request}` | `channel`, `outcome` (`typed`, `unavailable`), `resolved_by` |
| `harold.phone_requests.wrong` | counter, `{request}` | `channel`, `resolved_by` |
| `harold.phone_request.latency` | histogram, `s`, buckets 1, 2, 5, 10, 30, 60, 120, 300, 600 | `channel`, `resolved_by` |
| `harold.turns` | counter, `{turn}` | `session.id`, `pane_id`, `started_by` |
| `harold.turn.duration` | histogram, `s`, buckets 10, 30, 60, 120, 300, 600, 1800, 3600 | `started_by` |
| `pod.specs` | gauge, `{spec}` | `pod.product`, `state` |
| `pod.spec.words` | gauge, `{word}` | `pod.product`, `spec.id`, `state` |
| `pod.spec.lead_time` | histogram, `s`, buckets 600, 1800, 3600, 7200, 14400, 43200, 86400, 172800 | `pod.product`, `phase` (`waiting_for_user`, `build`), `spec.revisions` |

`pod.specs` reports every state of every product, 0 included, so a card can read 0.

`harold.turns` keeps one series per session (`session.id`, `pane_id`, and each `started_by` seen) for the life of the Harold process, so a long-running Harold accumulates series until it restarts. Past the SDK's limit of 2000 series per instrument, new sessions fall into one overflow series, marked `otel.metric.overflow = true`.

## What leaves Harold

No exported item carries what the user or an agent wrote, a voice line, the AI model's output, an error message, the recipient or a token.

**Allowed fields.** Exported spans and log records keep only the attributes on this list, `ALLOWED_FIELDS` in `harold/src/telemetry.rs`:

```
pane_id, pane, session.id, session.source, component, reason, outcome, channel,
resolved_by, kind, started_by, turn.start, state, alert, pod.product, spec.id,
spec.state_at, spec.words, spec.revisions, phase, change, event_type,
event_version, result
```

Every other field is dropped before export, including `text`, `msg`, `raw_output`, `tag` and `error`. Span events and links are dropped too, and an error status loses its description.

**Which log records are exported.** Only records whose target is `harold`, starts with `harold::`, is `events` or starts with `events::`. Other crates' lines, such as Turso's and reqwest's, interpolate errors and paths into their text, so they stay in `harold.log` only. The collector-health lines also stay in `harold.log` only.

**Log bodies** are fixed text. A test scans every log call in `harold/src` and `events/src` and fails on one that puts a value into its message, apart from a short list of fixed reasons, migration names and a path.

## When the collector is down

Export never slows Harold:

- Spans, log records and metrics are queued in memory and sent in batches by a background task. A full queue drops the newest item.
- Each send gives up after 5 seconds.
- Shutdown waits at most 2 seconds for queued data.
- An unreachable collector logs one `telemetry export failing (collector unreachable at <endpoint>)` line, and one `telemetry export recovered` line when sends work again. It never logs one line per batch.

An `otlp_endpoint` that cannot be used at all logs `telemetry export off: the collector address could not be used`, and Harold runs with export off.

## The dashboards

The definition files are in [`hyperdx/`](hyperdx/), in the shape HyperDX 2.40's external API takes. [Creating the definitions](#creating-the-definitions) gives the commands.

### Harold: the cards

`dashboard.json` is the "Harold" dashboard. It holds the cards in four groups, and a product filter. Each card is two number tiles side by side: the card's number over the last 24 hours, and beside it the same number over the 24 hours before, named "<card>, previous 24 h". "Lines merged, last hour" reads the last hour, and its "previous hour" tile reads the hour before.

Each tile's window is written into its query: `TimeUnix >= now() - INTERVAL 24 HOUR` for a current tile, and `TimeUnix >= now() - INTERVAL 48 HOUR AND TimeUnix < now() - INTERVAL 24 HOUR` for an earlier one. Open the dashboard with a time range of at least the last 48 hours. A shorter range cuts the tiles down to it.

| Group | Card | The number | How the tile reads it |
|---|---|---|---|
| Pods | Free slots, average | `pod.slots.free` averaged over the period | builder: average |
| Pods | Busy slots, p95 | the 95th percentile of `pod.slots.busy` over the period | builder: 95th percentile |
| Pods | Merges to main | `pod.merges` in the period | SQL: the counter rule |
| Pods | Lines merged | `pod.merged_lines` in the period | SQL: the counter rule |
| Pods | Lines merged, last hour | `pod.merged_lines` in the last hour | SQL: the counter rule over the last hour |
| Phone requests | Requests | `harold.phone_requests` in the period | SQL: the counter rule |
| Phone requests | Availability | typed divided by all requests | SQL: the counter rule, typed rises over all rises |
| Phone requests | Right agent | typed minus wrong, divided by all requests | SQL: the counter rule, over both counters |
| Phone requests | Latency, p95 | the 95th percentile of the phone-request latency | SQL: the 95th percentile of the duration of `harold.phone_request` spans that were typed (they carry `pane_id`) |
| Turns | Turns | `harold.turns` in the period | SQL: the counter rule |
| Turns | Turns per session, median | the median of turns per `session.id` | SQL: `harold.turn` spans counted per `session.id`, one span per turn |
| Specs | Waiting for approval | `pod.specs` in that state at the end of the period | SQL: the last value per product, summed |
| Specs | Merged | specs that reached merged in the period | SQL: specs whose first `pod.spec.words` sample in state `merged` falls in the period |
| Specs | Spec size, median words | the median `pod.spec.words` of specs first ready in the period | SQL: specs whose first sample past `drafted` falls in the period |
| Specs | Waiting for you, p50 | the median `pod.spec.lead_time`, phase `waiting_for_user` | SQL: the counter rule over the histogram's count and sum |
| Specs | Build, p50 | the median `pod.spec.lead_time`, phase `build` | SQL: the counter rule over the histogram's count and sum |

Fourteen of the sixteen cards are raw SQL tiles. Only the two slot cards use HyperDX's builder. The SQL tiles read ClickHouse through the connection, for three reasons:

- HyperDX's builder cannot express some cards, such as a median of per-session counts.
- HyperDX's own counter reading loses the first point of every new series ([Counters in HyperDX](#counters-in-hyperdx)).
- A card's number can be computed for any window, so each card can show its earlier number.

"Merged" and "Spec size" look back as far as ClickHouse keeps data, 30 days in the local stack. So on the first day the pod facts are on, every spec already merged counts as merged that day, and every spec already past `drafted` counts as first ready that day.

The latency card reads spans, not the `harold.phone_request.latency` histogram. A typed request's `harold.phone_request` span runs from the reply's own time to Enter, which is exactly its latency, and spans have no first-point loss.

**The product filter** is named "Product (pod and spec cards only)". It applies to the pod and spec cards only. The phone-request and turn tiles ignore it, because those metrics and spans carry no product: they are SQL tiles that leave out the `$__filters` macro, through which a SQL tile takes a filter, or they read the traces source, which the filter does not apply to. With all products, a pod card reads the products together: "Free slots, average" averages harold's and turso-object-store's numbers.

**How a card shows its change.** The spec asks each card to show, under its number, its change against the previous period: the last 24 hours against the 24 hours before.

| Card kind | The change reads |
|---|---|
| A count, a sum or an average | `▲ 50% vs 2`: the change as a share of the earlier value, rounded to whole percent; under 0.5% it reads `no change` |
| A percentage (availability, right agent) | `▲ 5.0 pts vs 95.0%`: the difference in percentage points, never a percent of a percent |
| A percentile or a duration (latency p95, waiting and build p50) | `▼ 7.8 s (48%) vs 16.3 s`: the difference in the card's unit, then as a share of the earlier value |

"Lines merged, last hour" compares the last hour with the hour before. Arrows show direction only, without colour, because a rise is good for some cards and bad for others.

Special cases:

- The earlier value is 0 and the current one is not: `new (was 0)`.
- Both are 0: `no change`.
- The earlier period has no data, because Harold was not running, the setting was off, or nothing of that kind happened: `no earlier data`.
- The current period has no data: the number reads `–` and the line reads `no data in this period`.
- A percentile from fewer than 5 values in either period shows its number, and the change reads `too few to compare`.

**HyperDX 2.40 number tiles cannot show a change line.** Only its line tiles have `compareToPreviousPeriod`. So each card shows the earlier number beside the current one, the spec's fallback, and the reader compares the two. The change line and the special cases above are what a card should show once HyperDX's number tiles can compare.

### Harold trends

`trends.json` is the "Harold trends" dashboard, with one panel each:

| Panel | The question | Behind it | The tile |
|---|---|---|---|
| Slots | How often is a slot free, and how many are busy at the busy times? | `pod.slots.busy` over time per product; per day, its 95th percentile and the share of time with at least one slot free | SQL line, per product: busy, busy p95, and the share of samples with `pod.slots.free` at 1 or more. Set the dashboard's granularity to a day for the per-day reading |
| Merges to main | How much landed on main, and how much in the last hour? | `pod.merges` per day; `pod.merged_lines` summed over the last hour, and per merge | SQL line, the counter rule: merges, lines merged, and lines per merge |
| Phone requests reaching the right agent | When I reply from the phone, does it reach the agent I meant? | `harold.phone_requests` and `.wrong` per day: right, wrong, unavailable; availability and right-agent rate | builder line: typed, unavailable, wrong, availability and right agent |
| Phone request latency | How long from my reply to the agent seeing it? | `harold.phone_request.latency`, median and 95th percentile | builder line over the typed `harold.phone_request` spans, as the latency card |
| One turn, end to end | What happened between my reply and the update on my phone? | the list of `harold.turn` traces, newest first; each opens as a waterfall with its logs | search over the traces source |
| Turns per session | Which sessions do the most work? | `harold.turns` grouped by `session.id`, and per day | SQL stacked bars per `session.id`, the counter rule |
| Specs by state | How many specs are where, in each pod? | `pod.specs` per product and state, over time | builder line per product and state |
| Spec size | How big are the specs, and does a bigger spec mean a bigger change? | `pod.spec.words` per spec, with `pod.merged_lines` for the merged ones | builder table of words per product, spec and state |
| Start to completion | How long from a spec being ready to it being on main, and how much of that is waiting for me? | `pod.spec.lead_time` per phase, p50 and p95 | SQL table per product and phase, the counter rule over the histogram's count and sum |

The SQL panels read the time range's points and, for the counter rule, the day before it.

The "Spec size" panel cannot show lines merged per spec: `pod.merged_lines` carries `pod.product` and `change` but no `spec.id`, so the two cannot be joined. Read a merged spec's lines from "Merges to main" at the time of its merge.

### Counters in HyperDX

Harold sends counters and histograms as running totals, the OpenTelemetry default. HyperDX turns a running total into increases by taking each point's difference from the series' previous point. It counts a series' first point as 0, because there is no previous one. The local stack's own provisioning notes the same undercount. Two things work around it.

**Counting from 0 at start.** With export on, Harold records 0 once, at start, for every attribute set of the counters the alerts read:

- `harold.phone_requests`, for each `outcome` (`typed`, `unavailable`), `channel` and `resolved_by`;
- `harold.phone_requests.wrong`, for each `channel` and `resolved_by`.

The pod facts do the same for each product on its first poll: `pod.merges` with no `spec.id`, and `pod.merged_lines` for each `change`. Each series then has a first point of 0 before anything happens, and the first real request after a restart is a rise HyperDX can see.

This cannot cover every series:

- A counter keyed by `session.id` (`harold.turns`) or by `spec.id` (a merge naming a spec) gets a new series for each new value, which cannot be known at start.
- A histogram cannot be given a 0 point, because recording a value adds an observation. So nothing reads latency from the `harold.phone_request.latency` histogram: the latency card, panel and the `phone-request-slow` alert read the typed `harold.phone_request` spans.

**The counter rule.** So every card and panel that counts a running total uses raw SQL with its own rule instead of HyperDX's. Each series is one metric with one set of resource attributes and attributes. Its points are read in time order, from one window length before the window (a day before for the trends panels). Each point rises by:

| Case | Rise |
|---|---|
| the point is at or above the series' previous point | its value minus the previous point |
| the series has no previous point: it first appears, such as a merge naming a new `spec.id` | its whole value, counting from 0 |
| the point is below the previous point: the counter reset when Harold restarted | its whole value, counting from 0, never negative |

The card's number is the sum of the rises of the points in its window. In SQL:

```sql
SELECT sum(rise) FROM (
  SELECT TimeUnix, if(Value >= before, Value - before, Value) AS rise
  FROM (
    SELECT TimeUnix, Value,
      lagInFrame(Value, 1, toFloat64(0)) OVER (
        PARTITION BY cityHash64(MetricName, ResourceAttributes, Attributes)
        ORDER BY TimeUnix ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS before
    FROM $__sourceTable(sum)
    WHERE MetricName IN ('pod.merges') AND TimeUnix >= <lookback> AND TimeUnix < <end>
  )
) WHERE TimeUnix >= <start>
```

`lagInFrame(..., toFloat64(0))` gives a series' first point a previous value of 0. A point written twice rises by 0 the second time. The test `the_counter_rule_counts_rises_new_series_and_resets_from_zero` in `harold/src/telemetry_tests.rs` runs the same rule in Rust on sample series: a rise, a new series, and a reset.

The lead-time cards and the "Start to completion" panel apply the rule to the histogram's count and sum, judging a reset by the count. Each rise in the count, with its rise in the sum, gives the lead times recorded since the previous point. Their median is weighted by the count's rise. Merges are minutes apart and Harold exports every 30 seconds, so a rise holds one merge. Two merges within one export would stand as their mean.

Two limits remain:

- A series whose previous point is older than the lookback counts from 0. After a restart that is right, because the counter starts again at 0. If the collector was unreachable for longer than the lookback while Harold kept running, the rise during the outage is counted when export resumes.
- The builder panel "Phone requests reaching the right agent" and the two counter alerts, `phone-request-unavailable` and `wrong-agent`, still use HyperDX's reading. They depend on the counting from 0 above.

## The alerts

| Alert | Fires when | HyperDX definition |
|---|---|---|
| `phone-request-unavailable` | Any phone request dropped with no pane or after 10 failed tries, or not typed within 10 minutes | sum of `harold.phone_requests` with `outcome = unavailable` above 0, every 5 minutes |
| `phone-request-slow` | The 95th percentile latency is above 60 seconds in an hour with at least 3 requests | 95th percentile of the duration of typed `harold.phone_request` spans above 60 seconds, every hour, on the traces source |
| `wrong-agent` | A request is counted as wrong | sum of `harold.phone_requests.wrong` above 0, every 5 minutes |
| `slot-idle` | A slot is free for more than 2 hours in a row between 08:00 and 23:00 local | for each product, the last sample of each 15-minute window has at least 1 slot free, for 8 windows in a row |

Their basis is the period the spec drew, 11 to 15 October:

- `phone-request-unavailable` would never have fired: none was dropped, and the slowest took 37 seconds.
- `phone-request-slow` would never have fired: the median was 6 seconds, the 95th percentile 11 seconds, and the highest hourly 95th percentile 37 seconds.
- `wrong-agent` would have fired once, on 14 October at 08:42.
- `slot-idle` would have fired every day: at most 2 of harold's 3 slots were ever busy at once. This is the gap the keep-3-busy rule is about.

Merges, turns per session and the spec facts have no alert. They describe throughput, and no level of them is wrong in itself.

Each alert is an inline alert: it carries its own query, so it needs no dashboard tile. A tile alert would need a tile with exactly the alert's query, and the cards dashboard holds the cards only.

`phone-request-slow` reads spans, as the latency card does. A typed request's `harold.phone_request` span carries `pane_id` and runs from the reply's own time to Enter, so its duration is the request's latency. Spans have none of the counters' first-point loss, so the first request after a restart counts.

HyperDX 2.40 cannot express three parts of these conditions:

- **10 minutes.** Its intervals are 1, 5, 15 and 30 minutes, 1, 6 and 12 hours, and 1 day. A count above 0 is caught by any window that holds the request, so the 5-minute windows catch every request a 10-minute window would, and sooner. The 10 minutes in `phone-request-unavailable` is the outcome's own rule, already applied by Harold.
- **At least 3 requests.** An alert has one condition, so `phone-request-slow` fires on the 95th percentile alone. The chief applies "at least 3 requests" by checking the count on the trends dashboard before it tells anyone.
- **08:00 to 23:00.** An alert has no time-of-day window, so `slot-idle` fires at any hour. The chief ignores a `slot-idle` line outside 08:00 to 23:00 local.

`slot-idle` is grouped by `pod.product`, so a free slot in one product is not hidden by a full one in another. Each product fires and clears on its own, and each sends its own post. HyperDX 2.40 reduces a gauge to its last sample in each alert window, so a slot taken and freed again within 15 minutes, between two samples, does not break the run.

## How an alert reaches the chief

Each alert has its own generic HyperDX webhook, `webhook-<name>.json`, named `harold-alerts-<name>`. The alert's name is written into the webhook's body, because HyperDX 2.40 leaves `note` out of what a webhook body can see, so `{{note}}` renders as an empty string. Each webhook:

- posts to `http://host.containers.internal:4320/alert`, Harold's `alert_listen` port as the stack's containers see this Mac with Podman. With Docker, change the host to `host.docker.internal`. The port must match `alert_listen`.
- has the body `{"alert": "<name>", "state": "{{#if (eq status 'firing')}}firing{{else if (eq status 'resolved')}}cleared{{else}}{{status}}{{/if}}"}`. HyperDX's `status` `firing` is sent as `firing`, and `resolved` as `cleared`. Any other status (`no_data`, `pending`, `error`) is passed through as it is, so Harold answers 400 and types nothing. HyperDX 2.40 documents posts only for firing and resolving, but the body does not rely on that.

When the port is taken at start, Harold logs `alert address unavailable` (`reason = bind_failed`) and takes no alerts until it restarts with the port free. HyperDX's posts then fail, and no line reaches the chief.

Harold answers:

- `204` for a post naming one of the four alerts with state `firing` or `cleared`. It then types exactly `harold alert: <name> <state>` into the pane of the agent running in the chief's folder, `[notify] away_messages_only_from`.
- `400` for anything else, including another path or method. It logs `alert ignored (unknown alert)` and types nothing.

Harold types one line of its own the same way: `harold alert: delivery-given-up firing`, when it gives up a delivery after 10 failed tries (see the [notification reference](../notification/README.md#architecture)). It carries no message text. It is typed only when `alert_listen` is set and the address opened, with the checks and the 10 minutes below, as a posted line is. A post naming `delivery-given-up` is answered `400`.

Harold also types `harold alert: store-failing firing` when it marks its store failing, and `harold alert: store-failing cleared` when the store recovers (see the [operation reference](../operation/README.md)). The two are typed under the same conditions as the `delivery-given-up` line. `cleared` follows only a `firing` line: a spell that began before the alert address opened is not cleared. Harold also logs the two changes, as `store failing` and `store recovered`, with only the fields `component = "store"` and `reason`. A post naming `store-failing` is answered `400`.

No log line carries any part of a posted body.

Typing follows the timed prompts' checks: the pane must be idle and its input box empty. Harold tries every 5 seconds. A line is dropped, and logged as `alert not delivered (<reason>)`, when:

- no agent runs in the chief's folder (`no_chief`);
- the chief's pane stays busy for 10 minutes, counted from the post, or for Harold's own line from when it was queued (`agent_busy`), or another check keeps failing that long. Lines are typed one at a time, so a line queued behind one that waited may find its 10 minutes already up;
- the queue of 32 lines waiting to be typed is full (`queue_full`). The post is still answered 204.

One writer at a time types into a pane. Alerts, timed prompts and phone replies share a lock per pane around typing and Enter. Alerts and timed prompts also treat a pane as busy for a minute after anyone typed into it, because the agents view can still read idle just after typing.

What the chief does with the line is the chief's own instruction.

## Creating the definitions

These commands create or update the definitions in HyperDX through its external API. They need an API key and the API's address:

```sh
export HYPERDX_URL=http://localhost:8000      # the external API v2 of the local stack
export HYPERDX_API_KEY=...                    # a HyperDX API access key
hdx() { curl -sS -H "Authorization: Bearer $HYPERDX_API_KEY" -H "Content-Type: application/json" "$@"; }
```

The files name HyperDX objects by placeholders, because their ids differ in each HyperDX. Look the ids up by name. The local stack's provisioning names them `clickhouse`, `Metrics` and `Traces`:

```sh
export CLICKHOUSE_CONNECTION_ID=$(hdx "$HYPERDX_URL/api/v2/connections" | jq -r '.data[] | select(.name == "clickhouse") | .id')
export METRICS_SOURCE_ID=$(hdx "$HYPERDX_URL/api/v2/sources" | jq -r '.data[] | select(.name == "Metrics") | .id')
export TRACES_SOURCE_ID=$(hdx "$HYPERDX_URL/api/v2/sources" | jq -r '.data[] | select(.name == "Traces") | .id')

fill() {
  sed -e "s/@CLICKHOUSE_CONNECTION_ID@/$CLICKHOUSE_CONNECTION_ID/g" \
      -e "s/@METRICS_SOURCE_ID@/$METRICS_SOURCE_ID/g" \
      -e "s/@TRACES_SOURCE_ID@/$TRACES_SOURCE_ID/g" \
      -e "s/@WEBHOOK_ID@/$WEBHOOK_ID/g" "docs/references/telemetry/hyperdx/$1"
}

# The id of the one webhook or alert named $1. Nothing if none has the name; a failure if more than one does,
# or if HyperDX cannot be reached or does not answer with a list.
webhook_id() { named webhooks "$1"; }
alert_id() { named alerts "$1"; }
named() {
  _list=$(hdx "$HYPERDX_URL/api/v2/$1") || return
  _ids=$(printf '%s' "$_list" | jq -n -r --arg name "$2" 'input | .data[] | select(.name == $name) | .id') || return
  _count=$(printf '%s' "$_ids" | grep -c .)
  [ "$_count" -le 1 ] || { echo "$_count $1 are named $2; delete all but one" >&2; return 1; }
  printf '%s' "$_ids"
}
```

Run them from the repository root.

**Create** the dashboards:

```sh
for file in dashboard.json trends.json; do
  fill "$file" | hdx -X POST "$HYPERDX_URL/api/v2/dashboards/validate" --data-binary @-   # HyperDX's own check
  fill "$file" | hdx -X POST "$HYPERDX_URL/api/v2/dashboards" --data-binary @- | jq -r '.data.id // .id'
done
```

Then create each alert's webhook and the alert pointing at it. A webhook that already exists is reused. An alert that already exists is left for the update commands, so a second run creates nothing:

```sh
for name in phone-request-unavailable phone-request-slow wrong-agent slot-idle; do
  WEBHOOK_ID=$(webhook_id "harold-alerts-$name") || break
  if [ -z "$WEBHOOK_ID" ]; then
    WEBHOOK_ID=$(fill "webhook-$name.json" | hdx -X POST "$HYPERDX_URL/api/v2/webhooks" --data-binary @- | jq -r '.data.id // .id')
    [ -n "$WEBHOOK_ID" ] && [ "$WEBHOOK_ID" != null ] || { echo "webhook harold-alerts-$name not created" >&2; break; }
  fi
  id=$(alert_id "$name") || break
  [ -z "$id" ] || { echo "alert $name already exists; use the update commands" >&2; continue; }
  fill "alert-$name.json" | hdx -X POST "$HYPERDX_URL/api/v2/alerts" --data-binary @- | jq -r '.data.id // .id'
done
```

**Update** an existing definition with `PUT` to its id. Find the ids by name:

```sh
dashboard() { hdx "$HYPERDX_URL/api/v2/dashboards" | jq -c --arg name "$1" '.data[] | select(.name == $name)'; }

# "Harold": an update must keep the product filter's id. The tiles keep theirs by the ids in the file.
old=$(dashboard "Harold")
fill dashboard.json \
  | jq --argjson old "$old" '.filters[0].id = ($old.filters[] | select(.name == "Product (pod and spec cards only)") | .id)' \
  | hdx -X PUT "$HYPERDX_URL/api/v2/dashboards/$(echo "$old" | jq -r .id)" --data-binary @-

# "Harold trends" has no filters.
fill trends.json | hdx -X PUT "$HYPERDX_URL/api/v2/dashboards/$(dashboard "Harold trends" | jq -r .id)" --data-binary @-
```

Then update each alert's webhook and the alert. The loop looks up both ids before it sends either PUT, and stops at the first name whose webhook or alert is missing or is named twice:

```sh
for name in phone-request-unavailable phone-request-slow wrong-agent slot-idle; do
  WEBHOOK_ID=$(webhook_id "harold-alerts-$name") || break
  [ -n "$WEBHOOK_ID" ] || { echo "no webhook harold-alerts-$name: create it first" >&2; break; }
  id=$(alert_id "$name") || break
  [ -n "$id" ] || { echo "no alert $name: create it first" >&2; break; }
  fill "webhook-$name.json" | hdx -X PUT "$HYPERDX_URL/api/v2/webhooks/$WEBHOOK_ID" --data-binary @-
  fill "alert-$name.json" | hdx -X PUT "$HYPERDX_URL/api/v2/alerts/$id" --data-binary @-
done
```

A HyperDX set up from the earlier single-webhook files has one webhook named `harold-alerts` and none of the four. Create the four with this loop, which skips any that already exist, then run the update loop above:

```sh
for name in phone-request-unavailable phone-request-slow wrong-agent slot-idle; do
  WEBHOOK_ID=$(webhook_id "harold-alerts-$name") || break
  [ -z "$WEBHOOK_ID" ] || continue
  fill "webhook-$name.json" | hdx -X POST "$HYPERDX_URL/api/v2/webhooks" --data-binary @- | jq -r '.data.id // .id'
done
```

`harold-alerts` is then unused and may be deleted.

## Not tested until real use

The definitions were written from HyperDX 2.40's API source and the local stack's own provisioning, and checked only by tests that read the files. Nothing was sent to the running stack. Untested until then:

- data arriving in HyperDX from either Mac;
- the dashboards, panels and alerts working as written in HyperDX 2.40, including the field names, the SQL tiles and the counter rule's SQL in ClickHouse;
- the counting from 0 at start reaching HyperDX as a first point of 0;
- the stack's containers reaching Harold's alert address, and each webhook's body arriving as written;
- the hooks' new fields on the installed agents.
