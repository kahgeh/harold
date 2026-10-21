# Telemetry

Harold can send its traces, logs and metrics to an OpenTelemetry collector, so they show in the local HyperDX. The data answers two questions: how the pods are moving, and whether a reply from the phone reaches the agent it was meant for. Six HyperDX alerts come back to Harold, which types them into the chief's pane.

With `export = false`, Harold works and logs exactly as before.

## Problem

Which build slots are busy, and what has merged, is spread across git worktrees and each pod's `status.md`. Harold writes a few lines per phone reply to `harold.log`, but nothing says how long a reply took to reach a pane, or whether it reached the right one. The lines of one turn carry four unrelated ids: one when the reply is read, one when it is routed, one when the turn ends, one when the update is sent.

The local stack already runs a collector that accepts OTLP, the standard format for traces, logs and metrics, and stores what it receives in ClickHouse, where HyperDX shows it.

## Settings

Export is on by default: `default.toml` sets `otlp_endpoint = "http://localhost:4317"`, so a Mac whose `local.toml` has no `[telemetry]` section sends to the local collector. To set export off, put this in `local.toml`:

```toml
[telemetry]
export = false
```

The other keys go in the same section of `local.toml`:

```toml
[telemetry]
otlp_endpoint = "http://localhost:4317"
alert_listen = "127.0.0.1:4320"
pod_registry = "~/Dev/p/<your-vault>/products.toml"
```

| Key | Turns on | When left out |
|---|---|---|
| `export` | Export, when true. False is export off: no pipeline is built, nothing is sent and no line is logged. `otlp_endpoint` is still checked | True: export is on |
| `otlp_endpoint` | The collector's address for export of spans, log records and metrics, over OTLP gRPC. Plain `http://` only: Harold is built without TLS, so an `https://` address is refused | `http://localhost:4317`, from `default.toml` |
| `alert_listen` | The alert address, `POST /alert` on this address and port | No address is opened |
| `pod_registry` | The pod facts, read from the organisation's `products.toml`, in its vault | No pod folder is read and no `git` command runs |

`alert_listen` and `pod_registry` are off on their own. A malformed value stops Harold at start, and `harold --check-config` fails, with an error naming the key:

```
telemetry.otlp_endpoint must be an http:// address such as http://localhost:4317
telemetry.alert_listen must be an address and port such as 127.0.0.1:4320
telemetry.pod_registry must be the path of the chief's products.toml
```

A well-formed alert address that cannot be listened on, such as a port another program already holds, does not stop Harold. Harold logs `alert address unavailable` with `reason = bind_failed` and runs with the alert address off until it restarts. Every other part of Harold, telemetry included, runs as usual.

Every export carries the resource attributes `service.name = "harold"` and `harold.machine = <machine_name>`, so the data of PasM4MBP and PasM1Mac stays apart. Set `pod_registry` on one Mac only. Both Macs reading the same pods would count every merge twice.

`harold.log` is written as today, with export on or off: the same lines, with message text still in them.

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
- `spec.id` and `spec.stage` come from the reflog entry `merge <name>: …`. When `specs/<name>.md` exists, `<name>` is the `spec.id` and there is no stage. Otherwise, when `<name>` reads `<id>-<stage>`, `specs/<id>.md` exists, and the stage is digits optionally followed by one lower-case letter (`1b`, `2`, `3a`), the merge is a stage of `<id>`: `spec.id` is `<id>` and `spec.stage` is the stage. Any other name is the whole `spec.id`, with no stage.

**The counted-tip file** holds, for each product, the base branch's tip that Harold has counted up to. It is `<store path>/../pod/tips`, which is `~/bin/harold/data/pod/tips` for the installed Harold. It has one `<product> <hash>` line per product and mode 0600. Harold writes it to `tips.tmp` and renames it.

- Harold writes the file first and records the counts and lead times after, so a crash can lose a poll's counts but never count them twice.
- When a write fails, Harold records nothing for that product until a write succeeds. It logs `pod merges and specs unavailable` once, with `reason="tip file unwritable"`.
- The first poll after a start counts the merges since the saved tip, so merges made while Harold was stopped are counted.
- A missing file, a line that does not parse, or a saved tip that the reflog does not hold before the current tip counts nothing. Harold starts from the current tip.
- After a rollback to a Harold built before 36a7c94, delete the file; see [Rollback](../operation/README.md#rollback).

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

**Start to completion** (`pod.spec.lead_time`) is recorded when Harold counts a merge, so once per merge, restarts included. It has two phases (`phase`):

- `waiting_for_user`: from "spec ready" to approval, recorded on the spec's first merge only (the oldest in the reflog);
- `build`: from approval to merge, including any wait for a free slot, recorded on each merge, stages included.

A merge whose spec has no approval file records no lead time, and neither does a merge on a base branch with no reflog, which has no spec.

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
4. A reply tagged for another Mac (`[PasM1Mac/pane] …` or `[PasM1Mac] …`) is a tag for a different pane. The Mac that typed the earlier request counts that request as wrong under rules 1 and 3, within 10 minutes. The tagged reply itself is counted only by the Mac it names, and, as rule 2 says, it leaves the earlier request in place. A reply with no machine in its tag that another Mac owns because it sent the latest phone notification counts nothing here.
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

`started_by` is `phone`, `timed_prompt`, `voice`, `desk` or `unknown`. A turn the call typed is `voice`. A prompt Harold typed after the turn's end reached Harold belongs to the next turn, so it does not set `started_by`. A turn started by an alert typed into the chief's pane is not marked as such: `started_by` has no alert value.

**A session** (`session.id`) is the agent's own session id, which the hooks send. Where none is given, the pane id stands in and the span carries `session.source = pane`.

**One trace per turn.** A turn that starts with a phone reply is one trace, with these spans in order:

| Span | From – to | Attributes |
|---|---|---|
| `harold.phone_request` | the reply's start – typed | `channel`, `outcome`, `resolved_by`, `pane_id` |
| `harold.route`, inside it | choosing the pane, including the AI model when asked | `resolved_by` |
| `harold.reply_stored` | storing the reply, so the stored event records the trace | |
| `harold.turn` | the turn's start – the stop hook reached Harold | `pane_id`, `session.id`, `started_by`, and `session.source`, `turn.start` when they apply |
| `harold.turn_complete` | the hook's call handled and its event stored | `pane_id` |
| `harold.delivery`, and `harold.voice_note` inside it | the turn-end update sent or said | `kind`, `channel`, `outcome`, `reason` for `held_back` |
| `harold.local_model`, inside it | the local model's run | `result` (`answered`, `failed`, `no_model`) |

**A turn you start by speaking in a call** is one trace, with these spans in order:

| Span | From – to | Attributes |
|---|---|---|
| `harold.call_turn` | the prompt typed – the update line arriving, or the wait giving up or the call ending | `pane_id` |
| `harold.turn`, and `harold.turn_complete` inside it | as above, with `started_by = voice` | as above |
| `harold.call_update`, and `harold.local_model` inside it | the turn-end update made, as the caller waits for it | none; `harold.local_model` has `result` |

`harold.call_turn` ends when the update line arrives, or when the wait gives up or the call ends. When the wait gives up, `harold.call_turn` ends before a late stop hook adds its `harold.turn`, so that child can outlive its parent. `harold.call_update`'s duration is how long the call makes the caller wait for the update. A `harold.local_model` span with no parent means a run escaped its turn.

`turn-end update fell back` (INFO, `reason` and `channel`) is written when Harold says the fallback sentence. A model that fails writes the WARN line `local model failed` with `reason` `spawn_failed` (`uv` not found or not runnable), `exit_status` (the model crashed or is not installed) or `empty_output`; the exit status stays in `harold.log`, which errors do not leave for HyperDX. No span, line or counter holds the sentence, the prompt or the model's text.

A watch run is a trace of its own: `harold.watch.run`, one run of a watch, with `harold.watch.model` (the Claude Code process, with `result`: `exited`, `killed` or `failed_to_start`) inside it. The run span has `watch`, `run`, `outcome` (`quiet`, `report`, `skipped` or a failure reason), `reason` and `cost_usd`, and the run's log lines carry its trace id.

`harold.delivery`'s `outcome` is `delivered`, `skipped`, `held_back` (a list or the chief rule kept the update from going out, with `reason` `not_listed` or `chief`, and `channel` the key of the list that held it), `failed` (a send that failed before its 10th try, which is tried again, or a malformed payload, which is not) or `given_up` (its 10th failure; it is not tried again).

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
| `pod.merges` | counter, `{merge}` | `pod.product`, `spec.id` when the reflog names one, `spec.stage` when the merge is a stage |
| `pod.merged_lines` | counter, `{line}` | `pod.product`, `change` (`added`, `removed`) |
| `harold.phone_requests` | counter, `{request}` | `channel`, `outcome` (`typed`, `unavailable`), `resolved_by` |
| `harold.phone_requests.wrong` | counter, `{request}` | `channel`, `resolved_by` |
| `harold.phone_request.latency` | histogram, `s`, buckets 1, 2, 5, 10, 30, 60, 120, 300, 600 | `channel`, `resolved_by` |
| `harold.turn_end_updates` | counter, `{update}` | `outcome` (`model`, `fallback`), `reason` (`none` for `model`, otherwise `no_model`, `model_failed`, `too_long` or `no_name`) |
| `harold.web_app.pushes` | counter, `{push}` | `outcome` (`accepted`, `gone`, `refused`, `unreachable`) |
| `harold.notifications` | counter, `{notification}` | `agent` (the pane's folder below the home folder, such as `Dev/p/harold/main`), `channel` (`imessage`, `telegram`, `web_app` or `machine_audio_output`), `outcome` (`sent`, or `kept` when the channel's list held it back). Counted only on a channel that is set up and turned on |
| `harold.turns` | counter, `{turn}` | `session.id`, `pane_id`, `started_by` |
| `harold.turn.duration` | histogram, `s`, buckets 10, 30, 60, 120, 300, 600, 1800, 3600 | `started_by` |
| `pod.specs` | gauge, `{spec}` | `pod.product`, `state` |
| `pod.spec.words` | gauge, `{word}` | `pod.product`, `spec.id`, `state` |
| `pod.spec.lead_time` | histogram, `s`, buckets 600, 1800, 3600, 7200, 14400, 43200, 86400, 172800 | `pod.product`, `phase` (`waiting_for_user`, `build`), `spec.revisions`, `spec.id`, `spec.stage` when the merge is a stage |
| `watch.runs` | counter, `{run}` | `folder`, `outcome` (`quiet`, `report`, `skipped`, or a failure reason: `timeout`, `process_failed`, `run_budget_reached`, `unreadable_answer`, `skill_missing`, `max_turns_reached`) |
| `watch.cost_usd` | counter, `USD` | `folder` |

`pod.specs` reports every state of every product, 0 included, so a card can read 0.

`harold.turns` keeps one series per session (`session.id`, `pane_id`, and each `started_by` seen) for the life of the Harold process, so a long-running Harold accumulates series until it restarts. Past the SDK's limit of 2000 series per instrument, new sessions fall into one overflow series, marked `otel.metric.overflow = true`.

## What leaves Harold

No exported item carries what the user or an agent wrote, a voice line, the AI model's output, an error message, the recipient or a token.

**Allowed fields.** Exported spans and log records keep only the attributes on this list, `ALLOWED_FIELDS` in `harold/src/telemetry.rs`:

```
pane_id, pane, session.id, session.source, component, reason, outcome, channel,
resolved_by, kind, started_by, turn.start, state, count, session_by, alert, pod.product, spec.id,
spec.state_at, spec.words, spec.revisions, phase, change, event_type,
event_version, result, watch, run, cost_usd, agent, seconds, event_id,
exit_code, signal, subtype, is_error, num_turns
```

Every other field is dropped before export, including `text`, `msg`, `raw_output`, `tag` and `error`. Span events and links are dropped too, and an error status loses its description.

**Which log records are exported.** Only records whose target is `harold`, starts with `harold::`, is `events` or starts with `events::`. Other crates' lines, such as Turso's and reqwest's, interpolate errors and paths into their text, so they stay in `harold.log` only. The collector-health lines also stay in `harold.log` only.

**The chief's pane.** `chief pane changed` (`info`) carries `state` (`found` or `none`), `count` (the agents in the chief's folder), `pane_id` and `session_by` (`setting` when `chief_session` picked the session, `longest` when the longest-running agent did). More than one move of the chief to another pane with `count > 1` in 7 days means an extra session or pane in the chief's folder to close. With `chief_session` set, a `session_by = longest` row means no agent ran in the named session; check that `chief_session` names the session `bin/pod` starts and that the chief runs in it. With it unset, a chief that restarts while a second session holds an older agent in the folder is replaced by that session's first pane, with `count = 2`, until the second session closes.

**Log bodies** are fixed text. A test scans every log call in `harold/src` and `events/src` and fails on one that puts a value into its message, apart from a short list of fixed reasons, migration names and a path.

## When the collector is down

Export never slows Harold:

- Spans, log records and metrics are queued in memory and sent in batches by a background job. A full queue drops the newest item.
- Each send gives up after 5 seconds.
- Shutdown waits at most 2 seconds for queued data.
- An unreachable collector logs one `telemetry export failing (collector unreachable at <endpoint>)` line, and one `telemetry export recovered` line when sends work again. It never logs one line per batch.

An `otlp_endpoint` that cannot be used at all logs `telemetry export off: the collector address could not be used`, and Harold runs with export off.

## The dashboards

The definition files are in [`hyperdx/`](hyperdx/), in the shape HyperDX 2.40's external API takes. [Creating the definitions](#creating-the-definitions) gives the commands.

### Harold: the cards

`dashboard.json` is the "Harold" dashboard. It holds the cards in five groups, a product filter, and an agent and a channel filter for the notifications card. Each card is two number tiles side by side: the card's number over the last 24 hours, and beside it the same number over the 24 hours before, named "<card>, previous 24 h". The exception is "Merges to main, last 24 h", whose earlier tile is named "Merges to main, the 24 h before". "Lines merged, last hour" reads the last hour, and its "previous hour" tile reads the hour before.

Each tile's window is written into its query: `TimeUnix >= now() - INTERVAL 24 HOUR` for a current tile, and `TimeUnix >= now() - INTERVAL 48 HOUR AND TimeUnix < now() - INTERVAL 24 HOUR` for an earlier one. Open the dashboard with a time range of at least the last 48 hours. A shorter range cuts the tiles down to it.

| Group | Card | The number | How the tile reads it |
|---|---|---|---|
| Pods | Free slots, average | `pod.slots.free` averaged over the period | builder: average |
| Pods | Busy slots, p95 | the 95th percentile of `pod.slots.busy` over the period | builder: 95th percentile |
| Pods | Merges to main, last 24 h | `pod.merges` in the period | SQL: the counter rule |
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
| Turn-end updates | Turn-end update, model p95 | the 95th percentile of the duration of `harold.local_model` spans with `result = answered`: the delay the user hears after a turn ends | SQL: the 95th percentile of those spans. Info only: tuning, no alert |
| Notifications | Notifications by agent and channel | a table with one row per `agent` and `channel`, and two columns, `sent` and `kept`: `harold.notifications` over the range you pick. A `kept` is something that channel's list held back; the chief rule, the voice mute, the pause and a message sent this turn are not `kept`. Only channels that are set up and turned on have rows. Severity info. You read it to see who is talking to you and to decide your lists: an agent with more than 20 `kept` in a day on the channel you use is one to consider adding, and an agent you expect to hear whose `sent` stops rising while it is working is one to look at. For a row, the `agent message dropped`, `spoken line dropped` and `notification skipped` lines with that `agent` and `channel` give `pane_id`, `kind`, `reason` and the times | SQL: the counter rule over the range, with the day before `$__fromTime` for the lag, `$__timeFilter` and `$__filters`. No fixed period, no earlier tile |
| Turn-end updates | Turn-end updates in full sentences | `model` divided by all `harold.turn_end_updates` over the last 7 days (its earlier tile is "previous 7 days"). Below 70% means the prompt and the model no longer agree on one sentence naming the agent | SQL: the counter rule, `model` rises over all rises. Info only: no alert |

The notifications card is the one card that is a table and has no earlier tile. Sixteen of the eighteen number cards are raw SQL tiles. Only the two slot cards use HyperDX's builder. The SQL tiles read ClickHouse through the connection, for three reasons:

- HyperDX's builder cannot express some cards, such as a median of per-session counts.
- HyperDX's own counter reading loses the first point of every new series ([Counters in HyperDX](#counters-in-hyperdx)).
- A card's number can be computed for any window, so each card can show its earlier number.

"Merged" and "Spec size" look back as far as ClickHouse keeps data, 30 days in the local stack. So on the first day the pod facts are on, every spec already merged counts as merged that day, and every spec already past `drafted` counts as first ready that day.

The latency card reads spans, not the `harold.phone_request.latency` histogram. A typed request's `harold.phone_request` span runs from the reply's own time to Enter, which is exactly its latency, and spans have no first-point loss.

**The product filter** is named "Product (pod and spec cards only)". It applies to the pod and spec cards only. The phone-request and turn tiles ignore it, because those metrics and spans carry no product: they are SQL tiles that leave out the `$__filters` macro, through which a SQL tile takes a filter, or they read the traces source, which the filter does not apply to. With all products, a pod card reads the products together: "Free slots, average" averages harold's and turso-object-store's numbers. The product filter is for pod and spec cards only. "Agent (notifications card only)" (on `Attributes['agent']`) and "Channel (notifications card only)" (on `Attributes['channel']`) are for the notifications card only: no pod, spec or turn metric carries `agent` or `channel`, so picking either empties those cards. `harold.notifications` carries no `pod.product`, so picking a product empties the notifications card: leave the product filter empty for it. The three filters do not mix, by design.

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
| Watch runs: cost and duration per run | Are the watches cheap and quick? As a guide, a run takes under 60 seconds and costs 1 to 2 cents | the `harold.watch.run` spans: median and 95th percentile duration, and the mean `cost_usd` per run | SQL table over the traces source, for the time range |

The SQL panels read the time range's points and, for the counter rule, the day before it.

The "Spec size" panel cannot show lines merged per spec: `pod.merged_lines` carries `pod.product` and `change` but no `spec.id`, so the two cannot be joined. Read a merged spec's lines from "Merges to main" at the time of its merge.

### Counters in HyperDX

Harold sends counters and histograms as running totals, the OpenTelemetry default. HyperDX turns a running total into increases by taking each point's difference from the series' previous point. It counts a series' first point as 0, because there is no previous one. The local stack's own provisioning notes the same undercount. Two things work around it.

**Counting from 0 at start.** With export on, Harold records 0 once, at start, for every attribute set of the counters the alerts read:

- `harold.phone_requests`, for each `outcome` (`typed`, `unavailable`), `channel` and `resolved_by`;
- `harold.phone_requests.wrong`, for each `channel` and `resolved_by`;
- `harold.web_app.pushes`, for each `outcome` (`accepted`, `gone`, `refused`, `unreachable`);
- `harold.turn_end_updates`, for `outcome = fallback` with `reason = model_failed`, which `local-model-failing` reads.

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
- The builder panel "Phone requests reaching the right agent" and the three counter alerts, `phone-request-unavailable`, `wrong-agent` and `local-model-failing`, still use HyperDX's reading. They depend on the counting from 0 above.

## The alerts

| Alert | Fires when | HyperDX definition |
|---|---|---|
| `phone-request-unavailable` | Any phone request dropped with no pane or after 10 failed tries, or not typed within 10 minutes | sum of `harold.phone_requests` with `outcome = unavailable` above 0, every 5 minutes |
| `phone-request-slow` | The 95th percentile latency is above 60 seconds in an hour with at least 3 requests | 95th percentile of the duration of typed `harold.phone_request` spans above 60 seconds, every hour, on the traces source |
| `wrong-agent` | A request is counted as wrong | sum of `harold.phone_requests.wrong` above 0, every 5 minutes |
| `slot-idle` | A slot is free for more than 2 hours in a row between 08:00 and 23:00 local | for each product, the last sample of each 15-minute window has at least 1 slot free, for 8 windows in a row |
| `local-model-failing` | The local model failed more than 3 times in an hour, so every turn-end update is the fallback sentence. Severity: warn. Action: the chief checks `uv` and `mlx_lm` (the `reason` on `local model failed` in the turn's trace says which). Expected rate: 0 | sum of `harold.turn_end_updates` with `reason = model_failed` above 3, every hour |
| `watch-cost-high` | Watches spent more than $1.00 in a day. Severity: warn. Action: the chief stops or tunes the watch (`harold tool watch list` shows the spend; `watch.cost_usd` by `folder` names the folder, and the `harold.watch.run` spans with the highest `cost_usd` name the watch and run). Expected firing rate: rarely. No budget ships, so this is the only spending signal until the user sets one | sum of `watch.cost_usd` above 1.00, over 1 day |

Their basis is the period the spec drew, 11 to 15 October:

- `phone-request-unavailable` would never have fired: none was dropped, and the slowest took 37 seconds.
- `phone-request-slow` would never have fired: the median was 6 seconds, the 95th percentile 11 seconds, and the highest hourly 95th percentile 37 seconds.
- `wrong-agent` would have fired once, on 14 October at 08:42.
- `slot-idle` would have fired every day: at most 2 of harold's 3 slots were ever busy at once. This is the gap the keep-3-busy rule is about.

`local-model-slow` is not an alert, and has no panel: the model's run time only needs tuning, and nobody is asked to act on it. The card "Turn-end update, model p95" (severity info, no action) shows the duration of the `harold.local_model` span. These are deviations from the approved spec, agreed by the chief.

- `watch-cost-high` is a signal, not a limit: it does not stop a watch. Failing runs are not an alert of their own: after 3 runs in a row fail, the watch stops and types `WATCH <name> stopped: 3 runs in a row failed (<reason>)` into the chief's pane, and the failure outcomes of `watch.runs` show the rest.

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

- `204` for a post naming one of the six alerts with state `firing` or `cleared`. It then types exactly `harold alert: <name> <state>` into the chief's pane: the first agent pane of the chief's tmux session in the folder `[notify] chief` matches.
- `400` for anything else, including another path or method. It logs `alert ignored (unknown alert)` and types nothing.

Harold also types lines of its own the same way. The first is `harold alert: delivery-given-up firing`, when it gives up a delivery after 10 failed tries (see the [notification reference](../notification/README.md#architecture)). It carries no message text. It is typed only when `alert_listen` is set and the address opened, with the checks below, as a posted line is. A post naming `delivery-given-up` is answered `400`.

The second is `harold alert: chief-message-held-back firing`, when a message or spoken line from the chief is held back by a channel's list (reason `not_listed`) on a channel that is set up and turned on. It is typed every time, a second one merges into one waiting, and a post naming it is answered `400`. Severity: warn. Action: the chief checks its folder against `[notify]` and the dash's agent list, then sends the message again or tells the user at the next chance. Expected rate: 0, because the default lists all name the chief's folder. The `agent message dropped` or `spoken line dropped` line at that time gives `channel`, `kind`, `agent` and `pane_id`; if the `chief pane changed` lines just before show a `count` above 1, a second pane in the chief's folder is the place to look as well.

The last two are `harold alert: store-failing firing`, when Harold marks its store failing, and `harold alert: store-failing cleared`, when the store recovers (see the [operation reference](../operation/README.md)). The two are typed under the same conditions as the `delivery-given-up` line. `cleared` follows only a `firing` line: a spell that began before the alert address opened is not cleared. Harold also logs the two changes, as `store failing` and `store recovered`, with only the fields `component = "store"` and `reason`. A post naming `store-failing` is answered `400`.

No log line carries any part of a posted body.

### What the chief receives

Harold remembers, for each alert, the state it last typed: `firing`, `cleared`, or nothing yet. A line is typed only when it changes that state:

- a `firing` is typed when the chief was last told `cleared`, or nothing;
- a `cleared` is typed when the chief was last told `firing`;
- a repeated `firing`, a second `cleared`, and a `cleared` for a `firing` the chief never heard are held back, and logged at debug level as `alert repeat held back`, with `reason` `repeat` or `never_fired`.

HyperDX posts `firing` again in every alert window while an alert's condition holds, so a spell of an alert reaches the chief as one `firing` line and one `cleared` line.

`slot-idle` is posted per product, but its line does not name the product. One `firing` covers every product's spell: a second product starting to idle while the first still does is held back, until a `cleared` is typed.

`delivery-given-up` is typed every time, since each one is a different lost message and none is cleared. While one waits, a second is merged into it, so the chief gets one line per wait. `store-failing` follows the rule of change, like the posted alerts.

What the chief does with a line is the chief's own instruction.

### Waiting for the chief

Typing follows the timed prompts' checks: the pane must be idle and its input box empty. Harold tries every 5 seconds, and a line waits as long as that takes. It also waits while no agent runs in the chief's folder, so a chief that starts later still hears an alert that is still firing. The first refused try logs `alert waiting (<reason>)` once, with `reason = no_chief` when no agent runs in the folder, and the line is logged again only when it is typed.

While a line waits, Harold keeps reading posts:

- a repeat of the waiting line changes nothing;
- a `cleared` that arrives while its `firing` waits untyped cancels both, logged as one `alert repeat held back` with `reason = never_fired`;
- a `firing` that arrives while a `cleared` waits, the chief having been told `firing`, cancels both, logged as one `alert repeat held back` with `reason = repeat`;
- lines waiting for different alerts are typed oldest first.

A line is dropped only when:

- the queue of 32 lines not yet read is full, logged as `alert not delivered (queue_full)`. The post is still answered 204. Repeats are read and held back as they arrive, so they do not fill it;
- Harold is stopping. A line queued after delivery stopped is logged as `alert not delivered (shutting_down)`; lines still waiting are abandoned without a log line;
- the line was typed and Enter failed, logged as `alert not delivered (enter_failed)`. It is not typed again, and counts as told, since it sits in the chief's input box.

### After a restart

What the chief was last told is kept in memory only, and starts empty when Harold starts. The first post of an alert that is still firing is typed again: for `slot-idle`, one line within 15 minutes of each restart.

### One writer at a time

One writer at a time types into a pane. Alerts, timed prompts and phone replies share a lock per pane around typing and Enter. Alerts and timed prompts also treat a pane as busy for a minute after anyone typed into it, because the agents view can still read idle just after typing.

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
for name in phone-request-unavailable phone-request-slow wrong-agent slot-idle local-model-failing watch-cost-high; do
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

# "Harold": an update must keep each filter's id, as it keeps the product filter's. The tiles keep theirs by the ids in the file.
old=$(dashboard "Harold")
fill dashboard.json \
  | jq --argjson old "$old" '.filters |= map(. as $filter | ([$old.filters[] | select(.name == $filter.name) | .id][0]) as $id | if $id then .id = $id else . end)' \
  | hdx -X PUT "$HYPERDX_URL/api/v2/dashboards/$(echo "$old" | jq -r .id)" --data-binary @-

# "Harold trends" has no filters.
fill trends.json | hdx -X PUT "$HYPERDX_URL/api/v2/dashboards/$(dashboard "Harold trends" | jq -r .id)" --data-binary @-
```

Then update each alert's webhook and the alert. The loop looks up both ids before it sends either PUT, and stops at the first name whose webhook or alert is missing or is named twice:

```sh
for name in phone-request-unavailable phone-request-slow wrong-agent slot-idle local-model-failing watch-cost-high; do
  WEBHOOK_ID=$(webhook_id "harold-alerts-$name") || break
  [ -n "$WEBHOOK_ID" ] || { echo "no webhook harold-alerts-$name: create it first" >&2; break; }
  id=$(alert_id "$name") || break
  [ -n "$id" ] || { echo "no alert $name: create it first" >&2; break; }
  fill "webhook-$name.json" | hdx -X PUT "$HYPERDX_URL/api/v2/webhooks/$WEBHOOK_ID" --data-binary @-
  fill "alert-$name.json" | hdx -X PUT "$HYPERDX_URL/api/v2/alerts/$id" --data-binary @-
done
```

A HyperDX set up from the earlier single-webhook files has one webhook named `harold-alerts` and none of the per-alert ones. Create them with this loop, which skips any that already exist, then run the update loop above:

```sh
for name in phone-request-unavailable phone-request-slow wrong-agent slot-idle local-model-failing watch-cost-high; do
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
