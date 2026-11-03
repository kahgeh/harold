# Architecture: Harold

What Harold is made of and how its parts talk, kept true by the pod that builds it. The architect reads this file before it reviews a spec or a merge.

## Purpose

Harold watches the coding agents in the user's tmux or Rex panes on each Mac, tells the user on the phone when one finishes, and routes the user's replies back to the right pane. It also hosts tools: the question tool, the backlog tool, the work tool and the release tool, which the chief of pods uses, and the watch tool, which any agent can use. The watch tool pushes its counts of running, paused and capped watches to the snapshot publisher through `ToolContext`.

## Components

```mermaid
flowchart LR
  subgraph hooksg["Hooks and the Claude Code mod"]
    hooks["hooks, Claude Code mod, OpenCode plugin"]
  end

  subgraph harold["Harold"]
    monitor["agent monitor"]
    stream["event stream"]
    projector["projector and reducer"]
    statedb["state database"]
    outbox["outbox"]
    handler["delivery handler"]
    channels["channels and channel listener"]
    routing["reply routing"]
    publisher["snapshot publisher"]
    toolhost["tool host: question tool, backlog tool, work tool, release tool, watch tool, telemetry tool"]
    voice["calls and voice worker"]
    tts["TTS server and TTS worker"]
    webapp["web app"]
    extras["activity summaries, timed prompts, alerts, telemetry and pod stats"]
    cli["CLI"]
  end

  subgraph dashg["the control-tower"]
    dash["the control-tower"]
  end

  subgraph scriptsg["Scripts"]
    ctl["haroldctl and the installer"]
  end

  api["API contracts: harold-api"]
  phone["phone"]
  panes["tmux or Rex panes"]

  hooks --> monitor
  panes --> monitor
  monitor --> stream
  channels --> stream
  stream --> projector
  projector --> statedb
  projector --> outbox
  outbox --> handler
  handler --> channels
  handler --> routing
  routing --> panes
  channels <--> phone
  webapp <--> phone
  statedb --> publisher
  publisher --> dash
  toolhost <--> dash
  webapp -->|"todos: ListQuestions, AnswerQuestion"| toolhost
  voice --> webapp
  voice --> tts
  handler --> tts
  cli --> monitor
  ctl -. installs and runs .-> harold
  api -. shared by .-> harold
  api -. shared by .-> dash
```

| Component | Responsibility | Code |
| --- | --- | --- |
| agent monitor | Finds agents with the process inventory, and works out their state from hooks, reports and screen adapters. It also reads the focused pane once a second and decides the waiting agents, which it serves in each snapshot. It clears a mark when the agent is seen, by focus or by the `AgentSeen` call. With an unknown focus it marks a finishing agent and clears it only when it is seen | `harold/src/agent/` |
| event stream and state database | Stores accepted observations and messages in order, and the current agent view, the outbox and the checkpoint | `harold/src/store.rs`, `harold/src/store/` |
| projector and reducer | Reads the event stream, works out each agent's state, and stages delivery work in the outbox | `harold/src/store.rs`, `harold/src/agent/reducer.rs` |
| delivery handler | Reads the outbox, and leaves out or sends each turn-end update. It speaks none and runs no model. The chief's update in a call comes from Haiku through the call answerer's `claude -p` path (`harold/src/voice_input/mac.rs`) | `harold/src/outbound/`, `harold/src/projector.rs` |
| channels and channel listener | Send to the phone by iMessage, and record the user's replies in the event stream | `harold/src/channels/` |
| reply routing | Decides which Mac owns a reply by its routing tag or the latest notifier, picks the pane by the tag or the other fallbacks, and types it there | `harold/src/inbound/` |
| snapshot publisher | Sends snapshots to clients over `WatchAgentStates`. It puts the watch counts that it receives from the watch tool in each snapshot, and it sends a new snapshot when they change. It never reads the watch tool's store | `harold/src/agent/snapshot.rs`, `harold/src/main.rs` |
| tool host, question tool, backlog tool, work tool, release tool, watch tool, knowledge tool, telemetry tool | Hosts tools with their own storage and gRPC services (ADR 0001). The watch tool runs a skill with a model on a schedule, every few minutes or on a cron expression, for a set time or with no end (an open-ended watch, which needs `[watch] daily_budget_usd`). Anyone can pause and resume a watch. A spent daily budget stops a watch with an end. It pauses an open-ended watch until the next local day. That watch is a capped watch, and nobody resumes it by hand. The tool also keeps one-time reminders, which run no model. See the [watch reference](../references/tools/watch.md). The telemetry tool takes declared counters, gauges and events from local panes, under a prefix that the caller's pane allows, and exports them as `agent.<name>`. An event carries declared values only, never free text. See the [telemetry tool reference](../references/tools/telemetry.md) and [ADR 0031](../adr/0031-agents-record-signals-through-harold-under-a-prefix.md). The release tool has no storage: it only reads. `harold tool release next` lists, for each product, the next release from the pod files and git (see the [release reference](../references/tools/release.md)). The control-tower and the web app are clients of the question tool. The work tool reads the question tool and the queue through Harold's own gRPC address on loopback (`ToolContext::grpc_addr`), as a local caller (ADR 0002), never through their stores. The knowledge tool indexes the vault by meaning with a local embedding model run by the embedding worker, in a tursodb database that buckwal keeps durable in a bucket, and answers the chief (`harold tool knowledge`), the web app's Notes page and the control-tower's `s` search. When `[knowledge.graph]` is on, it also keeps the **knowledge graph** in TypeDB, a separate store from the index: one graph writer checks, logs and applies every change, and two OpenData logs (a schema log and a data log) let Harold rebuild the graph from nothing. See the [knowledge reference](../references/tools/knowledge.md#the-knowledge-graph). See the diagram in the [knowledge reference](../references/tools/knowledge.md#how-the-parts-fit). | `harold/src/tools.rs`, `harold/src/tools/`, `harold/src/embed_worker.py` |
| calls, call answerer, voice worker and speech model | Voice calls with the chief, the call answerer (the small model that answers each turn and hands real work to the chief), the process that turns speech into text (it loads Whisper only), and the check of the Whisper model: Harold downloads it at start-up if it is missing, pinned to a commit and checked file by file ([ADR 0011](../adr/0011-harold-downloads-missing-speech-models-at-start-up.md)). A call also waits for the default TTS model | `harold/src/voice_input.rs`, `harold/src/voice_input/` (`answerer.rs`, `hand_over.rs`, `models.rs`), `harold/src/voice_worker.py`, `harold/src/voice_download.py` |
| TTS server and TTS workers | One TTS server on each machine, `harold.tts.v1.TtsService`, that turns text into audio for the desk voice, for calls and for `harold tool tts`. A TTS worker keeps one model loaded: the MLX worker on macOS, the ONNX worker on Linux (the NVIDIA GPU, with the CPU as the fallback). Kokoro is the default model. Other models come through the API and download only when they are on the approved model list ([ADR 0020](../adr/0020-each-machine-runs-one-tts-server-built-for-its-platform-and-downloads-only-approved-models.md), [TTS server reference](../references/tts/README.md)) | `harold/src/tts/`, `harold/src/tts_worker_mlx.py`, `harold/src/tts_worker_onnx.py`, `harold-api/proto/tts.proto` |
| web app | The phone's web app: the thread, push, calls, the todos, which it shows and answers through the question tool's service, and the spec pages, which it renders from each product's spec files and offers to share one at a time, and the page collections, which it serves from the folders named in `[[web_app.collections]]` (an `.html` page in a sandboxed frame with no script, an `.md` page rendered like a spec). A second, loopback-only listener, the specs listener, serves the same pages to the browser on the Mac. Its page records page loads, requests and script errors as spans (`telemetry.js` and one vendored OpenTelemetry bundle) and posts them to Harold's `POST /telemetry/traces`, which keeps only the fields it allows and forwards them to the collector | `harold/src/web_app/` (`specs.rs` for the spec pages, `collections.rs` for the page collections), `harold/static/web_app/`, `harold/templates/web_app/` |
| activity summaries | Optional work summaries that Claude Code writes from an agent's evidence | `harold/src/activity_summary.rs` |
| timed prompts | Text typed into a pane at a set time each day, or at the minutes of a cron expression | `harold/src/timed_prompts.rs`, `harold/src/cron.rs` |
| alerts | HyperDX alerts typed to the chief | `harold/src/alerts.rs` |
| telemetry, machine stats, pod stats and agent metrics | OpenTelemetry export, the machine's disk, memory, swap and CPU use with each hour's normal range (read every 60 s with `sysinfo`, kept in the hourly use file `data/machine-hourly-use.json`), and pod counts from the pod registry: slots, merges, specs by state, the time of each step, and the pods and agents that run. Pod stats also exports the free slots with work waiting (`pod.slots.idle_with_work`) and the work waiting in three lists (`pod.inventory.*`). For each spec, it exports the redo hours, the revisions after approval and the size (`pod.spec.effort`). It also exports the agents by state (`pod.agents`) and the time in each state, read from the agent snapshot, and each multiplexer call with its operation, outcome and time, recorded by `record_multiplexer_call`. Harold reads three small files that the pod's shared tools write once, `withdrawn/<id>.md`, `started/<id>.md` and `merged/<id>.md`, and never writes in the pod folder. `pod_stats` also reads the agent snapshot for pane ids and folders ([ADR 0021](../adr/0021-the-pod-records-a-specs-withdrawal-start-and-merge-in-small-files-that-harold-reads.md)). `pod_stats.rs` (`read_specs`) is the one place that decides a spec's state, for the pod metrics and for the release tool. It also gives each pane its role, product and spec id from the last poll. `turn_complete` uses that lookup to add each turn's tokens to `harold.turn.tokens` (`kind`, `model`, `role`, `pod.product`). It adds a build worker's tokens to `harold.turn.tokens.by_spec` (`spec.id`, `kind`). It also adds `role` and `pod.product` to `harold.turns` | `harold/src/telemetry.rs`, `harold/src/machine_stats.rs`, `harold/src/pod_stats.rs` |
| CLI | `harold` and its subcommands: `tool`, `speak`, `send-message-to-away-device` and the rest | `harold/src/cli.rs` |
| haroldctl and the installer | Install, start, stop and check the service | `scripts/` |
| hooks, Claude Code mod, OpenCode plugin | Tell Harold what an agent did. The Claude Code turn-end hook also sums the turn's input, output, cache-read and cache-write tokens from the transcript and sends them with the model. The turn starts at the last user line that is not a tool result and has no `sourceToolUseID`, so a Skill load inside a turn does not start a turn, but an injected task notification does | `hooks/` |
| control-tower | The control-tower has two views, switched with the palette command `view` or the key `V`. The agent view shows the agent rows (it marks the waiting agents that Harold serves, and it calls `AgentSeen` when it jumps to a pane. With an older Harold, which sends no `focus_known`, it keeps its own list, probe and poll), the todo rows, the focus lines (it reads the work tool's list for them, and the queue's Now item from an older Harold), voice and messaging switches, and the notes search (`s`) over the knowledge tool. The work view lists the open work records at `focus` and `next`. It reads the work tool's `List` and `Show` every 2 seconds and never writes. `harold start-control-tower` starts it. With `--remote`, it runs the remote control: a numbered list of the chief and the PnEMs in a floating Rex layer. Its rows show the roles `Chief`, `Coordinator` and `PnEM`, which Harold sends in the `role` field of each pane. The remote reads the `WatchAgentStates` stream, keeps the PnEM order through `GetRemoteOrder` and `SetRemoteOrder`, and closes its own layer with Rex's `session.close_layer` before it exits | `control-tower/` |
| API contracts | The protobuf definitions shared by Harold, the control-tower and the tools | `harold-api/` |

## Interfaces

What other components or products depend on, each with where its contract is written:

- **The Harold gRPC service** (`harold-api/proto/harold.proto`): `WatchAgentStates`, `TurnComplete`, `ReportAgentState`, `SpeakLine`, `SendMessageToAwayDevice`, `Call`, `SetTtsMuted`, `SetMessagingPaused`, and `AgentSeen`. Contracts: the proto and the [agent monitor reference](../references/agent-monitor/README.md#grpc-contract).
- **The TTS service** (`harold-api/proto/tts.proto`, package `harold.tts.v1`): `Speak`, `ListModels`, `AddModel`, `RemoveModel` and `SetDefault`, on Harold's gRPC address, for local callers only. Contract: the [TTS server reference](../references/tts/README.md).
- **The tool services** (`harold-api/proto/tools/questions.proto`, `harold-api/proto/tools/backlog.proto`, `harold-api/proto/tools/release.proto`, `harold-api/proto/tools/watch.proto`, `harold-api/proto/tools/knowledge.proto`). Contracts: the [tools references](../references/tools/README.md).
- **Commands:** `harold` and `haroldctl`. Contracts: the [operation reference](../references/operation/README.md) and the tools references.
- **Hook payloads** from Claude Code, Codex and OpenCode. Contract: [Set up agent monitor hooks](../how-tos/setup-agent-monitor-hooks.md).
- **Settings:** `default.toml` and `local.toml`, read in that order, with `local.toml` laid over `default.toml`. Contract: `harold/config/local.template.toml` and each feature's reference page.
- **The web app's HTTP routes**, and the specs listener's page routes (two spec routes and the four page collection routes). Contract: the [web app reference](../references/away-app/README.md).
- **Lines Harold types into panes:** the answers line, alerts, timed prompts, the chief's restart reminder, phone replies (a web app reply may end with `📷 <path>`, ADR 0010), `WATCH` lines, `REMIND` lines and hand-overs from a call. A hand-over is the one writer that may type into a `working` Claude Code pane (ADR 0009). The watch tool types two forms that need a note. The `REMIND <name>: <text>` line carries text that a local agent gave to `harold tool watch remind`. The `WATCH <name> paused: daily budget of $1.00 spent, resumes at 00:00` line is Harold's own fixed text. ADR 0002 covers both, since Harold trusts local callers. The limits on the `REMIND` line are the fixed `REMIND <name>: ` prefix, which shows the chief that an agent wrote the line, and the cut to 300 characters with control characters removed. Contracts: the [questions reference](../references/tools/questions.md), the [call answerer reference](../references/voice-input/call-answerer.md), the [watch reference](../references/tools/watch.md), the [telemetry reference](../references/telemetry/README.md), the [timed prompts reference](../references/timed-prompts/README.md) (which also covers the restart reminder) and the [inbound routing reference](../references/inbound-message-routing/README.md).
- **The alert address:** `POST /alert` on `[telemetry] alert_listen`, where HyperDX's webhooks post. Contract: the [telemetry reference](../references/telemetry/README.md).
- **Machine stats:** Harold reads its machine every 60 seconds when export is on, and sends gauges and `harold.machine.past_normal` to the collector. The alerts `machine-past-normal` and `disk-low` come back through the alert address. Contract: the [telemetry reference](../references/telemetry/README.md#machine-stats).
- **The pod registry:** Harold reads the vault's `products.toml` for pod stats. Contract: the [telemetry reference](../references/telemetry/README.md).
- **The routing tag** on replies. Contract: the [inbound routing reference](../references/inbound-message-routing/README.md).
- **The remote control:** `GetRemoteOrder` and `SetRemoteOrder` on the Harold gRPC service, and the `role` and `pod_product` fields of `AgentPaneState`. Harold keeps the order of the PnEMs in `remote-order.json` in its data folder. Contract: the [remote control reference](../references/control-tower/remote-control-rex-calls.md).
- **Files in Harold's data folder:** under the store folder, the event stream (`log/`) and `harold-state.db`; beside it, `remote-order.json` (the order of the PnEMs in the remote control, file mode 0600), `pod-notices.json` (the pod-file faults Harold has warned about, so a restart does not warn again, file mode 0600), and each tool's database (`tools/<tool>`; the telemetry tool's `tools/telemetry/telemetry.db`; the work tool's `tools/work/work-<YYYY-MM-DD>.db` files, the newest being the current one; the knowledge tool's `tools/knowledge/index` is the local copy of a database buckwal keeps in a bucket, and `tools/knowledge/worker` holds the embedding worker's script; the knowledge graph keeps `tools/knowledge/graph/schema-log`, `tools/knowledge/graph/data-log` and `tools/knowledge/graph/typedb`, all at mode 0700), the web app's key (`web-push/vapid.pem`), its paired devices (`web-app/devices.json`, fingerprints only) and pairing lock (`web-app/pairing-lock.json`), both mode 0600 in a folder at 0700, the echo test's runs (`echo-test/`, one JSON file per run) and the web app's photos (`app-photos/`, folder mode 0700, files 0600; the hourly prune deletes an upload no reply names after 1 hour, and a photo when its thread row goes, at 30 days). Also beside it, `speech-models/` (folder mode 0700): the download script `voice_download.py` (file mode 0600) and, for each speech model Harold has checked, a record of its files' paths, sizes and sha256 (`<org>--<name>@<commit>.json`, mode 0600). Contracts: the [operation reference](../references/operation/README.md), the [tools references](../references/tools/README.md) and the [web app reference](../references/away-app/README.md).
- **Telemetry:** export is on by default, over OTLP gRPC to the collector at `http://localhost:4317`, and `[telemetry] export = false` sets it off. Harold also forwards the web app's browser spans as OTLP JSON to `[telemetry] otlp_http_endpoint`, `http://localhost:4318`, because a browser cannot speak gRPC. Contract: the [telemetry reference](../references/telemetry/README.md).

## Decisions

One line per architectural decision, oldest first. Each date is the commit that brought the decision into the page that records it.

- 2026-08-24: Identity comes before state: an incarnation, not a pane, owns a state, so a new agent in an old pane starts at Unknown. [Agent monitor reference](../references/agent-monitor/README.md#incarnation-identity)
- 2026-08-24: Hooks win during a grace period, then conclusive screen evidence decides, so a late repaint or a missed hook does not leave a wrong state. [Agent monitor reference](../references/agent-monitor/README.md#reconciliation-contract)
- 2026-08-24: The projector stages the outbox in the same transaction as the state and the checkpoint, so a retryable delivery failure never undoes a recorded event. [Agent monitor reference](../references/agent-monitor/README.md#projection-and-storage)
- 2026-10-11: Two Macs run as hub and member, with a shared secret on the peer RPCs, because Telegram lets only one process read a bot's replies. [Inbound routing reference](../references/inbound-message-routing/README.md)
- 2026-10-21: The hub and member setup of 2026-10-11 is removed with Telegram. Two Macs share replies through iMessage, with no peer RPC, and the gRPC server listens on the Mac only. [ADR 0008](../adr/0008-harold-reaches-the-phone-by-imessage-only.md)
- 2026-10-16: The event stream lives in an OpenData log and the state in Turso, both inside the Harold process, so no other service has to run. [Operation reference](../references/operation/README.md)
- 2026-10-18: The web app is served on loopback and carried to the phone by `tailscale serve`, so nothing listens beyond the Mac. [Web app reference](../references/away-app/README.md)
- 2026-10-18: ADR 0001, tools are modules Harold hosts, so a new tool needs no change to the core store or the Harold gRPC service. [ADR 0001](../adr/0001-tools-are-modules-hosted-by-harold.md)
- 2026-10-18: ADR 0002, tool services trust local callers, because the gRPC listener has no transport authentication and only the peer RPCs carry a secret. [ADR 0002](../adr/0002-tool-services-trust-local-callers.md)
- 2026-10-18: ADR 0003, pod merge counts save the tip first, so a failed write changes nothing and a later poll counts the merges. [ADR 0003](../adr/0003-pod-merge-counts-save-the-tip-first.md)
- 2026-10-20: ADR 0004, tools may read Harold's agent list and pod facts through two read-only ports, so the watch tool can hand a model what a pod check needs. [ADR 0004](../adr/0004-tools-may-read-agents-and-pods.md)
- 2026-10-20: ADR 0005, a tool may run a hosted model with the tools a skill allows, unchecked, because the skill's author decides what a run may do. [ADR 0005](../adr/0005-a-tool-may-run-a-hosted-model-with-the-tools-a-skill-allows.md)
- 2026-10-20: ADR 0006, answers go to the chief early, and a set-aside question returns after a line is typed, a `later` or a dismiss that leaves nothing waiting and no answer unsent, so "later" still means after everything else. [ADR 0006](../adr/0006-answers-go-to-the-chief-early.md)
- 2026-10-21: ADR 0007, the web app reaches the todos through the question tool's gRPC service, as the dash does, so a phone on the tailnet can answer them. [ADR 0007](../adr/0007-the-web-app-reaches-todos-through-the-question-service.md)
- 2026-10-21: ADR 0008, Harold reaches the phone by iMessage only and its gRPC server stays on the Mac, so every Mac reads every reply from its own `chat.db` and nothing passes between Macs. [ADR 0008](../adr/0008-harold-reaches-the-phone-by-imessage-only.md)
- 2026-10-23: ADR 0009, a call is answered by a small model and real work goes to the chief as a hand-over, typed into the chief's pane even while it is busy, so a call no longer waits for a large model. [ADR 0009](../adr/0009-a-call-is-answered-by-a-small-model.md)
- 2026-10-23: ADR 0010, photos reach the chief as files: the phone shrinks a photo, Harold stores it in `app-photos/` without decoding it, and the line typed to the chief names its absolute path. [ADR 0010](../adr/0010-photos-reach-the-chief-as-files.md)
- 2026-10-23: ADR 0011, Harold downloads a missing speech model at start-up, from Hugging Face, pinned to a commit and checked file by file, with no token, while the voice worker stays offline; on the iPhone's hotspot the start-up download waits, and a tapped call starts it. [ADR 0011](../adr/0011-harold-downloads-missing-speech-models-at-start-up.md)
- 2026-10-23: ADR 0012, browser libraries are vendored as one bundle built from a pinned, audited npm lockfile, and browser spans reach the collector only through Harold's route, which keeps allowed fields and cuts URLs to their path. [ADR 0012](../adr/0012-browser-libraries-are-vendored-as-one-bundle-built-from-a-pinned-audited-npm-lockfile.md)
- 2026-10-23: ADR 0013, the web app admits only paired devices, by a device-secret cookie whose fingerprint Harold keeps, and a device is paired by typing a fixed code kept in a file outside the repository, which five wrong tries lock for good. [ADR 0013](../adr/0013-the-web-app-admits-only-paired-devices-by-a-device-secret-cookie-whose-fingerprint-harold-keeps.md)
- 2026-10-24: ADR 0014, the vault is indexed by meaning inside Harold, with a local model and a buckwal-durable tursodb, so the chief, the web app and the dash can find a vault note by what it means. [ADR 0014](../adr/0014-the-vault-is-indexed-by-meaning-inside-harold-with-a-local-model-and-a-buckwal-durable-tursodb.md)
- 2026-10-24: ADR 0015, the question tool reads the spec files the chief names, only inside a registered product's specs folder, so a todo whose spec changed after it was asked is marked and cannot be answered. [ADR 0015](../adr/0015-the-question-tool-reads-the-spec-files-the-chief-names.md)
- 2026-10-25: "Relegate to idea" replaced Later on a question, in the dash and the web app: it goes through `AnswerQuestion` as the kind `RELEGATE`, `DeferQuestion` and the deferred state are gone, and the answers line asks the chief to keep the question as an idea note. The notes in [ADR 0006](../adr/0006-answers-go-to-the-chief-early.md) and [ADR 0007](../adr/0007-the-web-app-reaches-todos-through-the-question-service.md) say what no longer holds.
- 2026-10-25: ADR 0016, typed relations between vault notes live in TypeDB, rebuilt from two OpenData logs, so the chief's relations survive a lost TypeDB database. [ADR 0016](../adr/0016-typed-relations-between-vault-notes-live-in-typedb-rebuilt-from-two-opendata-logs.md)
- 2026-10-25: ADR 0017, Harold serves each product's specs as pages in the web app and on a loopback-only listener, todo links point at those pages, and a spec leaves the machines only when the user presses **Share**, one spec at a time. [ADR 0017](../adr/0017-harold-serves-the-specs-and-a-spec-leaves-the-machines-only-when-the-user-shares-it.md)
- 2026-10-25: ADR 0018, every post and delete from a web app page goes through Datastar, which sends `Datastar-Request: true`, and the same-origin check reads that header. The call's raw audio post and the browser trace exporter are the two exceptions the user approved; only the user approves another. [ADR 0018](../adr/0018-every-web-app-post-goes-through-datastar-and-only-the-user-approves-an-exception.md)
- 2026-10-26: ADR 0020, each machine runs one TTS server, built for its platform (the MLX worker on macOS, the ONNX worker on Linux), with Kokoro by default and other models only when they are on the approved model list. The user approved two exceptions: Kokoro for Linux from a GitHub release pinned by SHA-256, and NVIDIA's proprietary CUDA wheels. [ADR 0020](../adr/0020-each-machine-runs-one-tts-server-built-for-its-platform-and-downloads-only-approved-models.md)
- 2026-10-26: ADR 0021, the pod's shared tools write `withdrawn/`, `started/` and `merged/` files once, and Harold reads them first, with the older git and worktree sources as a fallback. `pod_stats` also reads the agent snapshot, pane ids and folders only, to count pods and agents by role. [ADR 0021](../adr/0021-the-pod-records-a-specs-withdrawal-start-and-merge-in-small-files-that-harold-reads.md)
- 2026-11-03: ADR 0028, work records live in their own Turso files, and Harold itself moves the open ones to a new file every `rotation_days` days after a count check. [ADR 0028](../adr/0028-work-records-live-in-rotating-turso-files-of-their-own.md)
- 2026-11-03: The release tool only reads: it takes each spec's state from `read_specs` in `pod_stats.rs`, so the pod metrics and the next release use one rule. They differ only where the poller's record of worktrees seen to go is the only source of a merge. [Release reference](../references/tools/release.md)
- 2026-11-03: Pod signals. Pod stats records free slots with work waiting, redo, spec size, agents by state, and work waiting in three lists. The release tool publishes the fourth list, `merged_not_released`, itself, with the names that pod stats exports, so pod stats never calls the release tool. No ADR. [Telemetry reference](../references/telemetry/README.md)
- 2026-11-03: ADR 0030, Harold serves page collections: folders of pages named in `[[web_app.collections]]`, behind the web app's checks and on the specs listener, and an HTML page is sandboxed with no script and nothing from outside. [ADR 0030](../adr/0030-harold-serves-page-collections-sandboxed.md)
- 2026-11-04: Turn tokens. Harold counts each Claude Code turn's tokens by kind, model, role and pod, and a build worker's tokens by spec. The role, product and spec id come from the last pod poll. No ADR. [Telemetry reference](../references/telemetry/README.md)
- 2026-11-04: ADR 0031, a tool takes declared counters, gauges and events from local panes under a prefix that the caller's pane allows, and exports them as `agent.*`. An event carries declared values only, never free text. The prefix check is a mix-up check, not security. [ADR 0031](../adr/0031-agents-record-signals-through-harold-under-a-prefix.md)

## Departures from the standards

Each place Harold does not follow the organisation's `knowledge/standards.md`, why, and until when:

- **Other languages beside Rust.** The voice worker (`harold/src/voice_worker.py`), the TTS workers (`harold/src/tts_worker_mlx.py` and `harold/src/tts_worker_onnx.py`), the embedding worker (`harold/src/embed_worker.py`, a second Python worker beside it), the installer (`scripts/*.py`) and the turn-complete hooks (`hooks/providers/`, `hooks/shared/`) are Python, `scripts/install.sh` and `scripts/bootstrap.sh` are shell, the Claude Code mod is TypeScript, and the OpenCode plugin and the web app's scripts (`harold/static/web_app/`) are JavaScript. Each fits the program it extends, and the standard raises another language as a question, not a breach. Until: not set.
- **Proprietary packages and a model source that is not Hugging Face (Linux TTS worker).** The Linux TTS worker's Python environment holds NVIDIA's CUDA and cuDNN wheels, under NVIDIA's proprietary licence, and Kokoro for Linux comes from a GitHub release that publishes no checksum, pinned in code by size and SHA-256. The user approved both (Q106; [ADR 0020](../adr/0020-each-machine-runs-one-tts-server-built-for-its-platform-and-downloads-only-approved-models.md), the audit rows 13 and 14). Until the SHA-256 values are recorded, the Linux download refuses.
- **The TypeDB server is a long-lived child process that is not a Rust crate.** When `[knowledge.graph]` is on, Harold starts the TypeDB Community Edition server (MPL-2.0) on the first graph call, on `127.0.0.1` only, and stops it after `idle_minutes` without a call, as it does the embedding worker. You install the binary, and Harold downloads nothing. The supply-chain auditors do not cover a binary, so the check is by hand: the [dependency audits](../references/dependency-audits.md) hold its row. Until: not set.
- **Two paths to Turso.** Harold's `turso =0.8.3` (`mimalloc`) and buckwal's `turso_core =0.8.3` (`fs`, `json`, `uuid`, `encryption`, `conn_raw_api`) both resolve to the one `turso_core 0.8.3` in `Cargo.lock`, with the features joined. buckwal is a git dependency pinned to a commit (`harold/Cargo.toml`), not a crates.io release. Until: buckwal is published or the pin moves.
- **buckwal's telemetry runs through Harold's subscriber.** Harold never calls `buckwal::telemetry::init`, which installs a second global subscriber. The export filters and `ALLOWED_FIELDS` in `harold/src/telemetry.rs` accept the target `buckwal`, so its spans and log records are exported under `service.name = harold`. Until: not set.
- **axum's version.** Harold uses axum `0.8.8` (`harold/Cargo.toml`), unpinned, where the standard names `=0.8.7`. No one has said why. Until: not set.
- **Dependency audits** live in the table in `docs/references/dependency-audits.md`, linked from the README, to keep the front page short. Until: not set.
- **Licences of shipped third-party code** are in `LICENSES/`: Datastar (the web app's script), OpenTelemetry JS (the web app's browser bundle, built by `scripts/build_web_otel.sh`), Mermaid and its packages (the spec pages' browser bundle, built by `scripts/build_web_mermaid.sh`) and OpenSSL (linked statically into the release binaries).

## How it works

Harold inventories configured agent processes in the panes of its multiplexer (tmux or Rex, see [Two multiplexers](#two-multiplexers)), combines explicit lifecycle hooks with provider-specific visible-screen evidence, and stores the resulting current state durably. Existing stop hooks also report completed turns for turn-end updates and non-destructive work-summary updates. A completed turn gives the state `done`.

An agent has one of six states: `working`, `blocked`, `done`, `idle`, `error` or `unknown`. Two functions group them, and no code compares with one state by hand. `is_working()` is true for `working` and `blocked`. `is_at_rest()` is true for `done`, `idle` and `error`. `unknown` is neither. A blocked agent waits for you inside its turn, so Harold treats it as a working agent and types no differently.

The public agent-state stream starts with a complete current snapshot, then sends later revisions. Raw pane captures remain inside the screen adapter; only classified state and a normalized work summary can cross the acquisition boundary. The [agent-monitor reference](../references/agent-monitor/README.md) and the [hook setup guide](../how-tos/setup-agent-monitor-hooks.md) give the detail.

### Two multiplexers

The setting `[agent_monitor] multiplexer` picks the one multiplexer Harold works with: `"tmux"` (the default) or `"rex"`. Harold reads it once, at start. To switch, change the setting and start Harold again ([ADR 0022](../adr/0022-harold-works-with-one-multiplexer-chosen-at-start-and-rex-has-its-own-implementation-of-each-port.md)).

Each port has one implementation for each multiplexer:

| Port | tmux | Rex |
|---|---|---|
| `AgentInventoryPort` | `TmuxAgentInventory`: `tmux list-panes -a`, then `ps -t` | `RexAgentInventory`: `rex ls`, `session.view`, and `process` for each block |
| `PaneCapturePort`, `VisibleScreenPort` | `TmuxPaneCapture`, `TmuxVisibleScreen` (`tmux-visible-v1`): `tmux capture-pane` | `RexPaneCapture`, `RexVisibleScreen` (`rex-visible-v1`): a tail read with the block methods `format` and `blob_read` |
| `Ports` typing steps | `tmux send-keys` | a paste event (`block.event`) and `rex send-key` |

At start, Harold builds the inventory and the screen reader for the chosen multiplexer. A typing step acts by the form of the pane id it gets. The two forms are `%47` for a tmux pane and `block:<id>` for a Rex block. A pane id of the other form is refused (a third form, for agents on Rex hosts, is below). Both multiplexers show the pane label `<session>:<window>.<pane>`, and Harold stores it in `tmux_target`.

With Rex, the program status record of a block is a fourth source of evidence. Harold keeps one `rex events` child process for each Rex session. An event only triggers a `program_status` call for the block, and the answer of the call is the record. A record of the agent's pid gives a state with no screen read. Without a usable record, Harold reads the screen. The record's `msg` is the agent's current step, which goes only to the control-tower. [ADR 0024](../adr/0024-agents-have-six-states-and-harold-takes-them-from-the-terminal-when-it-reports-them.md) gives the decision.

Harold never starts a Rex server. A Rex call that fails makes the listing fail, and the snapshot keeps its panes. The code is in `harold/src/agent/rex.rs`.

### Rex hosts and the pane id forms

With `multiplexer = "rex"`, Harold can also list the agents on other machines. Each machine is a **Rex host**: another Rex server that this Mac's Rex knows by a label, such as `omarchy`. The setting `[agent_monitor] rex_hosts` names the labels. Harold reaches a host with `rex -S <label>` and only reads: it types nothing there, and it reads no screen and no transcript there. It keeps these agents in memory, outside the agent snapshot, the reducer and the store. A host row can be focused. Enter on it moves the user's Rex app with `rex -S <label>` and that host's own session and block ids. Harold still types nothing into an agent on a Rex host.

Every agent row carries a window name: the name of the window that holds its pane. The names of this Mac's windows live in memory in `WindowNames`, which the scan fills. A host row takes its window name from its slot.

A pane id has three forms:

| Where the pane is | Form | Example |
|---|---|---|
| This Mac, tmux | `%<n>` | `%47` |
| This Mac, Rex | `block:<id>` | `block:01a1955f-74fd-7e5f-b71b-99ed90790ae0` |
| A Rex host | `rex:<label>/block:<id>` | `rex:omarchy/block:01a19080-9102-7989-944c-5dcb46fb894c` |

The first two forms are the **local forms**. They do not change, and a bare `block:<id>` always means this Mac. The third form is the **host pane id**. It holds the label and not the host's `host_id`, because Harold knows the label at load, `rex -S` takes a label, and a reader can read it.

Every writer takes only a local form. A writer is code that sends a pane id to the Mac's tmux or Rex: phone replies, timed prompts, alerts, the chief's typing, screen reads. All these writers call one function first, `pane_route::route_pane(pane_id)`. `route_pane` decides by the shape of the id alone. It returns Rex for a valid `block:<id>`, tmux for a valid `%<n>`, and a refusal for every other id. A host pane id gives the reason `on_rex_host`, and any other id gives `invalid_pane_id`. The route runs before any lock, log line, `tmux` call or `rex` call. The Rex layer checks again: `rex::find_session` returns no session for an id that is not a valid `block:<id>`. The control-tower has its own checks: its first `Navigate` arm and `jump_to` refuse a host row.

The daemon's entry points apply the same rule. `turn_complete` drops a report whose pane id is not a local form, and `send_message_to_away_device` and `speak_line` refuse a host pane id as `src_id`. No host pane id enters the store, so no stored data changes.

### Follow one task

Suppose an agent is idle and you submit “Fix the login error.” With lifecycle hooks configured, the update reaches the control-tower like this:

1. The agent's start hook tells Harold that work has begun and supplies the task text. A **hook** is a small script a provider runs to report an agent action.
2. Harold checks which agent process is running in that pane and records the observation in the event stream.
3. Harold reads the recorded observation and updates its stored view of that agent to `working`, with the submitted instruction available as its work summary.
4. After saving that view, Harold publishes it to the control-tower. The control-tower can now show `working` and “Fix the login error.”

When the agent finishes, a completion hook can report its reply. Harold updates the agent to `done` and separately considers whether to tell you. Finishing a turn does not by itself prove the requested fix succeeded.

This flow has two useful records: **what Harold observed**, and **what Harold currently knows about each agent**. Keeping the observations lets Harold rebuild the current view after a restart.

### The main parts

The following diagram shows how observations become the control-tower's state and how messages lead to delivery work. These are responsibilities inside Harold; they are not separate services.

```mermaid
flowchart TD
    A["Agent hooks and terminal observations"] --> B["Agent monitor: identify the agent and record observations"]
    B --> C["Event stream: persistent history"]
    D["Incoming phone replies"] --> C
    C --> E["Projector: apply recorded events"]
    E --> F["State database: current agent view"]
    E --> G["Outbox: pending delivery work"]
    F --> H["Snapshot publisher: send saved state to the control-tower"]
    G --> I["Delivery handler: send turn-end updates or route replies"]
```

The **agent monitor** checks running processes, accepts hook reports, and reads terminal evidence. It handles agent observations one at a time so competing inputs cannot independently overwrite the current state.

The **event stream** stores accepted observations and messages in order. Harold keeps the event stream in an OpenData log (`opendata-log`, which stores its data with SlateDB on local disk) and its state in the Turso database `harold-state.db`, both inside the Harold process, with no other service.

The **projector** reads that history and updates the state database. For agent observations, a set of decision rules called the **reducer** determines the resulting state. The projector saves that state along with its position in the history, called a **checkpoint**. This records how far it has processed.

The **outbox** is a persistent list of delivery work, such as sending a turn-end update or routing a phone reply. The projector stages that work in the same database transaction as its state updates and checkpoint. Actual delivery happens afterward, so a retryable channel failure can remain pending without undoing the recorded event.

The **snapshot publisher** sends the saved agent view to clients through the `WatchAgentStates` gRPC stream. A snapshot is the complete current view. The control-tower receives one when it connects, followed by updated snapshots, so it can reconnect without reconstructing every change it missed.

See the [agent monitor reference](../references/agent-monitor/README.md#projection-and-storage) for the storage and API contracts.

### The question tool's todos

The question tool holds the chief's todos, of two kinds: questions to answer and instructions to follow by hand. Both live in the same `questions` table. The control-tower stores each tick of instructions' step with `TickStep`, in the `ticks` column, so the ticks survive a control-tower restart. The control-tower stores each pick with `SavePicks` in the `picks` column of the `questions` table, so picks survive a control-tower restart and carry to the next question version for unchanged decisions, and the chief reads them with `ShowQuestion`. The `done` and `ignore` action rows store a `DONE` or `IGNORE` answer, which goes in the next answers line at once. This stays inside the question tool, so it needs no ADR beyond ADR 0001, except for one thing: the tool reads only the spec files the chief names, and only inside a registered product's specs folder, to mark a changed spec ([ADR 0015](../adr/0015-the-question-tool-reads-the-spec-files-the-chief-names.md)). See the [questions reference](../references/tools/questions.md).

### The spec pages and the specs listener

Harold shows each product's specs as pages, so a spec stays on the user's machines unless the user shares it. `web_app/specs.rs` reads the pod registry through the `Products` port, as the question tool does, and renders the files `<repo>/.superpowers/pod/specs/<id>.md` with `pulldown-cmark`. Raw HTML shows as text, and Mermaid draws the diagrams in the page, from one vendored bundle. The web app serves the pages at `/specs` and `/specs/{product}/{id}`, behind the login, pairing and same-origin checks. The **Share** button posts `/specs/{product}/{id}/share`, and Harold puts one fixed reply in the thread for the chief, which has the PnEM publish that spec as a private artifact.

A second listener, `[web_app] specs_listen` (`127.0.0.1:4331`), serves the same two page routes, the four page collection routes and three static files to the browser on the Mac. It is open only with `[telemetry] pod_registry` or a configured page collection. It has no login check and answers only `GET`, so any local process can read the specs, as it can read the files, and none can make the chief publish one. `tailscale serve` does not forward to it. The question tool gives each todo its `spec_page`, and the listing gives `local_specs_url`, so the web app card and the control-tower `o` open the spec page. See [ADR 0017](../adr/0017-harold-serves-the-specs-and-a-spec-leaves-the-machines-only-when-the-user-shares-it.md) and the [web app reference](../references/away-app/README.md#specs).

Every post and delete from a web app page goes through Datastar. A script sets its signals with Datastar's `mergePatch` and starts the request with Datastar's `post` action; a template uses `@post`. The same-origin check accepts a post or a delete only with `Datastar-Request: true`. It accepts `application/octet-stream` only on `POST /call/audio`, and `application/json` only on `POST /telemetry/traces`. These two are the approved exceptions, and only the user approves another. Every moved post sets `retry: 'never'`, `retryMaxCount: 0` and `openWhenHidden: true`, and an asset test fails on a `fetch` post that is not the call audio. See [ADR 0018](../adr/0018-every-web-app-post-goes-through-datastar-and-only-the-user-approves-an-exception.md).

### The work files and the rotation

The work tool keeps the chief's work records. Each work file is one Turso database with three tables: `records`, `decisions` and `links`. The files are in `tools/work/` in Harold's data folder, named `work-<YYYY-MM-DD>.db` by the day each starts. The newest name is the current file.

Harold rotates the files by itself. At start, and once a day, it checks the age of the current file. When the file is `[work] rotation_days` days old, Harold writes the open work records, with their decisions and links, to a new file under a temporary name. It counts the open work records, decisions and links in both files. Only when the counts match does it rename the new file and use it. Then it opens the old file read-only. A failed rotation leaves the old file current, logs `work rotation failed` once, and tries again at the next daily check.

`list` and `show` read the current file. `history` and `search` read the files newest first. The first file that holds a name gives the answer. The [work tool reference](../references/tools/work.md) has the details.

### The watch tool's model launch

The watch tool is the one place where Harold starts a model that can use tools. Its trust boundary, step by step:

- **Who names what.** Any local agent runs `harold tool watch start` and names the watch, its skill and, by its working directory, its folder. The tool does not trust the folder: it must be an absolute path to an existing folder, and Harold stores its canonical form. The skill is a folder name under `<folder>/.claude/skills/`, and its `SKILL.md` must lie, links followed, inside that folder or inside a watch skill folder the user lists in `[watch] skill_folders` (so a skill kept in the chief-of-pods repository or an organisation repository can be linked in). This is checked at `start` and again at every run. A watch skill folder is shared trust: anyone who can change a listed folder changes what every watch linking there does at its next run.
- **What the model may use.** The skill's `allowed-tools` goes to Claude Code as `--allowedTools`, exactly as written and unchecked. The skill's author decides what a run may do ([ADR 0005](../adr/0005-a-tool-may-run-a-hosted-model-with-the-tools-a-skill-allows.md)). The caller can change the skill while the watch runs.
- **What the model sees, and where it goes.** The model gets a fixed system prompt (the answer format), the skill's text and the facts from the `agents` and `pods` ports ([ADR 0004](../adr/0004-tools-may-read-agents-and-pods.md)). The facts, the skill's text, the files the run reads and the output of the commands it runs go to Anthropic's hosted model.
- **What comes out.** Harold types one line of at most 300 characters, in the fixed `WATCH` forms and the `REMIND` form, and only into the chief's pane or the pane of the agent in the watch's own folder, under the timed prompts' checks. Nothing from the run reaches a log or an export.
- **A reminder runs no model.** The tool types `REMIND <name>: <text>` once, with the same checks. The text is the caller's own, cut to 300 characters with control characters removed.
- **The clock.** The tool reads the local offset from `/bin/date` at each use, so a daylight-saving change needs no restart.

See the [watch reference](../references/tools/watch.md).

### Why the agent monitor uses several sources

A start hook may say an agent is working while its terminal still displays the previous idle screen. Later, a completion hook might be missed even though the terminal clearly shows the agent idle at its prompt. Harold needs rules for both situations.

Each source answers a different question:

| Source | What it tells Harold | Limitation |
| --- | --- | --- |
| Process inventory | Which agent process is present in each pane | Presence alone does not establish working or idle |
| Lifecycle hooks | The agent explicitly started or finished work | Hooks can be missed |
| Screen adapters | The visible terminal matches a configured working or idle marker | The terminal can be stale, ambiguous, or changed by a provider update |
| Rex program status record | The agent reports its own state to Rex, and Rex keeps the record of the block (Rex only) | A record counts only with the agent's pid, and only some programs write one |

A **screen adapter** interprets the terminal format used by a particular agent. It returns observations for the agent monitor to record; it does not edit the state database.

After a hook fires, Harold gives the terminal a short grace period to repaint. During that period, the hook wins. Afterward, conclusive screen evidence can correct stale state. Inside the grace period, the agent's own Rex record can refine the hook's report: a blocked record refines a working report, and an error record refines a done or idle report. A hook report does not clear that record. One reduction gives the state to every reader, the blocked timer included. An unreadable or inconclusive screen preserves earlier evidence; without any conclusive evidence, the state is `unknown`. CPU use and elapsed silence do not determine working or idle.

Recording the evidence before applying these rules makes the result reproducible. A failed capture does not erase known state, and replaying the same history applies the same decisions. The [reconciliation contract](../references/agent-monitor/README.md#reconciliation-contract) defines the exact timing and precedence rules.

### Identity before state

Suppose an agent exits and you launch another in the same tmux pane. The pane still exists, but the new agent must not inherit the old agent's working state or work summary.

Harold therefore identifies the pane together with the agent process, its start time, and its provider. This combination is called an **incarnation**. Process start time matters because the operating system can reuse a process ID.

A replacement starts at `Unknown` with no work summary. Recorded events belonging to an older incarnation remain in history but cannot change the new incarnation's current view. The [identity reference](../references/agent-monitor/README.md#incarnation-identity) lists the exact fields.

### Keeping the work summary useful

The state answers “Is it working?” The work summary answers “What is it working on?” Those values can change independently. An idle agent can keep the work summary of its last task, and Harold can learn a work summary even when screen state is inconclusive.

Hooks supply task text directly. Screen adapters can recover submitted instructions when hooks miss them. Harold keeps these sources separately and normally uses the most recent meaningful instruction. Idle prompts such as “Ask Codex to do anything” do not replace a work summary.

When activity summaries are on, a background Claude request turns the available instruction and completion reply into a short work summary. The agent monitor keeps working while it runs, and the source instruction remains available if generation fails. A result is accepted only while it still matches the same agent incarnation and activity, so a slow response cannot replace a newer task's work summary.

An activity summary condenses supplied evidence; it does not independently verify an agent's reported outcome. Generating one also does not send a phone notification. See [activity summaries](../references/agent-monitor/activity-summaries.md) for the evidence limits and [work-summary rules](../references/agent-monitor/README.md#work-summaries) for selection and clearing behavior.

### Reading recaps from transcripts

Harold reads an agent's recap from the agent's transcript. It reads the tail of the transcript file through the shared tail reader. The recap adapter of the provider, named by `recap_adapter`, finds the recap in that tail. Today only `claude-transcript-v1` exists, for Claude Code.

Harold checks the transcript every 10 seconds. It keeps the recaps in memory only. It sends them only on the `WatchAgentStates` stream, for the control-tower. A recap and a current step are not work summaries. See [recaps](../references/agent-monitor/README.md#metrics) for the counter and the log lines.

### Recovering a submitted task from scrollback

A submitted instruction may scroll out of view before Harold reads it. For supported terminal formats, Harold can look through a bounded history tail to recover it. State classification continues to use the visible screen.

Old history creates an identity problem: text already in a pane may belong to a previous agent. On its first successful history capture, Harold records a baseline of prompt fingerprints (compact identifiers for recognizing the same submitted blocks) without adopting any existing prompt as new work. Later captures must establish continuity with that baseline before newly submitted text becomes eligible. If continuity is lost, Harold establishes another baseline.

This deliberately sacrifices some recovery: if Harold attaches after a task has begun, it may never recover that task from scrollback. The benefit is that retained pane history cannot silently become a replacement agent's task.

Raw terminal captures stay inside capture and parsing. Only eligible, sanitized, bounded instructions can become stored work summary evidence; raw captures are never stored, logged, published to clients, or sent to the summarizer. Parsing support varies by provider. The [screen-adapter reference](../references/agent-monitor/screen-adapters.md) covers supported formats, unsent drafts, baselines, and recovery limits.

### Turn-end updates and phone replies

A turn-end update and an agent-state update serve different purposes. The control-tower needs an updated view even when you are already looking at the agent and no turn-end update is needed.

A `TurnCompleted` event creates delivery work in the outbox. The delivery handler checks the `[notify]` settings and whether you are at the desk, then either leaves the update out, or sends it through the away channel, iMessage. At the desk it leaves every update out. Retryable delivery failures remain pending and are tried again after 1 second, doubling to 60 seconds. Harold gives the delivery up at the 10th failed try. A phone reply that cannot be routed because the inventory fails follows another rule. Harold does not count these tries. It tries the reply again when the agent snapshot shows that the inventory works, or every 5 seconds. It gives the reply up after `LONGEST_INVENTORY_WAIT`, 15 minutes, with `reason = inventory_failed`. While the reply waits, later rows that are not phone replies go ahead, and later phone replies wait behind it, so replies keep their order. A restart of Harold starts the wait again ([ADR 0023](../adr/0023-a-phone-reply-that-waits-on-the-inventory-is-tried-again-when-the-inventory-works.md)). Each channel has its own list of agents in `[notify]`, and one resolver finds the chief's pane for every part of Harold.

For a phone reply, the channel listener records an `InboundMessageReceived` event. The delivery handler resolves a live agent pane using the message's routing tag or the configured routing fallbacks, checks that an agent is still present, and submits the message to that pane through its multiplexer. A reply typed in the web app may carry a photo: its `InboundMessageReceived` payload then holds `photo`, a file name in `app-photos/`, and the line typed into the chief's pane ends with `📷` and that file's absolute path, which Claude Code reads ([ADR 0010](../adr/0010-photos-reach-the-chief-as-files.md)).

The channels are Cargo features of the build. The `imessage` feature builds in the iMessage channel and the `calls` feature builds in calls. The `mac-defaults` set holds both, and the `linux-defaults` set holds neither yet ([ADR 0019](../adr/0019-harold-is-built-from-per-part-features-for-the-mac-and-linux.md)). A build without `imessage` never sets up that channel and reaches the phone through the web app only.

Ordinary agent observations and activity summaries do not create delivery work. The [notification reference](../references/notification/README.md) and [inbound routing reference](../references/inbound-message-routing/README.md) describe channel decisions, routing, and failure behavior.

### Startup, restart, and shutdown

Harold reads its settings from `default.toml`, then lays `local.toml` over it, and export to the local collector is on by default.

On a fresh installation, Harold creates its state database with the complete current schema. Before accepting client traffic, it catches the stored view up to the event stream. It then loads the complete saved snapshot and starts publishing. The control-tower cannot connect halfway through that recovery and receive a partially rebuilt view.

Persistent observations, projected state, and accepted activity summaries survive restart. Temporary work, such as pending activity summary requests and scrollback fingerprints, does not. History capture establishes a fresh baseline when the agent monitor resumes.

During shutdown, Harold signals its background jobs to stop, closes the control-tower's streams, and lets in-flight requests drain. The agent monitor has a bounded stop period. Already committed events and state remain available on the next start.

See the [startup and shutdown sequence](../references/agent-monitor/README.md#startup-reconnect-and-shutdown) for the precise recovery behavior. To connect agents to Harold, follow [Set up agent monitor hooks](../how-tos/setup-agent-monitor-hooks.md).
