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
pod_registry = "~/code/<your-vault>/products.toml"
```

| Key | Turns on | When left out |
|---|---|---|
| `export` | Export, when true. False is export off: no pipeline is built, nothing is sent and no line is logged. `otlp_endpoint` is still checked | True: export is on |
| `otlp_endpoint` | The collector's address for export of spans, log records and metrics, over OTLP gRPC. Plain `http://` only: Harold is built without TLS, so an `https://` address is refused | `http://localhost:4317`, from `default.toml` |
| `otlp_http_endpoint` | The collector's OTLP HTTP address, where Harold forwards the web app's browser spans as OTLP JSON. Plain `http://` only | `http://localhost:4318`, from `default.toml` |
| `alert_listen` | The alert address, `POST /alert` on this address and port | No address is opened |
| `pod_registry` | The pod facts, read from the organisation's `products.toml`, in its vault | No pod folder is read and no `git` command runs |

`alert_listen` and `pod_registry` are off on their own. A malformed value stops Harold at start, and `harold --check-config` fails, with an error naming the key:

```
telemetry.otlp_endpoint must be an http:// address such as http://localhost:4317
telemetry.otlp_http_endpoint must be an http:// address such as http://localhost:4318
telemetry.alert_listen must be an address and port such as 127.0.0.1:4320
telemetry.pod_registry must be the path of the chief's products.toml
```

A well-formed alert address that cannot be listened on, such as a port another program already holds, does not stop Harold. Harold logs `alert address unavailable` with `reason = bind_failed` and runs with the alert address off until it restarts. Every other part of Harold, telemetry included, runs as usual.

Every export carries the resource attributes `service.name = "harold"` and `harold.machine = <machine_name>`, so the data of laptop-mac and studio-mac stays apart. Set `pod_registry` on one Mac only. Both Macs reading the same pods would count every merge twice.

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

- **Start:** the reply's own time, as stored in the Messages database.
- **End:** Harold has typed it into a pane and pressed Enter, before the confirmation is sent back.
- **Latency:** end minus start.

Each counted request has one outcome:

| Outcome | When |
|---|---|
| `typed` | It was typed into a pane within 10 minutes of its start |
| `unavailable` | Harold found no pane and dropped it, gave its delivery up after 10 failed tries, or typed it more than 10 minutes after its start. A reply read late because the Messages database was blocked counts here when it is typed late |
| `wrong` | It was typed on time, and the next phone reply, within 10 minutes, was tagged for a different pane. It is counted when that tagged reply arrives |

`harold.phone_requests` counts `typed` and `unavailable`. `harold.phone_requests.wrong` counts `wrong` separately, against the earlier request's channel and `resolved_by`. Availability is typed divided by all requests, on every channel. The right-agent rate is typed minus wrong, divided by all requests on the channels Harold can judge wrong. Today that is every channel but `web_app`, because a web app reply always goes to the chief's pane. A period with no request on those channels has no right-agent rate.

These rules decide `wrong`:

1. Only a request typed on time can later count as wrong. A late or dropped request never does.
2. "The next phone reply" is literal. Any counted reply replaces or clears the earlier one, including a reply that found no pane. A web app reply is counted but never judged: it always goes to the chief's pane, so it leaves the look-back alone, and never replaces the last typed iMessage reply that rule looks back at. A reply another Mac owns, or one that arrives while messaging is paused, leaves it alone.
3. Only a request whose pane Harold chose can count as wrong: `resolved_by` of `latest_notifier`, `ai_model` or `my_agent`. A request the user tagged (`exact_tag`, `tag_substring`) never counts as wrong.
4. A reply tagged for another Mac (`[studio-mac/pane] …` or `[studio-mac] …`) is a tag for a different pane. The Mac that typed the earlier request counts that request as wrong under rules 1 and 3, within 10 minutes. The tagged reply itself is counted only by the Mac it names, and, as rule 2 says, it leaves the earlier request in place. A reply with no machine in its tag that another Mac owns because it sent the latest phone notification counts nothing here.
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
| `unknown` | a reply with no open request, for example one stored before a restart | when it was stored |
| `web_app` | a reply sent from the web app | its `POST /replies` |

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
| `harold.delivery` | the turn-end update sent or said | `kind`, `channel` (the `[notify]` key: `machine_audio_output`, `imessage` or `web_app`), `outcome`, `reason` for `skipped` and `held_back`, `text_cut` on `imessage` and `web_app` updates |

**A turn you start by speaking in a call** is one trace, with these spans in order:

| Span | From – to | Attributes |
|---|---|---|
| `harold.call_turn` | speech starts – the turn's answer has played or been stopped, or the turn is dropped, forgotten, given up, or the call ends | `call_from` (`desk` or `phone`), `pane_id` (set when the turn is typed), `result` (the turn's result: `answered`, `handed_over`, `typed`, `nothing_heard`, `no_chief`, `chief_busy`, `failed`, `voice_muted` or `forgotten`), `answered_by` (`answerer`, `chief` or `none`), `first_sound_ms`, `answer_ms` |
| `harold.call_speech`, inside it | speech starts – the cut | `cut` (`quiet` after 60 quiet frames, or `max_length` at 60 s) |
| `harold.call_transcribe`, inside it | the turn's WAV written – the transcript back from Whisper | `audio_ms` (the clip), `waited_ms` (how long the job waited for the worker, which is busy while a Kokoro job runs, and does not count the worker's load), `load_waited_ms` (how long the turn waited for the worker to finish loading: 0 when the worker was ready before the turn was cut), `result` (`text`, `no_speech` or `failed`) |
| `harold.turn`, and `harold.turn_complete` inside it | as above, with `started_by = voice` | as above |
| `harold.call_update` | the chief's turn-end update made by the call answerer's `claude -p` path, as the caller waits for it (8 s at most) | `outcome` (`model` or `fallback`), `reason` (`none`, `failed`, `timeout`, `too_long`, `no_name` or `answerer_off`), `cost_micros`. No text |
| `harold.call_answer`, inside it | the call answerer's run: the call facts gathered and the `claude -p` run | `result` (`said`, `handed_over` or `failed`), `reason` (on a failure: `not_started`, `exited`, `timed_out` or `no_answer`), `facts_ms`, `model_ms`, `cost_micros`, `input_tokens`, `output_tokens` |
| `harold.call_voice`, inside it | a line or phrase queued for Kokoro – the sound made | `source` (`chief_line`, `update`, `answer`, `phrase` or `other_agent`), `waited_ms`, `audio_ms` (the made WAV), `result` (`made` or `failed`). A kept phrase already on disk has none |
| `harold.call_play`, inside it | the sound handed to the sink – the sink says it ended | `source` (as above, plus `tick`, `ringback` and `pickup`; the last two belong to no turn), `call_from`, and on the phone `start_ms` |

`harold.call_turn` opens when speech starts and ends when the turn's answer has played or been stopped. It also ends when the turn is dropped, forgotten, given up (the 90 s wait) or the call ends. A turn held for a busy chief goes on in the same span, and the user's next speech joins it. When the call ends while the turn is still held, the span ends there and the turn, once typed after the call, joins a root `harold.call_turn` of its own with only `pane_id`, so that trace has no speech spans and no sound numbers. `first_sound_ms` is the time from the cut to the start of Harold's first sound for the turn, the tick, the ringback and the pick-up left out; for a turn the call answerer answers it is the answer itself, so it equals `answer_ms`. `answer_ms` is the time from the cut to the start of the answer: the call answerer's `say`, the chief's first `harold speak` line in the turn, or the update line. On a turn the call answerer answered (`answered_by = answerer`) no phrase comes before the answer, and no turn-end update runs; a turn that fell back to the chief has `answered_by = chief`, and a dropped turn `none`. A failed run of the call answerer writes the WARN line `call answer failed` with `reason` (the exit status stays in `harold.log`). Hand-over lines carry `hand_over_id` and never the request: `hand-over typed` (`pane_id`, `typed_while_busy`, `wait_ms`), `hand-over waiting` (once, with `reason`) and `hand-over not typed` (WARN, after 30 minutes). See the [call answerer reference](../voice-input/call-answerer.md). Either is left out when no such sound played, and the cut is when the quiet frames end the speech, so the user's last word was about 1.2 s earlier. On the phone both count to when Harold offered the sound; `start_ms` on `harold.call_play` is the further time until it started on the phone, from 0 to 60,000. At the desk `afplay` starts at once, so there is no `start_ms`. A `harold.call_voice` or `harold.call_play` for a sound of the turn is a child of `harold.call_turn`; a line outside a turn, such as another agent's, gets root spans of its own. When the wait gives up, `harold.call_turn` ends before a late stop hook adds its `harold.turn`, so that child can outlive its parent. `harold.call_update`'s duration is how long the call makes the caller wait for the update. No call span holds what was said, the transcript, the line or a file path.


**A call's start** is a root span of its own, `harold.call_ring`, from the tap to the end of the pick-up sound, with `cycles` (ringback cycles played, 1 or more), `worker` (`ready` when the worker was loaded at the tap, `loaded_now` when the call waited for it) and `warm_ms` (how long the call answerer's warm-up took, 5000 when it was cut at its limit; absent when the call has no answerer). `cycles` should be 1 for a call whose worker the page loaded, and 2 or 3 for a cold one. To see how calls start, count `harold.call_ring` spans by `cycles` per day; act when calls ring 4 or more cycles, which is over 12 s, and read the `voice worker ready` line's `load_ms` and `started_by` from that time. Info only: no alert, and no tile on the dashboard yet.

The chief's update in a call is the fallback sentence when `harold.call_update` has `outcome = fallback`; its `reason` says why. A run that fails or passes 8 s also writes the WARN line `call update model failed` with `reason` (`failed` or `timeout`) inside the span. No span or line holds the sentence, the prompt or the model's text. To see how the updates go, count `harold.call_update` spans by `outcome` and `reason`, and read their duration, which should be under 5 s. Info only: no alert, because this update is rare and the user hears the fallback sentence.

A watch run is a trace of its own: `harold.watch.run`, one run of a watch, with `harold.watch.model` (the Claude Code process, with `result`: `exited`, `killed` or `failed_to_start`) inside it. The run span has `watch`, `run`, `outcome` (`quiet`, `report`, `skipped` or a failure reason), `reason` and `cost_usd`, and the run's log lines carry its trace id.

`harold.delivery`'s `channel` is the `[notify]` key of the channel that carried the update, or held it: a desk update is `machine_audio_output` whether skipped or held back, and one the app carried is `web_app`. Its `outcome` is `delivered`, `skipped` (with `reason` `duplicate_turn`, `messaging_pause`, `messaged_this_turn` or `duplicate_message`), `held_back` (a list, or the desk rule, kept the update from going out, with `reason` `not_listed` or `at_desk`, and `channel` the key of the list that held it, or `machine_audio_output` for `at_desk`), `failed` (a send that failed before its 10th try, which is tried again, or a malformed payload, which is not) or `given_up` (its 10th failure; it is not tried again).

`harold.delivery`'s `text_cut` is `true` when the update was cut to fit iMessage, and `false` when it went whole. The app always gets the whole text, so `web_app` is always `false`. A cut also writes an info line, `turn-end update cut`, inside the span, with `channel`, `pane_id` and `count` (the characters before the cut). It never holds the text. An `AI CLI failed` or `failed to spawn AI CLI` warn line in the same trace says the whole reply was cut, not the AI CLI's text; with neither line, the AI CLI's text was cut, unless the AI CLI is not set or printed nothing, which write no line. The `AI CLI failed` line carries `exit_code`, `subtype` and `is_error` from the CLI's JSON result, never its text.

After a release, check by channel and day: `web_app` with `text_cut = true` must be 0, since more means the app path still cuts. For `imessage`, a day with at least 5 updates and more than 20% cut means the AI CLI is ignoring its 280-character rule or failing over to the whole reply, which is the cue to tighten the prompt. This is a check, not an alert.

```sql
SELECT toDate(Timestamp) AS day, SpanAttributes['channel'] AS channel,
       count() AS updates, countIf(SpanAttributes['text_cut'] = 'true') AS cut
FROM otel.otel_traces
WHERE SpanName = 'harold.delivery' AND SpanAttributes['outcome'] = 'delivered'
  AND SpanAttributes['kind'] = 'turn_end'
  AND Timestamp > now() - INTERVAL 7 DAY
GROUP BY day, channel ORDER BY day
```

Two queries check `harold.delivery` after a release, each over the last 7 days. No alert reads them. Both should give 0. The first gives more than 0 when a skip path records no `channel` or `reason`. The second gives more than 0 when a turn-end update was delivered on `machine_audio_output`: Harold speaks none at the desk now, so the trace shows which turn and `pane_id` which pane.

```sql
SELECT count() FROM otel.otel_traces
WHERE SpanName = 'harold.delivery' AND SpanAttributes['outcome'] = 'skipped'
  AND (SpanAttributes['channel'] = '' OR SpanAttributes['reason'] = '')
  AND Timestamp > now() - INTERVAL 7 DAY;

SELECT count() FROM otel.otel_traces
WHERE SpanName = 'harold.delivery' AND SpanAttributes['outcome'] = 'delivered'
  AND SpanAttributes['channel'] = 'machine_audio_output'
  AND SpanAttributes['kind'] = 'turn_end'
  AND Timestamp > now() - INTERVAL 7 DAY
```

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
| `harold.web_app.pushes` | counter, `{push}` | `outcome` (`accepted`, `gone`, `refused`, `unreachable`) |
| `harold.app_spans.not_forwarded` | counter, `{span}` | `reason` (`collector_unreachable`, `collector_refused`, `timed_out`, `queue_full`) |
| `harold.notifications` | counter, `{notification}` | `agent` (the pane's folder below the home folder, such as `code/harold/main`), `channel` (`imessage`, `web_app` or `machine_audio_output`), `outcome` (`sent`, or `kept` when the channel's list held it back). Counted only on a channel that is set up and turned on |
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

**Allowed fields.** Exported spans and log records keep only the attributes on `ALLOWED_FIELDS` in [`harold/src/telemetry.rs`](../../../harold/src/telemetry.rs). That list is the only source: each field on it is an id, a count, a time, a fixed word or a short name, never free text. A change to what leaves Harold is a change to that list, made in a spec and reviewed with it.

`model` is a model name from the settings, a Hugging Face repository for the voice or the `[watch] model` setting, never a path or a token. Every other field is dropped before export, including `text`, `msg`, `raw_output`, `tag` and `error`. Span events and links are dropped too, and an error status loses its description.

**Which log records are exported.** Only records whose target is `harold`, starts with `harold::`, is `events` or starts with `events::`, or is `buckwal` or starts with `buckwal::`. Other crates' lines, such as Turso's and reqwest's, interpolate errors and paths into their text, so they stay in `harold.log` only. The collector-health lines also stay in `harold.log` only.

**The chief's pane.** `chief pane changed` (`info`) carries `state` (`found` or `none`), `count` (the agents in the chief's folder), `pane_id` and `session_by` (`setting` when `chief_session` picked the session, `longest` when the longest-running agent did). More than one move of the chief to another pane with `count > 1` in 7 days means an extra session or pane in the chief's folder to close. With `chief_session` set, a `session_by = longest` row means no agent ran in the named session; check that `chief_session` names the session `bin/pod` starts and that the chief runs in it. With it unset, a chief that restarts while a second session holds an older agent in the folder is replaced by that session's first pane, with `count = 2`, until the second session closes.

**Log bodies** are fixed text. A test scans every log call in `harold/src` and `events/src` and fails on one that puts a value into its message, apart from a short list of fixed reasons, migration names and a path.

**A pane found.** `agent monitor pane found` (`info`) is logged when Harold first tracks a new agent incarnation in a pane, whether the inventory or a hook saw it first, with `pane_id`, `provider_id` and `program`: the file name of the matched agent program, never its arguments. Its counterpart is `agent monitor pane gone`, with `pane_id`; the two give how long the pane was listed. A `program` that is not an agent's name means a false match. This query should list only `claude`, `codex` and `opencode`:

```
SELECT LogAttributes['program'] AS program, LogAttributes['provider_id'] AS provider, count()
FROM otel.otel_logs
WHERE ServiceName = 'harold' AND Body = 'agent monitor pane found' AND Timestamp > now() - INTERVAL 7 DAY
GROUP BY program, provider
```

## When the collector is down

Export never slows Harold:

- Spans, log records and metrics are queued in memory and sent in batches by a background job. A full queue drops the newest item.
- Each send gives up after 5 seconds.
- Shutdown waits at most 2 seconds for queued data.
- An unreachable collector logs one `telemetry export failing (collector unreachable at <endpoint>)` line, and one `telemetry export recovered` line when sends work again. It never logs one line per batch.

An `otlp_endpoint` that cannot be used at all logs `telemetry export off: the collector address could not be used`, and Harold runs with export off.

## Browser spans

The web app records what happens in the phone's browser as spans under `service.name = harold-web-app`: a `documentLoad` span with `documentFetch` and one `resourceFetch` per script and stylesheet, a fetch span for each request (the `POST /replies` of a Send, a todo answer), a `browser.error` span for each script error, and a `browser.message_shown` span when a row of the thread is drawn, and a `browser.thread_opened` span each time the thread is placed at the read mark (when the page loads and when it comes back to the front). Harold serves the page's `telemetry.js` and one vendored OpenTelemetry bundle only while export is on ([ADR 0012](../../adr/0012-browser-libraries-are-vendored-as-one-bundle-built-from-a-pinned-audited-npm-lockfile.md)).

**The route.** The page posts a batch every 5 seconds, and when it is hidden, to `POST /telemetry/traces`, behind the login and origin checks, as OTLP JSON. Harold answers `200 {}` at once, `413` for a body over 256 KB and `400` for one that is not OTLP JSON. With export off it answers `200 {}` and drops the batch.

**What Harold keeps.** Spans from three scopes only: document load, fetch and `harold-web-app`. Each keeps its trace id, span id, parent span id, name (letters, digits and `_ . - / :` only), kind, times and status code, and these attributes: `url.full` (rewritten to its path: no scheme, host, query or fragment), `http.request.method`, `http.response.status_code`, `http.response_content_length`, `user_agent.original`, `error.kind`, `exception.type`, `code.filepath` (a path), `code.lineno`, `code.column`, `harold.row` (`chief` or `me`, on `browser.message_shown` only), `harold.opened_at` (`first_unread`, `newest` or `oldest_on_page`) and `harold.unread` (the count of rows after the read mark that are not mine; with `oldest_on_page` a lower bound), both on `browser.thread_opened` only, and, on `browser.error` only, `exception.message` cut to 200 characters. Events keep their name and time. Links, status messages and every other attribute are dropped. The resource is set by Harold: `service.name = harold-web-app` and Harold's version. **`exception.message` is the only free text in the web app's spans**, a choice the user made knowingly; a message can quote page data.

**Forwarding.** In the background, to `[telemetry] otlp_http_endpoint` + `/v1/traces`, each forward under 2 seconds and at most 4 at a time. A batch beyond that is dropped. Spans that did not reach the collector are counted in `harold.app_spans.not_forwarded` by `reason`. One line, `app traces not forwarded` with `reason`, is logged when forwarding starts failing, and `app traces forwarded again` when it recovers. The lines carry no span content.

**One trace from the tap to the answer.** The page's fetch timing adds a `traceparent` header to `POST /replies` and the todo answers. Harold opens `harold.phone_request` (`channel = web_app`) under the phone's span and stores the reply inside `harold.reply_stored`, so the typed prompt, the chief's turn, `harold.turn_complete`, `harold.delivery` and the answer row join the phone's trace. The row is stored with the `traceparent` of the span that stored it (`app_messages.trace`); the update stream opens `harold.app_row_sent` when it first sends the row, and the row carries that span as `data-trace`, so the page records `browser.message_shown` as its child. A message the chief sends mid-turn opens `harold.away_message` under the pane's last typed prompt while it is within one hour. A todo answer keeps the `traceparent` with the answer (`trace_parent`); the `ANSWERS` line opens `harold.answers_typed` under the first answer's trace with a link to each other, and a line of answers from the dash is a root. A hand-over joins the call turn's trace.

```
SELECT SpanName, Timestamp, Duration / 1000000 AS ms
FROM otel.otel_traces
WHERE TraceId = '<trace id>'
ORDER BY Timestamp
```

The first real batch after an install:

```
SELECT SpanName, count() FROM otel.otel_traces
WHERE ServiceName = 'harold-web-app' AND Timestamp > now() - INTERVAL 1 HOUR GROUP BY SpanName
```

The clocks of the phone and the Mac can differ by a few milliseconds, so a phone span may start just before its Harold parent in HyperDX.

## The knowledge index

The [knowledge tool](../tools/knowledge.md#logs-and-spans) logs and traces these. No line or span holds a query, a vault note's text or a passage.

**The owner's questions and where they are answered**

1. *Is the index up to date with the vault?* `knowledge indexed` (info) after each pass and each watched change: `notes` (vault notes embedded), `count` (passages embedded), `removed`, `embed_ms`, `pending` (changed files not yet indexed, 0 after a good pass) and `last_ok_age_s`.
   - **Issue detection:** the hourly timer logs `knowledge index stale` (warn) with `last_ok_age_s` when no pass has succeeded for more than 2 hours, two missed hourly passes. The release watch reports any warn. The card "Knowledge index: hours since the last good pass" shows the time since the last `knowledge indexed` line, info only, with the same 2-hour line.
   - `knowledge index failed` (warn) carries `reason`: `vault_unreadable`, `worker_failed`, `store_failed` or `bucket_unreachable`.
   - **Error diagnosis:** for `bucket_unreachable`, the `buckwal.commit` span with `buckwal.outcome = "error"` and its `buckwal.upload` child's `s3.status` and `buckwal.retries` in the same trace say whether the bucket refused (a 4xx) or did not answer. For `worker_failed`, `knowledge worker exited` gives the `reason`: `failed_to_load`, `exited` or `timed_out`.
2. *Does search find what I mean, and how fast?* The `harold.knowledge.search` span carries `source` (`chief`, `app` or `dash`), `results`, `embed_ms` and `search_ms`.
   - **Optimisation:** a warm search should take under 300 ms (`embed_ms` plus `search_ms`). The card "Notes searches by source" shows the median and the count, info only.
   - **Issue detection:** searches with 0 results are counted on the same card. More than 5 in a day means the index or the prompts are off.
3. *What does it cost the Mac?* `knowledge worker stopped` (info), when the worker stops: `load_ms`, `embed_ms_per_passage` (the mean over its life), `passages` and `rss_mb` (its resident memory just before it stopped, read with `ps`).
   - **Optimisation:** resident memory over 1.5 GB, or a mean over 200 ms a passage, is the cue to rethink the runtime or the model. The card "Knowledge worker, last 7 days" shows the highest `rss_mb`, the mean `embed_ms_per_passage` and the passages.

## The knowledge graph

The [knowledge tool](../tools/knowledge.md#graph-logs-and-spans) logs and traces these. No field holds a vault note's path or text.

1. *Is the graph in step with its logs?* The gauge `harold.knowledge.graph_waiting` counts the schema and data records with a sequence greater than the graph position. It is published after each change, after a change that TypeDB refuses or did not answer, and every minute, while the graph is on.
   - **Issue detection:** the card "Graph records waiting" shows it, info only, with a line at 0. Over 0 for more than 10 minutes means the graph is short of its logs. The `knowledge graph behind` warn line fires when TypeDB refuses a record (`sequence`, `reason`, `waiting`), and when records have waited 10 minutes (`reason` = `waiting_too_long`, `sequence` = the first record that waits). It logs once for each spell, and the release watch looks for it. It should never happen. A graph that is stopped when idle is not judged. When TypeDB did not answer, the minute tick judges nothing and no warn line is written: the gauge and `graph status` show the records that wait.
   - **Error diagnosis:** the warn line names the record's sequence and a reason word. `harold tool knowledge graph status` prints the position, the end of each log, the records waiting and the last refusal. Read the record from the data log by its sequence. A `voided` count above 0 on a rebuild span leads to the `knowledge graph record voided` lines.
2. *If TypeDB were lost, would the rebuild work, and how long would it take?* The span `harold.knowledge.graph_rebuild` carries `reason` (`asked` or `missing`), `schema_steps`, `records`, `voided`, `ms` and `result`.
   - **Optimisation:** the `ms` for the vault's size is the number a change is judged by. The aim is under 30 s at today's size.
3. *Is the chief using the typed relations?* The gauge `harold.knowledge.typed_relations` counts them. The card "Typed relations, 30 days" shows it, info only. Flat for 30 days means the chief is not using them, and its skill rule is worth a look.

**buckwal's telemetry.** Harold never calls `buckwal::telemetry::init`, which would install a second global subscriber and fail beside Harold's. buckwal's spans and log records (target `buckwal`) go through Harold's own subscriber: they are written to `harold.log` as other crates' lines are, and `span_export_filter` and `is_exported_log_target` in `harold/src/telemetry.rs` also export them under `service.name = harold`. `ALLOWED_FIELDS` adds the buckwal attributes the diagnosis uses. **Detection:** `buckwal.commit` spans with `buckwal.outcome = "error"` counted per hour in ClickHouse are on the card "buckwal commits that failed, per hour". Above 0 in an hour on two hours running matches `knowledge index stale`.

The dashboard group "Knowledge index" holds the six cards. They read `otel.otel_logs` and the span table directly, so they are tables and numbers without an earlier tile, and the product filter does not reach them.

## The dashboards

The definition files are in [`hyperdx/`](hyperdx/), in the shape HyperDX 2.40's external API takes. [Creating the definitions](#creating-the-definitions) gives the commands.

### Harold: the cards

`dashboard.json` is the "Harold" dashboard. It holds the cards in six groups, a product filter, and an agent and a channel filter for the notifications card. Each card is two number tiles side by side: the card's number over the last 24 hours, and beside it the same number over the 24 hours before, named "<card>, previous 24 h". The exception is "Merges to main, last 24 h", whose earlier tile is named "Merges to main, the 24 h before". "Lines merged, last hour" reads the last hour, and its "previous hour" tile reads the hour before.

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
| Phone requests | Right agent | typed minus wrong, divided by all requests, on every channel but `web_app` (no number if there are none) | SQL: the counter rule, over both counters |
| Phone requests | Latency, p95 | the 95th percentile of the phone-request latency | SQL: the 95th percentile of the duration of `harold.phone_request` spans that were typed (they carry `pane_id`) |
| Turns | Turns | `harold.turns` in the period | SQL: the counter rule |
| Turns | Turns per session, median | the median of turns per `session.id` | SQL: `harold.turn` spans counted per `session.id`, one span per turn |
| Specs | Waiting for approval | `pod.specs` in that state at the end of the period | SQL: the last value per product, summed |
| Specs | Merged | specs that reached merged in the period | SQL: specs whose first `pod.spec.words` sample in state `merged` falls in the period |
| Specs | Spec size, median words | the median `pod.spec.words` of specs first ready in the period | SQL: specs whose first sample past `drafted` falls in the period |
| Specs | Waiting for you, p50 | the median `pod.spec.lead_time`, phase `waiting_for_user` | SQL: the counter rule over the histogram's count and sum |
| Specs | Build, p50 | the median `pod.spec.lead_time`, phase `build` | SQL: the counter rule over the histogram's count and sum |
| Turns | Call turn, time to first sound and to the answer, last 7 days | a table with one row per `call_from` and `answered_by`: the number of `harold.call_turn` spans, and the p50 and p95, in seconds, of `first_sound_ms` and of `answer_ms` over the last 7 days. The aim is an `answer_ms` p50 about 5.5 s for `answerer` (`first_sound_ms` follows it, since the answer is the first sound), and near the chief's own turn time plus 1 s for `chief`. On a day with at least 3 answerer turns, act when the `answer_ms` p50 is above 7 s, or above 15 s for `chief`: look at a turn's trace, where the long child span shows the slow part (`call_answer`'s `model_ms` against `call_transcribe`, and `load_waited_ms` against the worker's `load_ms`). The last column, `waited_for_the_load`, counts the `harold.call_transcribe` spans in those turns with `load_waited_ms` above 0; a call now rings until its worker is ready, so a first turn does not wait for the load and only a worker restarted mid-call can make it non-zero; the early-load signal is `harold.call_ring`. On a day with at least 3 call turns, more `chief` turns than `answerer` turns means the call answerer is failing. Info only: no alert, no earlier tile | SQL: `harold.call_turn` spans, `quantileIf` over the turns that carry the number |
| Turns | Call answers failed, by day | the number of `harold.call_answer` spans with `result = failed` for each of the last 7 days. Above 2 on a day is the cue to look. Info only: no alert. The release watch already reports any `call answer failed` WARN since an install | SQL: `harold.call_answer` spans counted per day |
| Turns | Call answerer spend, by day | the sum of `cost_micros` of `harold.call_answer` spans, in dollars, for each of the last 7 days. Above $0.10 on a day, about 300 turns at today's price, means far more turns than expected or a prompt that grew. Act when the median `input_tokens` of a day's answers is above 6,000 (`SELECT quantile(0.5)(toUInt64OrZero(SpanAttributes['input_tokens'])) FROM otel.otel_traces WHERE SpanName = 'harold.call_answer' AND Timestamp > now() - INTERVAL 1 DAY`): the cause is then the pods' spec count. Info only: no alert | SQL: `harold.call_answer` spans, `cost_micros` summed per day |
| Web app | Web app errors per day | `browser.error` spans for each of the last 7 days, by `exception.type` and `code.filepath`. Act when an error is new, or when any one passes 5 in a day | SQL table over `harold-web-app` spans |
| Web app | Phone page load (p50, p95) | `documentLoad` spans for each of the last 7 days: the number, p50 and p95 in seconds. Act when the p95 passes 3 s on a day with at least 5 loads | SQL table |
| Web app | Browser spans not forwarded per day | `harold.app_spans.not_forwarded` for each day and `reason`. Act on any value above 0: check the collector, then the `app traces not forwarded` line | SQL table: the counter rule |
| Web app | Phone turns in one trace | `harold.turn` spans with `started_by = phone`, and the share with a parent, for each day. Act when it is below 90% on a day with at least 5 phone turns; a reply typed after the prompt is kept (1 hour), or after a restart, loses its link | SQL table |
| Web app | Browser spans stored per day | the number of `harold-web-app` spans for each day. Act when it passes 2,000 a day; a normal day is a few hundred | SQL table |
| Web app | Thread opened at the oldest row, 7 days | the share of `browser.thread_opened` spans with `harold.opened_at = oldest_on_page` over the last 7 days. A share above 10% means the 50-row page is too short for how long you are away: a backlog item. A wrong position shows as `newest` with `harold.unread` above 0, or `first_unread` right after a reply; the span's trace id and the `app page connected` line at the same second show whether a reconnect patch landed late. Info only: no alert | SQL table |
| Web app | Web app posts refused, by route, per day | the `app post refused` lines for each of the last 7 days, by `route`. Expected: none after the release. Any count is a page that still posts without Datastar, which the asset test should have caught: look at once. Info only: no alert | SQL table over `otel.otel_logs` |
| Web app | Pages closed without a hidden report, share per day | the `app page disconnected` lines for each of the last 7 days: the number, the share with `hidden_reported = false`, and the number with `repeated`. A share over 20% on a day with at least 10 closes means the phone drops the hidden reports often and they hold back pushes: the user then decides whether to keep the three hide-time posts through Datastar. A `repeated` above 0 means a report reached Harold twice, which no retry should cause. Info only: no alert | SQL table over `otel.otel_logs` |
| TTS server | TTS time to first audio, p50 and p95 by machine, loaded and device, last 7 days | for each day, machine, `loaded` and `device`: the number of `harold.tts.speak` spans and the p50 and p95 of `first_audio_ms`, for lines that were spoken. The aims, with the model loaded: a p95 under 150 ms on the RTX 4090 (`device = gpu`), and under 500 ms on the Mac and on a CPU fallback. A model change is judged by it | SQL table over `harold.tts.speak` spans |
| TTS server | TTS failures per day, by machine, caller and reason | the `tts failed` lines for each of the last 7 days, by machine, `caller` (`desk`, `call`) and `reason`. A failure that affects only calls shows here. Info only: no alert, because you hear a missing line | SQL table over the logs |
| Notifications | Spoken lines dropped while locked, by pane, per day | the `spoken line dropped (screen locked)` lines for each of the last 7 days, by `pane_id`. More than 3 drops on a day from one pane means that agent speaks while the user is away: change its skill to send text instead. Expected: under 3 a day for each pane, since the chief's lines go to the web app as text. Info only: no alert | SQL table over `otel.otel_logs` |
| Notifications | Notifications by agent and channel | a table with one row per `agent` and `channel`, and two columns, `sent` and `kept`: `harold.notifications` over the range you pick. A `kept` is something that channel's list held back; the chief rule, the voice mute, the pause and a message sent this turn are not `kept`. Only channels that are set up and turned on have rows. Severity info. You read it to see who is talking to you and to decide your lists: an agent with more than 20 `kept` in a day on the channel you use is one to consider adding, and an agent you expect to hear whose `sent` stops rising while it is working is one to look at. For a row, the `agent message dropped`, `spoken line dropped` and `notification skipped` lines with that `agent` and `channel` give `pane_id`, `kind`, `reason` and the times; `spoken update left out (not in notify.machine_audio_output)` gives them for a desk update | SQL: the counter rule over the range, with the day before `$__fromTime` for the lag, `$__timeFilter` and `$__filters`. No fixed period, no earlier tile |

The notifications card and the call turn card are the two cards that are tables and have no earlier tile. Sixteen of the eighteen number cards are raw SQL tiles. Only the two slot cards use HyperDX's builder. The SQL tiles read ClickHouse through the connection, for three reasons:

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
| Phone requests reaching the right agent | When I reply from the phone, does it reach the agent I meant? | `harold.phone_requests` and `.wrong` per day: right, wrong, unavailable; availability on every channel, and right-agent rate on every channel but `web_app` | builder line: typed, unavailable, wrong, the same two for judged channels, availability and right agent |
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

- `harold.phone_requests`, for each `outcome` (`typed`, `unavailable`), `channel` and `resolved_by`, and for `channel = web_app` with `resolved_by = none` only;
- `harold.phone_requests.wrong`, for each `channel` and `resolved_by` of `imessage` and `unknown`, never `web_app`;
- `harold.web_app.pushes`, for each `outcome` (`accepted`, `gone`, `refused`, `unreachable`);
- `harold.app_spans.not_forwarded`, for each `reason`;

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
| `watch-cost-high` | Watches spent more than $1.00 in a day. Severity: warn. Action: the chief stops or tunes the watch (`harold tool watch list` shows the spend; `watch.cost_usd` by `folder` names the folder, and the `harold.watch.run` spans with the highest `cost_usd` name the watch and run). Expected firing rate: rarely. No budget ships, so this is the only spending signal until the user sets one | sum of `watch.cost_usd` above 1.00, over 1 day |

Their basis is the period the spec drew, 11 to 15 October:

- `phone-request-unavailable` would never have fired: none was dropped, and the slowest took 37 seconds.
- `phone-request-slow` would never have fired: the median was 6 seconds, the 95th percentile 11 seconds, and the highest hourly 95th percentile 37 seconds.
- `wrong-agent` would have fired once, on 14 October at 08:42.
- `slot-idle` would have fired every day: at most 2 of harold's 3 slots were ever busy at once. This is the gap the keep-3-busy rule is about.
- `watch-cost-high` is a signal, not a limit: it does not stop a watch. Failing runs are not an alert of their own: after 3 runs in a row fail, the watch stops and types `WATCH <name> stopped: 3 runs in a row failed (<reason>)` into the chief's pane, and the failure outcomes of `watch.runs` show the rest.

Merges, turns per session and the spec facts have no alert. They describe throughput, and no level of them is wrong in itself.

Each alert is an inline alert: it carries its own query, so it needs no dashboard tile. A tile alert would need a tile with exactly the alert's query, and the cards dashboard holds the cards only.

`phone-request-slow` reads spans, as the latency card does. A typed request's `harold.phone_request` span carries `pane_id` and runs from the reply's own time to Enter, so its duration is the request's latency. Spans have none of the counters' first-point loss, so the first request after a restart counts.

HyperDX 2.40 cannot express three parts of these conditions:

- **10 minutes.** Its intervals are 1, 5, 15 and 30 minutes, 1, 6 and 12 hours, and 1 day. A count above 0 is caught by any window that holds the request, so the 5-minute windows catch every request a 10-minute window would, and sooner. The 10 minutes in `phone-request-unavailable` is the outcome's own rule, already applied by Harold.
- **At least 3 requests.** An alert has one condition, so `phone-request-slow` fires on the 95th percentile alone. The chief applies "at least 3 requests" by checking the count on the trends dashboard before it tells anyone.
- **08:00 to 23:00.** An alert has no time-of-day window, so `slot-idle` fires at any hour. The chief ignores a `slot-idle` line outside 08:00 to 23:00 local.

`slot-idle` is grouped by `pod.product`, so a free slot in one product is not hidden by a full one in another. Each product fires and clears on its own, and each sends its own post. HyperDX 2.40 reduces a gauge to its last sample in each alert window, so a slot taken and freed again within 15 minutes, between two samples, does not break the run.

### Speech models and the voice worker

Speech models are an alert path, not an alert: `speech model not downloaded` and `voice worker exited` are WARNs, and the release watch already reports every WARN in `harold.log`. The expected rate is near 0 a day: only after an install that changes a model, or a cleared cache. The log lines are in the [voice input reference](../voice-input/README.md#speech-models).

- **A model still missing a day after a start-up** is a `speech model download started` with no `speech model downloaded` for that `model` within 3 hours. The release watch runs, on `otel.otel_logs` with `ServiceName = 'harold'`:

```
SELECT LogAttributes['model'] AS model,
       countIf(Body = 'speech model download started') AS started,
       countIf(Body = 'speech model downloaded') AS downloaded
FROM otel.otel_logs
WHERE ServiceName = 'harold'
  AND Body IN ('speech model download started', 'speech model downloaded')
  AND Timestamp > now() - INTERVAL 3 HOUR
GROUP BY model
HAVING started > downloaded
```

- **Error diagnosis:** the `reason` on `speech model not downloaded`, then the notice's command, run by hand.
- **Optimisation:** `ms` per GB from `speech model downloaded`.
- **The worker's load** is the log line `voice worker ready` with `load_ms` and `started_by` (`page` or `call`), and `voice worker stopped` with `reason` `no_page`, `call_ended` or `exited`. The tap-to-listening time is the `harold.call_ring` span. A worker `started_by = call` while a page was on screen means the page was not seen in time, and the call's `harold.call_ring` has `worker = loaded_now`; `cycles` of 4 or more is slow whichever the worker.
- No progress lines are logged. The phone shows progress, and the start and end lines say whether a model was missing or still downloading.

### The TTS server

The TTS server adds signals and two cards ([TTS server](../tts/README.md)). None is an alert: you hear a missing line.

| Signal | Kind | Fields | Answers |
|---|---|---|---|
| `harold.tts.speak` | span | `model`, `caller` (`desk`, `call`), `result` (`spoke`, `failed`), `first_audio_ms`, `loaded` (`true` when this request loaded the model), `device` (`gpu`, `cpu`, `apple_gpu`) | Does a line start fast, on each machine? |
| `tts gpu unavailable` | warn, once for each reason in a Harold run | `reason` (`no_gpu`, `no_driver`, `cuda_load_failed`, `gpu_out_of_memory`) | Is the voice slower than it can be? |
| `tts failed` | warn | `reason` (`worker_failed`, `model_downloading`, `model_missing`, `timed_out`), `model`, `caller` | When a line failed, why, and did the fallback speak? |
| `tts model downloaded` | info | `model`, `bytes`, `ms` | Which models are on each machine? |
| `tts model download failed` | warn | `model`, `reason` (`not_approved`, `unsafe_format`, `check_failed`, `network`; also `disk_full`, `not_allowed`, `not_found` and `failed`) | Did a download fail? |

`model` is the model's id (its repository name), never a path. No field holds the text.

- **Optimisation.** The p50 and p95 of `first_audio_ms` per day, by machine and `loaded`, on the first card, which also splits by `device`.
- **Issue detection.** On the Omarchy machine, `device = cpu` on `harold.tts.speak` means the voice ran on the CPU: expected 0 spans a day there. The Mac's worker always reports `apple_gpu`, so any `cpu` span comes from Linux. `tts gpu unavailable` is the nudge: expected 0 lines, at most one for each reason in one Harold run. The release watch runs:

```
SELECT count() FROM otel.otel_traces
WHERE SpanName = 'harold.tts.speak'
  AND SpanAttributes['device'] = 'cpu'
  AND Timestamp > now() - INTERVAL 1 DAY
```

and looks for the `tts gpu unavailable` line in `harold.log`. Either above 0 is a report. More than 2 on a day is worth a look. `tts model download failed` is a warn, which the release watch already looks for.

- **Error diagnosis.** From a failed line: `reason` and `model`, then `harold.log` for `tts failed` (the worker's own error output is discarded). From a slow line: `loaded` tells whether the load or the speech was slow. `harold tool tts models` shows each model's state and reason.

## How an alert reaches the chief

Each alert has its own generic HyperDX webhook, `webhook-<name>.json`, named `harold-alerts-<name>`. The alert's name is written into the webhook's body, because HyperDX 2.40 leaves `note` out of what a webhook body can see, so `{{note}}` renders as an empty string. Each webhook:

- posts to `http://host.containers.internal:4320/alert`, Harold's `alert_listen` port as the stack's containers see this Mac with Podman. With Docker, change the host to `host.docker.internal`. The port must match `alert_listen`.
- has the body `{"alert": "<name>", "state": "{{#if (eq status 'firing')}}firing{{else if (eq status 'resolved')}}cleared{{else}}{{status}}{{/if}}"}`. HyperDX's `status` `firing` is sent as `firing`, and `resolved` as `cleared`. Any other status (`no_data`, `pending`, `error`) is passed through as it is, so Harold answers 400 and types nothing. HyperDX 2.40 documents posts only for firing and resolving, but the body does not rely on that.

When the port is taken at start, Harold logs `alert address unavailable` (`reason = bind_failed`) and takes no alerts until it restarts with the port free. HyperDX's posts then fail, and no line reaches the chief.

Harold answers:

- `204` for a post naming one of the five alerts with state `firing` or `cleared`. It then types exactly `harold alert: <name> <state>` into the chief's pane: the first agent pane of the chief's tmux session in the folder `[notify] chief` matches.
- `400` for anything else, including another path or method. It logs `alert ignored (unknown alert)` and types nothing.

Harold also types lines of its own the same way. The first is `harold alert: delivery-given-up firing`, when it gives up a delivery after 10 failed tries (see the [notification reference](../notification/README.md#architecture)). It carries no message text. It is typed only when `alert_listen` is set and the address opened, with the checks below, as a posted line is. A post naming `delivery-given-up` is answered `400`.

The second is `harold alert: chief-message-held-back firing`, when a message or spoken line from the chief is held back by a channel's list (reason `not_listed`) on a channel that is set up and turned on. It is typed every time, a second one merges into one waiting, and a post naming it is answered `400`. Severity: warn. Action: the chief checks its folder against `[notify]` and the dash's agent list, then sends the message again or tells the user at the next chance. Expected rate: 0, because the default lists all name the chief's folder. Two queries check it, each over the last 7 days, and both gave 0 on 2026-10-21; more than 0 is the cue to act as above. The first counts the alerts typed, the second the cause, including alerts still waiting (`agent` is the chief's folder below the home folder, as `harold.notifications`' `agent` is):

```sql
-- alerts typed
SELECT count() FROM otel.otel_logs
WHERE Body = 'alert typed' AND LogAttributes['alert'] = 'chief-message-held-back'
  AND Timestamp > now() - INTERVAL 7 DAY;
-- the cause, including alerts still waiting
SELECT count() FROM otel.otel_logs
WHERE Body IN ('agent message dropped', 'spoken line dropped')
  AND LogAttributes['reason'] = 'not_listed' AND LogAttributes['agent'] = 'code/chief-of-pods'
  AND Timestamp > now() - INTERVAL 7 DAY
```

The `agent message dropped` or `spoken line dropped` line at that time gives `channel`, `kind`, `agent` and `pane_id`; if the `chief pane changed` lines just before show a `count` above 1, a second pane in the chief's folder is the place to look as well.

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
for name in phone-request-unavailable phone-request-slow wrong-agent slot-idle watch-cost-high; do
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
for name in phone-request-unavailable phone-request-slow wrong-agent slot-idle watch-cost-high; do
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
for name in phone-request-unavailable phone-request-slow wrong-agent slot-idle watch-cost-high; do
  WEBHOOK_ID=$(webhook_id "harold-alerts-$name") || break
  [ -z "$WEBHOOK_ID" ] || continue
  fill "webhook-$name.json" | hdx -X POST "$HYPERDX_URL/api/v2/webhooks" --data-binary @- | jq -r '.data.id // .id'
done
```

`harold-alerts` is then unused and may be deleted.

**Remove** an alert and its webhook that no file defines, such as `local-model-failing`, which left the files when Harold stopped running a local model. Find both ids by name, delete the alert first, then its webhook. The commands delete nothing else:

```sh
name=local-model-failing
id=$(alert_id "$name")
WEBHOOK_ID=$(webhook_id "harold-alerts-$name")
[ -z "$id" ] || hdx -X DELETE "$HYPERDX_URL/api/v2/alerts/$id"
[ -z "$WEBHOOK_ID" ] || hdx -X DELETE "$HYPERDX_URL/api/v2/webhooks/$WEBHOOK_ID"
```

After the create or update loops, run the check by hand. It compares HyperDX with the alert files and with the `harold-alerts-<name>` webhooks. It uses the same `HYPERDX_URL`. It reads the key from a file, not from `HYPERDX_API_KEY`. The file is `~/.config/harold/hyperdx-access-key`, or the file named by `HYPERDX_API_KEY_FILE`.

Make the file once from the local stack's provisioning output. The path below is where the stack lives on your machine; change it if it differs. Make it again when the stack is provisioned from scratch:

```sh
umask 077
jq -r .accessKey ~/code/sre/stack/hyperdx/state.json > ~/.config/harold/hyperdx-access-key
```

The loops above may take their key from the same file: `export HYPERDX_API_KEY=$(cat ~/.config/harold/hyperdx-access-key)`.

Run the check:

```sh
scripts/check_hyperdx_alerts.sh
```

The check prints `check_hyperdx_alerts: ok, <n> alerts` and exits 0 when HyperDX holds every alert in `hyperdx/` with its file's `threshold`, `thresholdType`, `interval` and `numConsecutiveWindows`. A file with no `numConsecutiveWindows` must match `null`, which is what HyperDX returns for it. The check also expects each alert to point at its own webhook, and no alert tagged `harold` to be without a file. Otherwise it prints one line per problem and exits 1. The check exits 2, with one line that starts `check_hyperdx_alerts: cannot check`, when `HYPERDX_URL` is not set, when the key file is missing, empty or not a regular file, when group or others can read it, when it lies inside a git checkout, when HyperDX cannot be reached, or when it does not answer with a list.

## Not tested until real use

The definitions were written from HyperDX 2.40's API source and the local stack's own provisioning, and checked by tests that read the files. The alerts and their webhooks were applied to the local stack on 2026-10-24, and `scripts/check_hyperdx_alerts.sh` checks that the stack holds them. Untested until real use:

- data arriving in HyperDX from either Mac;
- the dashboards, panels and the counter rule's SQL working as written in HyperDX 2.40, including the field names and the SQL tiles in ClickHouse;
- each alert firing in HyperDX as its file says;
- the counting from 0 at start reaching HyperDX as a first point of 0;
- the stack's containers reaching Harold's alert address, and each webhook's body arriving as written;
- the hooks' new fields on the installed agents.
