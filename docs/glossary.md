# Glossary

This page lists the terms Harold's specs, code, logs, settings and docs use, each with one meaning and the words not to use for it. A change that names a new term adds it here, with its meaning and the words not to use, in the same change.

## Parts and people

| Term | Meaning | Not to use |
| --- | --- | --- |
| Harold | The daemon on each machine that watches agents, notifies the user, routes replies and hosts tools | "the server" |
| page id | The random signal `page_id` that one open thread page sends with each update stream, so Harold counts pages and not streams. It is not a secret and not a credential. | "page token", "session id" (an agent's session), "client id" |
| feature | A Cargo feature of Harold that builds one platform part in, such as `imessage`. It is chosen when Harold is built and is not a setting. For a part of a product that people use, see product feature. | "flag" alone, "toggle". "Feature flag" stays allowed: it is your own word |
| feature set | A named group of features: `mac-defaults` (the default) or `linux-defaults` | "profile", "preset" |
| timing test | A test that asserts a real time, and so can fail on a busy machine. It runs only with `--features timing-tests`, locally | "flaky test" |
| test log | The kept log of one test run, with its header and footer, in the test log folder | "test output" for the kept file |
| test log folder | The base checkout's `.superpowers/pod/test-logs/`, one subfolder for each spec id | "artifacts folder" |
| container test set | A named list of tests that `scripts/test-run --container` runs in a podman container, because they open a port or start a second Harold | "integration tests", "sandbox" |
| `imessage` | The feature that builds in the iMessage channel. macOS only | "iMessage flag" |
| `calls` | The feature that builds in calls, the call answerer, the call models and the echo test page. macOS only | "voice feature" |
| Mac package | The release file `harold-aarch64-apple-darwin.tar.gz`, built with `mac-defaults` | "bundle" for the file |
| Linux package | The release file `harold-x86_64-unknown-linux-gnu.tar.gz`, built with `linux-defaults` | "Omarchy build", "bundle" for the file |
| service manager | What starts Harold and starts it again after a crash: launchd on the Mac, the systemd user manager on Linux | "launchd" for both |
| haroldctl | The command that starts, stops and checks the installed Harold service, through the service manager | — |
| control-tower | The terminal program beside the chief that lists the agents, holds the chief's questions and lets you act on them. `harold start-control-tower` starts it | "the controller", "the dash", "dashboard", `tmx-agent-dash`, "TUI" |
| agent | A coding agent running in a pane: Claude Code, Codex or OpenCode | "session" for the agent, "process" for the agent |
| echo test | The hidden web app page that measures how much of Harold's voice the phone's mic picks up while the phone plays it | "loopback test", "AEC test" |
| paired device | A phone or browser that typed the pairing code and holds a device secret, listed by `harold app devices` | "trusted device", "registered" |
| pairing code | The code you choose and keep as `HAROLD_PAIRING_CODE` in the pairing file; you type it into the pair screen. It is fixed and does not expire | "PIN", "password", "one-time code" |
| pairing file | `~/bin/harold/config/pairing.env`, mode 0600, outside the repository, which holds the pairing code | "secret file", ".env" alone |
| device secret | The random secret a paired device keeps as the cookie `harold_device`; Harold keeps only its fingerprint | "token", "API key" |
| pairing lock | What stops pairing for good after 5 wrong tries in all, until the code changes or `harold app pair --unlock` | "ban", "rate limit" |
| provider | Which kind an agent is: `claude`, `codex` or `opencode` | — |
| multiplexer | The program that holds the panes an agent runs in: tmux or Rex. Harold works with one, the one `agent_monitor.multiplexer` names | "back end", "terminal" for it |
| Rex | The Superlogical terminal multiplexer: a macOS app with a server. A Rex pane is a block, with an id such as `block:01a1…` | "Rex terminal" for the server |
| `agent_monitor.multiplexer` | The setting that picks the multiplexer Harold works with, `"tmux"` (the default) or `"rex"`. Harold reads it at start | — |
| pane, pane id | The tmux pane or the Rex block an agent runs in, and its id. The id has three forms: `%47` (tmux, this machine), `block:01a1…` (Rex, this machine) and `rex:omarchy/block:01a1…` (Rex, a Rex host; see host pane id). The first two are the local forms | "pane address", "target" |
| Rex host | Another machine's Rex server that this Mac's Rex knows by a label, such as `omarchy`. Harold reads the agents of the hosts in `[agent_monitor] rex_hosts`, read-only. `rex hosts list` lists the hosts, and `rex -S <label>` reaches one | "node" |
| agent on a Rex host | An agent in a Rex host's pane. Harold shows it in the control-tower and does nothing else with it. In the code: `RexHostAgent` | "remote agent" |
| host pane id | The pane id of an agent on a Rex host, `rex:<label>/block:<id>`. No writer on the master types into it. The master will send a type request to the slave that holds the pane (planned in 2c.2) | "remote id", "qualified id", "pane address" |
| master | The Harold that your phone, the chief and the control-tower talk to: the Mac's | "hub", "primary" |
| slave | A Harold on another machine that reports its agents to the master: omarchy's. In 2c.2 it will also do the master's requests | "satellite", "replica", "member" |
| slave link | The one gRPC stream a slave opens to the master and keeps open, carrying reports up (and, in 2c.2, requests down) | "link" alone (a todo's link field), "tunnel", "peer connection" |
| report (on a slave link) | One message a slave sends up: a snapshot, a turn end or a lock state | "event" (an event is in Harold's event store) |
| report backlog | The reports a slave has not yet had acknowledged by the master, kept in `report-backlog.db` for up to 24 hours or 1,000 reports | "outbox" (the state database's list of delivery work), "queue" |
| slave pairing secret | The one-time 32-byte secret that `harold slaves pair` prints on the master and `harold master pair` keeps on the slave. It is the only check of a slave link | "pairing code" (that is the phone's fixed code), "token", "password" |
| type request (planned in 2c.2) | A master's request on a slave link to type one line into a slave's pane. Not built yet | "command" |
| host icon | The mark name that `[agent_monitor.rex_host_marks]` gives a host, `arch`, `linux` or `mac`, keyed by a label in `rex_hosts` or by Harold's own `machine_name`. A host with no entry has none | "host logo", "host image" |
| host marker | The glyph, emoji or label and dot that the control-tower's WHERE column shows before the session of an agent: the mark of its host icon, or, in text mode, the label of a Rex host | "host badge", "icon prefix", "host tag" |
| exited block | A Rex block whose program has exited. Rex keeps it, with its `last_exit`, and it holds no agent | "dead pane", "zombie block" |
| unreadable block | A Rex block whose `process` answer failed, timed out or could not be read in one inventory scan | "bad block", "broken pane" |
| window name | The name of the window that holds an agent's pane: the window's own `label` in Rex's `session.view` (not a pane's label), or tmux's `#{window_name}`. The control-tower shows it as the row's text when the row has no role. It is not the **pane label** (`<session>:<window>.<pane>`, the `tmux_target` field) | "tab name", "worker name" for the field (a worker's window is named after the worker, but the field is the window's) |
| pane label | The text the agent list shows and a routing tag matches: `<session>:<window>.<pane>` | "target" |
| INSERT mode | Claude Code's Vim editor mode in which keys type text into the input box | — |
| NORMAL mode | Claude Code's Vim editor mode in which keys are commands. Harold presses `i` to return to INSERT mode before it types | — |
| pane resolve | The look-up of one pane by its id (`resolve_pane` in the code), which runs its own scan after it waits for the monitor's current tick. Harold logs a slow one as `agent pane resolve slow` | "pane lookup" |
| incarnation | One run of an agent in a pane, told apart by pane, process, start time and provider. A new one starts at `unknown` | — |
| the chief, chief of pods | The first agent pane in the chief's session among the agents in the chief's folder, which talks to the user and asks them questions. `role` = `chief` on `pod.agents` counts every pane in the chief's folder | "coordinator" for the chief (the Coordinator row is the pod's agent), "main agent" |
| the chief's row | The chief's row in the control-tower's agent list | — |
| pod | One product's sessions: its PnEM, its pod coordination and its build workers | — |
| pod session | A pod-coordination or PnEM pane | — |
| build worker | An agent building one spec in its own worktree | "slot" for the worker |
| slot | A place for one build worker in a pod, busy or free | — |
| pod registry | The vault's `products.toml`, named by `[telemetry] pod_registry` | — |
| open spec | A spec that has `specs/<id>.md` and no later state: the user has not approved it. `state` = `open` | "drafted", "waiting" for the state |
| in-progress spec | A spec with a started file, or with a busy worktree, and no later state. `state` = `in_progress` | "building" for the state |
| completed spec | A spec with a merged file, or one that the older sources show merged, and no withdrawn state. `state` = `completed` | "merged" for the state |
| withdrawn spec | A spec with a withdrawn file or a line in `rejections.md`. `state` = `withdrawn` | "rejected" for the state |
| withdrawn file | The file `withdrawn/<id>.md` in a pod folder, which the PnEM writes when it withdraws or replaces a spec | — |
| started file | The file `started/<id>.md` in a pod folder, which baton's `spawn.sh` writes at the first spawn for a spec | — |
| merged file | The file `merged/<id>.md` in a pod folder, which pod coordination writes when it sends `MERGED` for a spec's last stage | — |
| pod notices | The pod-file faults Harold has warned about once, such as a spec completed without its merged file, kept in `pod-notices.json` so a restart does not warn again | "alerts" (an alert is HyperDX's), "warnings file" |
| approval file | The file `approvals/<id>.md` in a pod folder, which the chief writes when you approve a spec, or when it starts one under a waiver. It sits beside the withdrawn, started and merged files | "approved file" |
| next release (of a product) | What `harold tool release next` lists for a product: merged since the release, in progress, approved, not started, parked, awaiting approval and not clear | "changelog" (it lists specs, not changes) |
| merged since the release | The completed specs whose merge commit is not in the product's newest `v*` tag. A completed spec that is in the tag is not listed. Here "merged" names the merge commit, as in merged file, not the state | "unreleased" |
| in progress (a list) | The in-progress specs that have a branch and no parked handoff file. It differs from in-progress spec, which also holds parked and not clear specs | "building" |
| approved, not started | The approved specs with no started file and no parked handoff file, approved on or after the product's first merged file. It differs from the approved state, which also holds old approvals, and from queue, the chief's own list | "queued" |
| parked (a spec) | A spec whose build worker pod coordination stopped, with `parked/<id>-handoff.md` for the next worker | "paused" (a paused product is a registry setting) |
| awaiting approval | The open specs whose first line is `Waiting for your approval.` It differs from open spec, which also holds specs under the small-fix waiver, and from waiting, which is never used alone | "waiting for you" |
| not clear (a spec) | An in-progress spec with no branch and no parked handoff file | "stale", "lost" |
| direction revision | A revision of a spec that follows a change of what the user wants. Its heading in `specs/<id>.revisions.md` carries the tag `direction` | "rework" |
| free slot with work waiting | A free slot while an approved spec of its pod is not started. A free slot alone (`pod.slots.free`, the `slot-idle` alert, "idle slots" in the telemetry README) counts every free slot. This counts only the free slots that work waits for, which is waste. `pod.slots.idle_with_work` stays as the signal name | "idle slot with work", "idle slot" for this one |
| redo | Build work after a `direction` or `defect` revision of a spec that had started | "rework", "rejected" |
| value created | The product features whose use count rose in the 7 days, plus the fixes confirmed in them | "work checked" for it (the old meaning), "output" |
| product feature | A part of a product that a person or a part of the system uses, which a spec of kind `feature` adds. Not the Cargo feature | "feature flag" for it |
| kind (of a spec) | `feature` (a product feature), `fix` or `other`, from the spec's `**Kind.**` line | "type". Not the Cargo feature |
| feature-use count | The counter `<product>.feature.used` with `spec.id`, which rises at each use of a product feature | "usage metric". Not the Cargo feature |
| evidence query | A named query file in `queries/evidence/` that returns `present` and `reason` for one Done-when item of a spec, after `{since}` | "proof query", "evidence SQL", "check query" |
| evidence check | Harold's run of a spec's evidence queries, which writes the evidence file and, for a fix, the confirmed file | "verification", "evidence run", "the audit" |
| evidence file | The file `evidence/<id>.md` in a pod folder, with the state `waiting`, `present` or `missing`, and the `told:` line when the chief has the `EVIDENCE` line for it | "evidence report", "proof file" |
| evidence missing | The list in `harold tool release next`, and the state of a spec whose evidence query still returns 0 seven days after its release | "unproven", "no evidence" (alone) |
| confirmed file | The file `confirmed/<id>.md` in a pod folder, which the evidence check writes when the evidence query of a fix finds its signal. It sits beside the approval, merged and withdrawn files | |
| spec size | Small (under half a day of build), medium (up to a day) or large (split before approval), in the spec's `**Size.**` line | "estimate", "points" |
| item-days | The sum of the ages, in days, of the specs waiting in one list | "WIP" |
| wording (revision tag) | A revision that changes words only: it counts as a revision and is never redo | "typo fix" for the tag |
| PnEM | A pod's product and engineering manager: the session that keeps the pod's backlog and writes its specs. `role` = `pnem` on `pod.agents` names it | "PM", "product manager", "planner" |
| pod coordination | A pod's session that starts build workers and merges their work. The control-tower names its row Coordinator. `role` = `pod_coordination` on `pod.agents` names it | "baton" for the session, "orchestrator" |
| browser span | A span the web app records in the phone's browser, such as a page load, a request or a script error, which Harold forwards to the collector | "RUM", "client span", "frontend telemetry" |
| collector | The OpenTelemetry collector at `http://localhost:4317` and `http://localhost:4318`, part of the local stack. It takes Harold's traces, logs and metrics over OTLP gRPC on 4317, and browser spans, which Harold forwards, as OTLP JSON on 4318, and stores them in ClickHouse | "ClickHouse" for where Harold sends, "exporter", "agent" for the collector |
| local stack | The services in `~/code/sre/stack` on a Mac, which are the collector, ClickHouse and HyperDX | "SRE stack", "observability stack", "HyperDX" for the whole |
| export, export on, export off | Harold sending to the collector, set by `[telemetry] export`, `otlp_endpoint` and `otlp_http_endpoint`. Export is on by default; `export = false` sets it off, and then Harold also leaves out the page's tracing script and drops any browser span posted to it. When export is on, `harold.log` holds only telemetry's own failures and the unsent log lines | "telemetry on/off" for export, "disabled", "enabled", "no address", "push" |
| unsent log line | A log line that the export could not send to the collector. Harold writes it to `harold.log` with `unsent=true`, and keeps it to send again | "fallback line" (the glossary already has **fallback sentence**), "dropped line" |
| resend queue | The unsent log lines that Harold keeps in memory, at most 5,000, and sends again when the export works again | "retry buffer", "backlog" (the backlog is the PnEM's list) |
| parity check | `scripts/check_log_parity.sh`, which sorts each line of `harold.log` by why it is in the file, and looks in ClickHouse for each unsent log line | "log diff" |
| multiplexer call | One request that Harold makes to its multiplexer, today one run of the `rex` or `tmux` program. Harold counts each one in `harold.multiplexer.calls` and times it in `harold.multiplexer.call.duration` | "rex call" or "tmux call" for the general idea |
| host | The machine whose multiplexer holds a pane. It can be another machine than the one Harold runs on. The metric attribute `host` is the empty string for Harold's own machine. Machine stats are always about Harold's own machine, not about a host. For an agent on a Rex host, it is the label of that Rex host | "server" for the machine, "node" |
| machine stats | The disk, memory, swap and CPU use that Harold reads on its own machine and sends to HyperDX | "system metrics" alone, "host metrics" |
| normal range | For one machine, metric and local hour of the day, the use from 0 to the top that Harold computes from the same hour on the last 7 days | "baseline" (that is the pane's prompt fingerprints), "threshold" (that is HyperDX's fixed limit) |
| hourly use | The mean of one machine stat for one local hour, kept for 14 days in the hourly use file, `data/machine-hourly-use.json` | "baseline", "history" (that is a pane's history capture, or a history note) |
| user | The person Harold works for | — |
| phone | The user's iPhone, reached by iMessage or the web app | "away device", except in the skill name `send-message-to-away-device` |

## Where the user is

| Term | Meaning | Not to use |
| --- | --- | --- |
| at the desk | The Mac's screen is unlocked, so Harold speaks on the Mac | — |
| away | The Mac's screen is locked, so Harold writes to the phone | — |
| away channel | iMessage, the channel that texts the phone and reads its replies. Code and the skill name call it `away` (`notify_away`, `AwayText`, `send-message-to-away-device`) | — |
| web app | The home-screen app on the phone that holds the chief's thread | "away app", "home-screen app" |
| thread | The web app's one conversation with the chief | — |
| route class | The fixed name of a group of the web app's paths, such as `thread`, `updates` or `pages`, used in its signals as `route_class` in place of the path | "endpoint", "path", "route" alone |
| web request refusal | A request that the login, device or origin check answers with a refusal, with its reason | "error" for it |
| `web_app.app_name` | The setting for the name under the web app's icon on the home screen and in its title; "Chief" when left out | "app title", "display name" |
| `web_app.icon` | The setting for the PNG picture, the icon, that marks the web app on the home screen | "favicon", "app icon" |
| phone notification | Any message Harold sends to the phone, tagged with its machine and pane: a turn-end update, an agent message or a spoken line, sent by iMessage or the web app | "notification" alone, "phone message" |
| latest notifier | The Mac that sent the latest phone notification. A reply with no routing tag goes to it | — |
| push | How the web app shows a phone notification on the phone's lock screen, through the phone's push service | "alert" for a push |
| machine name | This Mac's name, as tagged on its messages | — |
| routing tag | The `[machine/pane]` text at the start of a reply that picks its pane | — |
| watch | A schedule an agent asks Harold for: run one of its skills with a model every few minutes or on a cron expression, for a set time or with no end, and report to the chief or the agent itself. It is not the agent monitor's watching of agents ("Watching agents"), and not the `WatchAgentStates` stream. See also agent job, which is its own term and is not a watch | "scheduled job", "monitoring job", "job" (a background job is Harold's own thread), "watcher", "monitor" |
| watch skill | The Claude Code skill a watch runs, named under the watch's folder's `.claude/skills/` (its files there or in a watch skill folder), holding the checks, what is worth reporting, and in `allowed-tools` what the model may use, and in `add-dirs` the folders outside the watch's folder its run may reach | "watch prompt", "watch config" |
| watch skill folder | A folder the user lists in `[watch] skill_folders`, where a watch skill linked into `.claude/skills/` may keep its files, such as the chief-of-pods repository's standard watch skills or an organisation repository's own | "skill root", "skill path", "skills directory" |
| watch run | One run of a watch's model, with its facts and its answer | "tick", "job run" |
| run limit | The most time one watch run may take before Harold stops it, set with `--run-limit` and logged as `run_ms` against it, with `wait_ms` as the wait for the model before it | "timeout" for the setting, "run timeout", "run time limit", "run cap", "run budget" (the run budget is the most a run may spend) |
| watch report | A `WATCH <name> …` line Harold types for a watch: a run's `REPORT` line, a `quiet` line under `--every-run`, or a closing line. What is worth reporting is the watch skill's to decide | "alert" (an alert is HyperDX's), "problem" for every report |
| watch tool | The tool that holds the watches, runs them and types their reports | — |
| open-ended watch | A watch started with no `--for`: it runs until `watch stop`, and needs a daily budget | "forever watch", "permanent watch" |
| paused watch | A watch that runs nothing until `watch resume` or its pause ends. It is not the **messaging pause** | "suspended" |
| capped watch | An open-ended watch that the spent daily budget paused until the next local day. `list` shows it as `paused (budget)`. `resume` and `W` do not resume it | "over budget" for the state |
| watch reminder | A `REMIND <name>: <text>` line that the watch tool types once at a set time, with no model | "alarm", "timer" |
| agent job | A long command that an agent started outside its pane and recorded with `harold tool job add`. Harold checks it and tells the agent when it ends. The bare word "job" is allowed only in the job tool and its names (`harold tool job`, job line, `harold.job.*`) | "background job" (Harold's own threads), "workflow", "task" |
| job line | The line under an agent row that shows one of its agent jobs | "job row" (a row is an agent row) |
| end line (of an agent job) | The line that Harold types into the agent's pane when its job ends: `JOB <name> ended after <n> m (pid <pid>). Check its output.` For a unit: `JOB <name> on <host> ended after <n> m: exit 0.` (`failed (exit 2)` or `exit unknown` when it differs). The master types it for a slave's job | "job notice", "done message" |
| unit (of an agent job) | A systemd unit on a Linux machine that runs an agent job, named with `--unit`. | "service" alone, "job" for the unit itself |
| unknown (an agent job) | The state of a slave's agent job while the master's slave link to that slave is closed. It is not the agent state `unknown`. | "lost", "stale" |
| keep-awake file | The file `harold-job-<name>` that Harold writes for a running agent job in the agent-activity folder (`[job] keep_awake_folder`), so omarchy's idle-suspend does not suspend the machine. | "heartbeat file", "lock" |
| host (of an agent job) | The slave's name, which is also its Rex host label, for example `omarchy`. Empty for a job on the master's own machine. | "machine name" for it (that is this Mac's name) |
| facts | What Harold hands the model on each watch run: agents, pods, the chief's pane and the run numbers; and, for a call, the call facts | "context", "evidence" |

## Watching agents

| Term | Meaning | Not to use |
| --- | --- | --- |
| agent monitor | The part of Harold that finds agents and works out their state | "monitor" alone |
| inventory, process inventory | The agent monitor's regular scan of the panes of Harold's multiplexer and the agent processes in them | — |
| tty read | The inventory's read of only the processes on the tmux panes' ttys (`ps -t`). Only with tmux. With Rex, the agent comes from Rex's `process` reply | "partial scan", "fast scan" |
| full read | The inventory's read of every process on the Mac (`ps -ax`), done only when a chain is broken | "full scan", because "scan" is the inventory's whole pass |
| inventory scan | One pass of the inventory over the panes of Harold's multiplexer, or of one Rex host | "full scan", "fast scan", "partial scan" |
| screen read | One read of a pane's screen text through the multiplexer | "capture" alone, "screen grab" |
| prompt recovery | The agent monitor's read of a pane's history to find a submitted instruction that no hook reported | "prompt scan", "history scan" |
| purpose (of a screen read) | Why Harold reads a screen: `state`, `prompt` or `writer`, an attribute of the multiplexer metrics | "reason", which names a failure elsewhere |
| screen adapter | The per-provider reader of an agent's visible screen, such as `claude-v1` | — |
| input box | The row where Claude Code shows the text the user types, between two rows of `─`. Harold types a line into it only when it is empty | "prompt box", "text box" |
| suggested prompt | The dim text Claude Code draws in an empty input box after a turn. It is not typed text, and the input box still counts as empty | "grey suggestion", "autocomplete" |
| hook | A small script a provider runs to tell Harold what an agent did | — |
| Claude Code mod | The Claude Code plugin in `hooks/claude-mod` that reports a working pane | "plugin" for it. OpenCode's is an OpenCode plugin |
| agent state | What an agent does now: `working`, `blocked`, `done`, `idle`, `error`, or `unknown` when Harold has no conclusive evidence. The agent's own report, the hooks, the program status record or the screen supply it. A pane with no agent program has no state and is not listed | "Busy", "Idle" (the old states), "status" |
| working | The agent runs a turn | "busy", "running" |
| blocked | The agent waits for you inside its turn: a permission prompt, a question or a login | "waiting", "stuck" |
| done | The agent finished a turn | "finished", "complete" |
| idle | The agent waits at its prompt | "free", "ready" |
| error | The agent stopped with a failure | "failed", "crashed" |
| unknown | An agent runs in the pane, and no evidence gives its state yet, or Harold has not read its Rex host for longer than `host_unknown_after_ms` | "Unknown" with a capital, "unknown" for a pane with no agent |
| turn | One request to an agent and its work until it stops | — |
| turn tokens | The input, output, cache-read and cache-write tokens of one Claude Code turn, from its transcript | "cost" (tokens are not dollars), "usage" alone |
| task | The work the user asks an agent to do. Its short text is the agent's work summary | "task" for a thread of Harold's own, say "background job" |
| work summary | Short text that names what an agent is working on. A recap and a current step are not work summaries | "task description", "current work", "dashboard description", "summary" alone |
| activity summary | A work summary that Claude Code writes from the agent's evidence, when `[activity_summary]` is on | "generated description" |
| program status record | What a terminal keeps for a pane from the program's `OSC 7501` reports: the state, `app`, `msg` and the `owner` pid. Harold reads the root record only | "tab status" |
| recap | An agent's own short text of what its session is doing and what comes next, which the provider writes into the session's transcript. Today only Claude Code writes one, after you were away. The control-tower shows the latest one under the label `WORK SUMMARY`, with its age, in place of the work summary | "away summary", "summary" alone |
| recap adapter | The per-provider reader of recaps from a transcript, such as `claude-transcript-v1`, named by `recap_adapter` in the provider's `[[agents]]` entry | "recap parser" |
| current step | The one line an agent reports as its step now, from the `msg` of its program status record, while the state of the record is working or blocked. It is not a work summary | "status", "activity" |
| waiting agent | An agent that stopped working while you were not looking at it: its state went from working or blocked to done, idle or error. Harold decides it and serves it to every control-tower. The proto fields `waiting` and `waiting_since_ms` and the metrics `harold.agents.waiting` and `harold.agent.waiting.duration` are code names for it. The control-tower marks it `◆` until it is seen | — |
| seen (a waiting agent) | Harold knows you looked at the agent's pane: it is the focused pane, or a control-tower jumped to it | "visited", "acknowledged" |
| focused pane | The pane you look at now: the focused block of the active window of the one session the Rex app shows, or the active tmux pane | "active agent", "selected row" (the cursor is another thing), "work record focus" (`focus` is a work record's priority) |
| focus tracking | Harold's check, once a second, of the focused pane. `focus_known` tells the control-tower whether it knows it | — |

## Talking to the user

| Term | Meaning | Not to use |
| --- | --- | --- |
| turn-end update | What Harold says or sends when an agent's turn ends, as delivery work that the `TurnCompleted` event creates | "completion notification", "notification" alone |
| trailing question | The last sentence of a turn-end update, when the update ends with it and it ends in `?`. iMessage sends it as a second message | "question" alone, which is a todo kind |
| fallback sentence | The spoken turn-end update `The chief has finished and is waiting.`, spoken in a call when Haiku gives no sentence that names the chief, fails or passes 8 s, or the call answerer is off | "default message", "fixed tail" |
| agent message | Text an agent asks Harold to send to the phone (`send-message-to-away-device`) | — |
| spoken line | A line the chief asks Harold to say (`harold speak`) | — |
| voice note | A spoken line sent to the phone as audio. Harold no longer sends one (no-voice-notes, 2026-10-25) | — |
| held back | An update or message a channel's rules keep from going out. A turn-end update held back is "left out", and an agent message or spoken line held back is "dropped". `outcome = held_back` covers both | "skipped" (except in the existing `notification skipped` log line, kept as written), "suppressed" |
| left out | A turn-end update Harold chose not to say or send. It names the channel list that held the pane back | — |
| dropped | An agent message or spoken line Harold chose not to send. It names the channel list that held the pane back | — |
| channel | One of the three ways Harold reaches the user, each with its own list in `[notify]`: `imessage`, `web_app` and `machine_audio_output`. In the event `chief.attention_request`, the value `chief_pane` also names a message the user reads in the chief's pane at the desk | "away channel" for it (the away channel is only iMessage), "route", "output" alone |
| attention request | A message from the chief to the user that needs a decision or an answer, counted once per message, as the event `chief.attention_request` | "question" for it (a question is a todo with an id), "ask" (an ask is a decision's own question text), "prompt" |
| attention cost | The time the user took to answer attention requests, in hours, times the hourly rate of `[attention]`, in its currency (AUD by default) | "question cost", "dollars" for it when the currency is not dollars |
| channel list | A channel's list in `[notify]` of the folders whose agents may use that channel | "allowlist", "whitelist", "only_from" |
| the chief's session | The session the chief runs in, in Harold's multiplexer: the one `notify.chief_session` names, or else the session of the longest-running agent in the chief's folder | "chief window", "the chief's tmux session" for a Rex one |
| `notify.chief_session` | The `[notify] chief_session` entry that names the chief's session exactly, a tmux session name or a Rex session label. It picks the session among the agents in the chief's folder, not which folder is the chief's | "chief session setting", "session prefix" |
| the chief's folder | The folder `[notify] chief` names. The channel lists count every pane in it as the chief's, while only the chief's pane gets the chief's typing | "chief directory" |
| `notify.chief` | The `[notify] chief` entry that names the chief's pane by its folder. It decides which pane is the chief, not what the chief may use | "chief folder setting", "away_messages_only_from" for it |
| voice, voice mute | Harold's speech at the desk, and the switch that silences it (`Voice: mute`) | — |
| messaging pause | The switch that stops all phone traffic in both directions (`Messaging: pause`) | "kill switch" as its name |
| Harold state | What Harold holds now, as `harold get` reads it. It holds the voice mute and the messaging pause. It holds the release version and the machine name. It holds the multiplexer, the Rex hosts and their host icons. It holds the TTS engine and the chief's session. A value can change while Harold runs. In `harold get`, a **thing** is one named item of it, such as `voice`. It never holds a token, key, pairing code, phone number, command line or path | "config dump", "settings export" |
| call | A voice conversation through Harold, answered by the call answerer, which hands real work to the chief. At the Mac or in the web app | "call" for an RPC or a function |
| desk call | A call at the Mac, run by Harold's voice input (`voice_input::daemon_calls()`, the `Call` RPC), with the Mac's microphone and speaker | "Mac call", "local call" |
| phone call | A call in the web app on your phone (`AppCalls`, `harold/src/web_app/call.rs`), with the phone's microphone and speaker. The log lines `app call started` and `app call ended` (`voice_input/phone.rs`) keep their names, because HyperDX searches find them by those names: this is the one place where "app call" stays | "web call", "app call" |
| call answerer | The small model, one `claude -p` run on Haiku for each turn, that answers your turns in a call from the call facts and hands real work to the chief. You asked for it as the fast call voice | "call voice" (voice is Harold's speech), "assistant", "bot", "fast model" |
| call facts | The facts for a call: what Harold gives the call answerer on each turn, as it gives a watch run its facts. The agents, the pods, and this call's hand-overs and chief lines, and with Choice C(b) your todos and the chief's queue | "brief", "context" |
| hand-over | A request the call answerer passes to the chief, typed into its pane as `🎙 hand-over H<n>: <request>` | "handoff" (the PnEM's context note), "delegation", "escalation" |
| talk now | The key, `t`, that cuts the chief's answer short in a call | — |
| voice worker | The process that turns speech into text in a call. It loads Whisper only, never downloads it, and waits until Harold has it. The TTS worker makes the audio | "TTS worker" (that makes the audio) |
| speech model | Whisper, which turns speech into text. The log line `speech model downloaded` and the phone use the same words. Kokoro is a TTS model | "voice model", "weights", "Whisper" for a model that turns text into speech |
| TTS server | The part of Harold, on each machine, that turns text into audio for every caller, through `harold.tts.v1.TtsService`. The desk voice, calls and `harold tool tts` all use it | "voice server", "speech server" (the speech models are Whisper and Kokoro together) |
| TTS worker | The process that keeps one TTS model loaded for the TTS server: the MLX worker on macOS, the ONNX worker on Linux, which runs on the NVIDIA GPU and falls back to the CPU | "voice worker" (that loads Whisper for calls), "runner" |
| TTS model | A text-to-speech model that the TTS server lists, such as Kokoro. Harold logs its download as `tts model downloaded` | "voice" for the model (a voice is one speaker inside a model, such as `af_heart`) |
| approved model list | `tts-models.toml`, beside `local.toml`: the TTS models that Harold may download, each written after an audit. A model that is not on it waits for approval | "allowlist" alone (ADR 0011's list in `models.rs` is the speech models') |
| connecting | The call state from tapping Call until Harold picks up, shown as "Connecting…". It lasts at least one ring cycle, 3 s, and more cycles until the voice worker is ready | "dialling", "loading" for the state |
| ringback | The phone-call ring, "ring-ring", Harold plays from the tap until it picks up, one whole 3 s cycle after another | "ringtone", "connecting tone", "tick" (the tick is the chief-thinking sound) |
| pick-up | The short rising two-note sound that ends the ringback; the call is listening when it ends | "answer", "beep" |
| metered | Harold's guess that the Mac is on the iPhone's Personal Hotspot, from the default gateway, which holds back a speech model download | "cellular", "expensive", "offline" |
| timed prompt | Text Harold types into a pane at a set time each day, or at the minutes of a cron expression | — |
| restart reminder | The one line Harold types into the chief's pane when its session is old or has compacted often, asking it to hand off and restart | "reminder" alone, for this or for a watch reminder |
| cron expression | Five fields, minute hour day month weekday, that name the local minutes a schedule runs | "crontab" (there is no file), "cron job", "day-of-week" for the fifth field |
| alert | A HyperDX alert that Harold types to the chief as `harold alert: …` | "alert" for a push |
| dictation | Turning your speech into text in the web app's reply box, on the phone, for you to check and send | "transcription", which is Whisper on the Mac in a call; "voice message", since nothing recorded is sent |
| photo | An image you attach to a web app reply, kept by Harold in `app-photos/` and named in the line typed to the chief | "attachment" alone, "image upload", "media" |
| thread page | The web app's page that shows the thread and the reply box. Its header links Todos and Pages | "message page", "chat page" |
| todo page | The web app's page that lists your todos and lets you answer them from the phone | "task page", "queue page" |
| back swipe | A swipe right from the left edge of a page outside the thread, which goes back one level | "swipe" alone (the thread and the todos have their own) |
| page dots | The two dots above the reply box that show which page is open | "pager", "swipe indicator" |
| read mark | The newest thread message a device has shown fully on screen, kept in that device's browser | "last seen", "read receipt", "unread marker" |
| todo count | The orange number on the thread page's **Todos** button: the waiting questions and the instructions | "badge", "unread count" |
| checked line | The line at the top of the web app's todo page saying when the chief last checked the todos. It turns red when the check is over 2 hours old and a todo waits | "freshness", "sync line" |
| changed spec | A todo whose spec file no longer matches the checksum it was asked with, which is the question tool's spec state `changed` | "stale", "outdated", "out of date" |
| sent line | The line at the top of the web app's todo page that says which answer was sent to the chief, such as `Q53: revise sent to the chief.` It stays until the next answer from that page or until the page is left | "toast", "banner" |
| new-message count | The number beside the speech-bubble icon in the todo page's header: the chief's messages that arrived while the todo page was open | "unread count", "notification count" |
| sender label | The small word at the bottom right of each message, `chief` or `me` | "byline", "name tag" |
| stored mark | The one ✓ before `me`: Harold has stored the reply but not typed it into the chief's pane | "tick" (a tick is a ticked step), "read receipt", "delivered" |
| typed mark | The ✓✓ before `me`: Harold has typed the reply into the chief's pane. The chief may not have read it yet | "tick", "read receipt", "read" |
| responding dots | The three grey dots on the chief's side while the chief works after your reply was typed | "typing indicator" (the chief does not type) |
| New message button | The `New message ↓` button that appears when a message arrives while you read older messages | "toast", "banner" |

## The control-tower

| Term | Meaning | Not to use |
| --- | --- | --- |
| masthead | The control-tower's top line: version, transport, REV, voice, the messaging pause and the watches | "header" for it |
| summary strip | The line under the masthead with the count of agents in each state, `unknown` only when above 0, and the server's clock. It always shows WORKING and IDLE. It shows BLOCKED, DONE and ERROR only when above 0 | "summary" alone |
| transport | The control-tower's connection to Harold: `LIVE`, `STALE` and the like | — |
| filter | The control-tower's search, which hides agent rows that don't match | — |
| agent row | A row of the agent list for one agent | — |
| cursor | The row the control-tower's keys act on, marked `▶` | "focus", "selection" |
| detail panel | The panel beside the agent list, on a wide terminal, about the agent under the cursor | — |
| held keys | Keys the control-tower ignores after a question changed under a note, until Esc | "held" alone |
| remote control | The small numbered list that one key opens over your Rex window: the chief as 0 and each running PnEM after it, in your order. A digit jumps there. `harold start-control-tower --remote` starts it, and its title is `Harold: remote control`. Here "remote" means a control at a distance. It never means an agent on a Rex host | "switcher", "picker" |
| remote order | The order of the PnEMs in the remote control, which you set with `J` and `K` and Harold keeps. The flag `--remote`, the calls `GetRemoteOrder` and `SetRemoteOrder`, and the file `remote-order.json` use "remote" in the same sense, never for a Rex host | "harpoon list" |

## Questions

| Term | Meaning | Not to use |
| --- | --- | --- |
| todo | Something the chief puts in the control-tower for you, either a question to answer or instructions to follow, with an id such as `Q1` or `I1` | "task", "item" (an item is a backlog entry), "request", "question" for both kinds |
| question | Something the chief asks the user, with an id such as `Q1` and one or more decisions. A question is one kind of todo. | — |
| instructions | One set of numbered steps the chief asks you to do by hand, a todo with an id such as `I1`. It is not a question: the question tool holds both in one table, and the API carries instructions in a `Question` message with `steps` in place of `decisions`. | "step list", "checklist", "question" for it |
| step | One numbered line of instructions, with its text and optional detail | "task", "item" |
| tick, ticked, untick | A step marked `[✓]` with `Space`, and the reverse. The ticked count, `2 of 4 ticked`, is the steps ticked out of all steps. | "check", or "done" for a single step |
| done | The action row of instructions that tells the chief they are done, and the answers line's `done` | "send" for it, "done" for a single step |
| ignore | The action row of instructions that tells the chief you will not do them, and takes them off your todos | "relegate to idea", "reject" for it |
| waiting question | A question not yet answered, relegated or dismissed. Instructions are never one. | "waiting" alone |
| todo rows | Every row the todos take under the chief's row: the closed row while closed, and while open the open todo's rows plus the other todos' todo rows | "question rows", "question block" |
| closed row | The one row `▸ n todos waiting` that stands for every todo while closed | — |
| todo header | The first row of an open todo, a question's or instructions': `▾ I1 · instructions · 1 of 2 todos · …` | "question header" |
| todo row | The one row of a single todo that is not open, `▸ Q1 · question · …`, shown under the open todo. One todo row is not "todo rows". | "closed row" for it |
| edge marker | `▲` or `▼` with the todos count, at the list's edge when the todo rows are scrolled out of view | — |
| decision | One choice within a question, with its ask and options | — |
| ask | A decision's own question text | — |
| option, option row | One answer a decision offers, with its key, label and impact, and its row | — |
| impact | An option's one line saying what choosing it means | — |
| suggested | The option the chief suggests, marked `(suggested)` | — |
| pick | The option chosen for a decision, marked `●` and made with Space | — |
| saved picks | The picks Harold holds for a question as the user makes them, stored in the `questions` table, before and after an answer is sent, and carried to the next question version for unchanged decisions | "draft", "unsent answer" |
| free option | An option that takes the user's own text | — |
| text field | Where the user types a free option's text | — |
| action row | A question's `send`, `relegate to idea…`, `revise…`, `reject…`, or the `done` and `ignore` of instructions | "actions" alone |
| send | The action row that sends the answer with every decision's pick, at once | "record", "send now" |
| relegate to idea | The action row `relegate to idea…` of a question, and what it does: the question leaves the todos for good, and the answers line asks the chief to keep it as an idea note | "later", "set aside", "defer", "ignore" (that is for instructions), "reject" |
| idea note | A vault note that the chief writes for a relegated question, with the question, its intro, decisions and impacts, the picks so far, your note and the spec's link and file, so the idea comes back ready to work on | "note" alone (that is a backlog note), "backlog item", "todo" |
| revise…, reject… | The action rows that open a note asking for a change, or saying why not | — |
| note | The text typed for `revise…`, `reject…` or `relegate to idea…` | "note" for a backlog note |
| notice | A short line under the todo rows, such as `Q1 changed, your picks were cleared`, `Q1 changed, picks kept for unchanged decisions` or `Harold can't save your picks right now` | — |
| question version | The count that rises each time the chief puts a question again | "version" alone |
| answers line | The line starting `ANSWERS` that Harold types into the chief's pane | "batch line" |
| settle | The 3 seconds Harold waits after the newest answer before typing an answers line, for answers not sent at once | — |
| page collection | A folder of pages that Harold serves at `/c/<name>`, named in `[[web_app.collections]]`. | "content type", "route" for it |
| collection page | One file of a page collection, at `/c/<name>/<file>`. | "artifact" (an artifact is published on claude.ai), "explainer" for any page (an explainer is one kind) |
| Pages | The web app's index at `/c`: each page collection with its page count, then Specs and Notes. | "menu" |
| spec page | Harold's page that shows one spec, at `/specs/<product>/<id>`, in the web app or on the specs listener. | "spec artifact", "spec link" for the page |
| specs listener | Harold's second, loopback-only address, `127.0.0.1:4331` by default, that serves the spec pages to the browser on the Mac. | "local server", "localhost app" |
| share a spec | Ask the chief, with the web app's **Share** button, to publish one spec as a private artifact. | "publish" for the button, "export" |
| spec file | The file `<repo>/.superpowers/pod/specs/<id>.md` of a product in the registry, and a todo's `spec_file` field that names it. | "spec" alone for the file |
| link | A todo's `link` field: a web address the chief gives for a todo with no spec file. | "spec page" for it |

## Tools

| Term | Meaning | Not to use |
| --- | --- | --- |
| tool | A module Harold hosts, with its own storage and gRPC service (ADR 0001) | — |
| question tool | The tool that holds the chief's todos (questions and instructions) and the user's answers | "question queue" as its name |
| backlog tool | The tool that holds each product's backlog and the chief's queue | — |
| knowledge tool | The tool that keeps the knowledge index and answers searches, related notes and suggested links | "search service" |
| vault | The organisation's knowledge folder of Markdown notes, which Harold reads and never writes | "notes folder", "wiki" |
| vault note | One Markdown file in the vault | "note" alone, which is a backlog note |
| knowledge index | Harold's index of the vault by meaning: its passages and embeddings in tursodb, kept durable by buckwal | "search index" alone, "vector DB", "RAG". The knowledge graph is a separate store. |
| embedding worker | The process Harold starts to run the embedding model, as the voice worker runs Whisper | "embed worker", "model server" |
| passage | A vault note, or a part of a long one, embedded as one unit | "chunk" |
| embedding | The numbers the model gives a passage or a query, close together for similar meaning | "vector" alone, "fingerprint" |
| related notes | The vault notes closest in meaning to a vault note | "similar", "neighbours" |
| knowledge graph | The vault notes, their links and their typed relations, held in TypeDB and rebuilt from the schema log and the data log | "knowledge base", "graph database" for the whole thing |
| typed relation | A relation the chief records between two vault notes, of one of the schema's types, such as `supersedes` | "link" (a `[[link]]` in a vault note's text), "edge", "suggested link" (the knowledge index's guess, never stored in the graph) |
| schema log, data log | The two OpenData logs the knowledge graph is rebuilt from: one for schema steps, one for every insert and update | "event stream" (Harold's own log), "journal" |
| graph position | The record inside TypeDB of the last schema step and data record applied, as log sequences | "cursor" (`read-cursor.json`), "checkpoint", "count" |
| schema step | One numbered addition to the knowledge graph's schema, in Harold's code | "migration" (the Turso store's) |
| graph writer | The one task in Harold that checks, logs and applies every knowledge graph change, one at a time | "worker" (the voice and embedding workers) |
| voided record | A `void_record` in the data log that tells every replay to pass one refused record | "skip", "deleted record" |
| missing vault note | A vault note whose file has gone while a typed relation still names it, kept in the knowledge graph with its relations | "orphan", "dangling" |
| suggested link | A related note scoring 0.60 or more that the vault note does not link to yet | "auto-link" |
| backlog | A product's top 5 items, ranked, and its backlog notes | — |
| item | One entry in a backlog | — |
| backlog note | A note kept with a backlog: a rule, history, held or left out | "note" alone |
| queue | The chief's own list of things to do, with one Now item. The work records replace it in the chief's use (work-records) | — |
| Now item | The queue's current item | — |
| queue item age | How long ago a queue item's text was added or last changed. Moving the item does not change it | "last updated" (a move is not an update) |
| evidence | A pointer, such as a file and line, that backs an item. Not the same as an evidence query | — |
| work record | The chief's record of one piece of work: its priority, status, what it waits for, its next step, its decisions and its links. The work tool keeps it. A work record is not a program status record | "record" alone (the glossary also has the program status record), "ticket", "task card" |
| priority (of a work record) | `focus`, `next` or `later`. At most two open work records are in focus | — |
| status (of a work record) | Where the work is, from `idea` to `checked by the user`. It is not an agent state | "state" for it |
| open, closed (of a work record) | A work record is open before `checked by the user`, and closed at it | — |
| decision (of a work record) | One thing decided about the work, with its date, who decided and its mark: `agreed`, `draft`, `dropped` or `open`. The decision of a question is one choice within that question | "decision" alone where both could be meant |
| link (of a work record) | A pointer from a work record to a spec, a question or a note. A todo's `link` is a web address | — |
| work file | One Turso file of work records. The newest is the current one | "shard" |
| status move (of a work record) | One change of the status of a work record, with its time | "transition", "event" (an event is in Harold's event store) |
| delivery lead time | The time from the first draft of a spec (when work on it starts) to the release that holds it, the sum of its four phases. It differs from the lead time of a work record (creation to the check by the user) and from spec opened to merged | "lead time" alone, "cycle time" |
| lead time (of a work record) | The time from the creation of the work record to `checked by the user`. It is not `pod.spec.lead_time`, which measures a spec from open to its release | "cycle time" |
| slice | One spec that a work record links to. A work record split into three specs has three slices | "stage" (a stage is a part of a spec, as in linux-satellite stage 2b) |
| phase (of a lead time) | A named part of a lead time. For a spec (`pod.spec.lead_time`): `open_to_approved`, `approved_to_started`, `started_to_completed`, `merged_to_released`, and the three parts of `open_to_approved`: `spec_writing`, `architect_review`, `approval_wait`, and the two parts of `started_to_completed`: `started_to_ready`, `ready_to_completed`, and `idea_to_open`: from the first entry of a backlog item in the backlog to the draft time of its spec. For a work record (`harold.work.lead_time`): `discovery`, `spec_and_approval`, `build_and_merge`, `release_and_check` and `whole` | "stage" |
| ready file | The file `ready/<id>.md` in a pod folder, written once when a build is done and waits for its merge. Pod-coordination writes it. | "done file", "finished file" |
| reviews file | The file `reviews/<id>.md` in a pod folder, with the time of each SPEC line sent to the architect and of each verdict. The chief writes it. | "review log", "arch file" |
| draft time | The time of a spec's first revision heading, which is the open time of the spec. | "creation time", "birth time" |
| factory flow page | The web app page `/flow` that shows the eight flow stages, each with its items waiting, its median wait and its levers. | "pipeline page", "dashboard" (the dashboard is HyperDX's) |
| flow stage | One of the eight steps of the factory flow: idea, spec, architect review, approval, build, merge, release, installed. | "stage" alone (a stage is a part of a spec), "phase" (a phase is a part of a lead time) |
| median wait | The median time that items spent in a flow stage, from the rows of the query `factory-flow`. | "average", "lead time" (a lead time spans several flow stages) |
| install lag | The time from a release's tag to its first start on a machine. | "deploy time", "rollout time" |
| released not installed | The specs in a release newer than the Harold that runs on this Mac. | "pending install" |
| has data | A flow stage whose numbers come from a signal that Harold records. A stage without it shows "no data yet". | "empty" (a stage with data can have 0 items) |
| rotation | Harold's move of the open work records to a new work file every `rotation_days` days | "archive" for the move |
| agent signal | A counter, gauge or event that a pane records with `harold tool telemetry`, exported as `agent.<name>`. It is not an agent's state | "custom metric" |
| agent event | An agent signal that writes one log line with declared values only | "event" alone (an event is in Harold's event store), "event stream" |
| signal prefix | The first part of an agent signal's name, `chief` or a product, which decides which panes may write it | "namespace" |
| named query | A SQL file in `docs/references/telemetry/queries/` that a dashboard card and `bin/logq --named` both run | "saved search" (a HyperDX search is another thing) |
| the big picture | The HyperDX dashboard "Harold: the big picture", six cards, each one line with a value and its change against the 7 days before | "big picture page", "KPI dashboard", "scorecard" |
| lever | A way of working that moves quality, speed, waste or token cost: spec quality, slot use, model mix, slicing and sizing, the user's waiting time, test and release time, tooling | "KPI", "metric" for it |
| card (of a dashboard) | One HyperDX chart or table on a dashboard. On the big picture, a card is one table tile with one line | "pair", "KPI" |
| work waiting (a card) | A card of "Harold trends" with the specs and work records that wait in each list, the oldest age and the item-days | "inventory" for it. The glossary's inventory is the agent monitor's pane scan. `pod.inventory.*` stays only as the name of the signal |
| inventory cost (a card) | The card of the big picture with the cost of the work not delivered, in AUD: the tokens already spent on it at the list prices converted from USD, plus your human attention | "inventory" alone, "work waiting" |
| human attention | The time the user takes to answer the chief's attention requests, as the Inventory cost card shows it, priced at the hourly rate. The query column keeps the name `attention_cost` | "attention" alone, "user attention" |
| list price | The public Anthropic API price of one million tokens of one kind for one model, in USD, in the `[prices]` setting and the gauge `harold.list_price.per_mtok` | "rate" (an hourly rate or an exchange rate), "cost" (a cost is a list price times tokens) |
| exchange rate (USD to AUD) | The number of AUD for 1 USD, in the setting `exchange_rate_usd_to_aud` and the gauge and dictionary row `harold.exchange_rate.usd_to_aud` | "FX" alone, "conversion" |
| Harold now | The HyperDX dashboard of this moment, with the agents working of all agents and the specs in progress, each one line with the time of the reading | "Now card" (the big picture has no Now card), "live dashboard" |
| release attempt | One run of `make release`, from its checks to its install or its failure | "build" for it (a build is the GitHub run) |
| push checks | The Linux and macOS workflows that run on each push to `main`. A release needs both to have passed on its commit | "CI" alone, "pre-release checks" |
| release run | The GitHub run of `build.yml` on a published release. It only builds, checks the binaries and uploads them | "build" for the whole release |
| decision record | The decision tool's record of one decision made from evidence, with its check date, status and grades. It never rotates. See the [decision reference](references/tools/decision.md) | "decision" alone, "work record decision" |
| decision tool | The tool that keeps the decision records. In its names (`harold tool decision`, `decisions.db`, `harold.decisions.*`), the bare word "decision" means a decision record | "decision" alone outside these names |
| decision details entry | The entry for one decision record, in the file next to the thing that it belongs to, written by its details owner (the PnEM or the chief) | "decision note" ("note" is a backlog note) |
| coach | The chief subagent that evaluates the due decision records | "reviewer", "architect" |
| details owner (of a decision record) | Who writes the details entry: `chief` or `pnem:<product>`. It is the field `details_owner` | "accountable" (whose decision it was), "owner" alone (the glossary uses "owner" for the owner pid of a program status record) |
| decision kind | The group of a decision record, for the coach's lessons, from `[decision] kinds` | "type", "category" |
| accountable (of a decision record) | `user` when the decision was the user's, otherwise the role that made it | "details owner" (the writer of the details entry), "owner" alone |
| process grade, outcome grade | `good` or `poor`. The process grade judges how a decision was made from the evidence known then. The outcome grade judges the result against the expected result | "score" |

## Storing and delivering

| Term | Meaning | Not to use |
| --- | --- | --- |
| event stream | Harold's ordered record of the observations and messages it accepted, kept in an OpenData log | — |
| `TurnCompleted` | The event recorded when an agent's turn ends | — |
| `InboundMessageReceived` | The event recorded when a phone reply arrives | — |
| projector | The part of Harold that reads the event stream and updates the state database and the outbox | — |
| reducer | The rules the projector applies to an agent's observations to work out its state | — |
| checkpoint | The projector's saved position in the event stream | "cursor" for the saved position |
| agent view | The control-tower's list of agents, the view it opens with | "dashboard", "main view" |
| work view | The control-tower's list of open work records at `focus` and `next`, switched with `view` | "board", "queue view" (the queue is another thing) |
| state database | The Turso database `harold-state.db` that holds the current agent states, the outbox and the checkpoint | "SQLite" for it |
| outbox | The stored list of delivery work, such as a turn-end update to send or a phone reply to route | "queue" for it |
| try | One time the delivery handler works on an outbox row. The code keeps `attempt_count` | "attempt" in text, "retry" for a single try |
| give up | The delivery handler marks an outbox row as failed for good, after its 10th failed try, or after `LONGEST_INVENTORY_WAIT` while the inventory fails | "drop", "abandon" |
| delivery handler | The part of Harold that reads the outbox, checks the `[notify]` settings and whether the user is at the desk, then leaves out or sends a turn-end update, or routes a phone reply to its pane | "dispatcher", "delivery worker" |
| channel listener | The part of Harold that receives replies from the away channel and records each in the event stream | "poller", "watcher" for it |
| snapshot | The complete current view of every agent, as the control-tower receives it. On a master, it includes each slave's agents, from the slave's snapshot over the slave link | — |
| snapshot publisher | The part of Harold that sends snapshots to clients | — |
| `WatchAgentStates` | The gRPC stream that sends a client a snapshot when it connects, then each new one | — |
| grace period | How long a hook's or a report's state wins over the screen: `hook_grace_ms`, 2 seconds, and `report_grace_ms`, 30 seconds | — |
| baseline | The prompt fingerprints Harold records on its first history capture of a pane, so text already there is never taken as new work | — |
| prompt fingerprint | A compact identifier for knowing a submitted block of text again | — |
| `PaneTyping` | The one way Harold's code types into a pane (ADR 0001) | — |
| `Writer` | Who is typing through `PaneTyping`, such as `Writer::Tool(name)` for a tool | — |

## Words with two senses

Each of these words has one sense in Harold. Anything else is named another way:

| Word | Harold's sense | For the other sense, say |
| --- | --- | --- |
| alert | A HyperDX alert typed to the chief | push (the phone) |
| summary | Never alone | work summary, activity summary, summary strip |
| note | The control-tower's revise or reject text. The web app's Notes page, reached from Pages, means vault notes, never backlog notes | backlog note, vault note |
| dash | not used: say control-tower | HyperDX dashboard |
| monitor | Never alone | agent monitor, HyperDX alert |
| state | Never alone. It has three senses: agent state (what an agent does now), spec state (the question tool's state of a spec, such as `changed`), and switch state (the `state` field, `on` or `off`, of `harold get voice` and `harold get messaging`; `off` means the switch stops it) | agent state, spec state, switch state |
| session | A tmux session, a Rex session, or a pod session | agent, for the agent itself |
| task | The work asked of an agent | background job, for Harold's own threads |
| version | Never alone | question version, release version |
| waiting | Never alone | waiting agent, waiting question |
| held | Never alone | held keys, held backlog note |
| header | The todo header | masthead, for the control-tower's top line |

## Not yet consistent

Each place in code, log lines, setting names, proto names and tests that uses a word this glossary says not to use, found with a search for each such word (`git grep -i`, leaving out docs). This change renames none of them; each is for a later change, which removes its entry. A search for the other "Not to use" words ("TUI", "coordinator" for the chief, "main agent", "kill switch", "poller", "completion notification", "delivery worker", "away app", "pane address") found no use in code. `sqlite3` in `harold/src/channels/imessage.rs` is the macOS command that reads Apple's `chat.db`, not Harold's state database, so it stays.

- The name `HAROLD_LAUNCHD`, the function `started_by_launchd` and the parameters named `launchd` mean "the service manager started Harold". This is true on Linux too. ADR 0019 keeps the name. The word would become: service manager. Where: `harold/src/main.rs`, `harold/src/main_tests.rs`, `harold/tests/install_checks.rs`, `scripts/install.py`, `scripts/harold_service.py`, `scripts/tests/test_install.py` and `scripts/tests/test_harold_service.py`. The comments of `harold/src/crash_guard.rs` and `harold/src/log_trim.rs` also say "launchd".
- `tmux_target` holds the pane label, for tmux and Rex. "Target" is a word the pane entry says not to use. It would become: `pane_label`. Where: `harold/src/agent/domain.rs`, `store.rs`, `store/migrations/001_initial.sql`, `harold-api/proto/harold.proto`, `inbound/tmux.rs`, `outbound/left_out.rs`, `agent/runtime.rs`, `voice_input/answerer.rs`, `tools/watch/facts.rs`, `main.rs` and their tests.
- `tmux_ms` in `agent inventory scan slow` is the time of the tmux listing, and `rex_ms` is its Rex twin. The pair would read better as one `listing_ms` with a `multiplexer` field. Where: `harold/src/agent/inventory.rs`, `harold/src/telemetry.rs` (`ALLOWED_FIELDS`), `docs/references/telemetry/README.md`.
- The word `dispatcher` (variables and `fake_dispatcher`) names the delivery handler. It would become: delivery handler. Where: `harold/src/main_tests.rs`: 11 uses, lines 1913–2215; `harold/src/projector.rs`: 58 uses, lines 33–945; `harold/src/projector_tests.rs`: 307 uses, lines 8–4109; `harold/src/telemetry_tests.rs`: 383.
- The word `dashboard` for the control-tower or for activity summaries (the HyperDX dashboards in `harold/src/telemetry_tests.rs` and `harold/src/pod_stats_tests.rs` keep it). It would become: the control-tower, or activity summary. Where: `harold/src/activity_summary.rs`: 15 (the model prompt); `harold/src/agent/screen_tests.rs`: 106, 118 (sample prompts); `harold/src/voice_input/shared.rs`: 19 (a doc comment); `hooks/opencode/harold-plugin.test.mjs`: 21, 129, 137 (sample prompts); `control-tower/src/ui.rs`: 9 uses, lines 1914–3025 (sample work summaries in test data).
- `send_now` is the proto field, store column and migration `002_answer_send_now.sql` of the question tool. It is Harold's API and stays. The control-tower's `send` is the action row. The phrase "send now" appears in a comment and in control-tower test strings. It would become: send (comment and test strings only). Where: `harold-api/proto/tools/questions.proto`: 124; `harold-api/tests/questions_contract.rs`: 208, 261, 267, 271, 274, 278; `harold/src/tools/questions/batch.rs`: 6; `harold/src/tools/questions/batch_tests.rs`: 78, 85, 193, 198; `harold/src/tools/questions/migrations/002_answer_send_now.sql`: 1; `harold/src/tools/questions/service.rs`: 145, 151; `harold/src/tools/questions/service_tests.rs`: 51, 60, 69, 413, 425, 439; `harold/src/tools/questions/store.rs`: 13 uses, lines 22–767; `harold/src/tools/questions/store_tests.rs`: 72, 74, 141, 157, 570, 612, 622; `harold/src/tools/questions/test_support.rs`: 186, 192, 283, 288; `harold/src/tools_tests.rs`: 495; `control-tower/src/api.rs`: 418, 1471, 1472, 1479, 1481; `control-tower/src/app.rs`: 3299; `control-tower/src/questions.rs`: 79, 618, 1083, 1094, 1180, 1224, 1246, 1483; `control-tower/src/runtime.rs`: 2492, 3069, 3073; `control-tower/src/ui.rs`: 4082, 4312, 4316.
- "question block" in a comment and control-tower test strings. It would become: todo rows. Where: `control-tower/src/app.rs`: 966; `control-tower/src/ui.rs`: 3694, 3741, 4080.
- "away device" in comments, a proto comment and help text. The command name `send-message-to-away-device` stays. It would become: phone. Where: `harold-api/proto/harold.proto`: 11; `harold/src/away_message.rs`: 1; `harold/src/channels/mod.rs`: 236; `harold/src/cli.rs`: 1157; `harold/src/main.rs`: 743; `harold/src/projector.rs`: 709; `harold/src/projector_tests.rs`: 1924, 4746; `harold/src/store.rs`: 75.
- The function `batch_line`. It would become: answers line. Where: `harold/src/tools/questions/batch.rs`: 91, 314; `harold/src/tools/questions/batch_tests.rs`: 481.
- "question queue" for the question tool in proto and code comments and control-tower text. It would become: question tool. Where: `harold-api/proto/tools/questions.proto`: 1; `harold/src/tools/questions/mod.rs`: 1; `harold/src/tools/questions/service.rs`: 1; `harold/src/tools/questions/store.rs`: 1; `control-tower/src/api.rs`: 251, 431; `control-tower/src/app.rs`: 629; `control-tower/src/questions.rs`: 3; `control-tower/src/runtime.rs`: 406, 1030, 1050, 1113.
- "The server trusts local callers" in two proto comments, and a test comment, where "the server" means Harold. It would become: Harold. Where: `harold-api/proto/tools/backlog.proto`: 37; `harold-api/proto/tools/questions.proto`: 23; `harold/src/tools_tests.rs`: 811.
- "home-screen app" for the web app in a setting's comments. It would become: web app. Where: `harold/config/local.template.toml`: 124; `harold/src/settings.rs`: 517, 1124.
- "busy" for the agent state, which ADR 0024 keeps in its own text because an accepted ADR is not rewritten. "Busy" stays for a pod slot and for a pane that a writer finds busy (`PaneBusy`). The state's old word also remains in: the field `screen_busy_since_ms` (`harold/src/agent/runtime.rs`: 210, 229); the setting `agents.busy_all` (`harold/src/settings.rs`: 550, 621; it names screen fragments, and a setting name is Harold's API); the proto alias `AGENT_STATE_BUSY` (`harold-api/proto/harold.proto`: 83; it reads old events); the chief's flag `busy` (`harold/src/web_app/chief.rs`: 11 to 13) and the tests about a busy chief (`harold/src/voice_input_tests.rs`, `voice_input_call_tests.rs`, `alerts_tests.rs`, `inbound/mod_tests.rs`); and the tests of the migration and replay of old events (`harold/src/store_tests.rs`: 2846, 2872, 2966, 3020), where "busy" is the old stored word. It would become: working. This change renamed `BUSY_SINCE`, `pane_turned_busy` and `take_busy_since` in `harold/src/telemetry.rs` (now `WORKING_SINCE`, `pane_turned_working`, `take_working_since`).
- "alert" for a push, wherever it appears in code: the button label `Turn on alerts`, `alerts.js`, and comments, constants and test names in the web app and the store. HyperDX alerts (`harold/src/alerts.rs`, telemetry, timed prompts, settings) and JavaScript's `alert()` in test strings keep the word. It would become: push. Where: `harold/src/web_app/flow_tests.rs`: 81; `harold/src/web_app/origin.rs`: 101; `harold/src/web_app/pages.rs`: 18, 210; `harold/src/web_app/push.rs`: 1, 3, 6, 38, 39, 344, 345, 346; `harold/src/web_app/push_tests.rs`: 6, 293, 295, 296; `harold/src/web_app/replies.rs`: 35; `harold/src/web_app/route.rs`: 2, 8, 50; `harold/src/web_app/route_tests.rs`: 62, 66, 83; `harold/src/web_app/server_tests.rs`: 241, 256; `harold/static/web_app/alerts.js`: 1, 8; `harold/static/web_app/sw.js`: 1; `harold/templates/web_app/thread.html`: 15, 21; `harold/src/outbound/mod.rs`: 325, 327, 335; `harold/src/projector_tests.rs`: 4089; `harold/src/store.rs`: 117, 128.

- "selected" and "selection" for the cursor in the control-tower (177 uses; the inventory's process selection and the vendored `datastar-1.0.2.js` keep the word). It would become: cursor. Where: `control-tower/examples/control_tower_demo.rs`: 60, 85, 117; `control-tower/src/app.rs`: 101 uses, lines 249–3961; `control-tower/src/runtime.rs`: 1219, 2875, 2885, 2913, 3164; `control-tower/src/ui.rs`: 68 uses, lines 557–4503.
- "monitor" alone for the agent monitor (62 uses; the `MONITOR …` control-tower literals, `monitor-timeouts` and `monitor-trouble` names, and HyperDX monitors keep the word). It would become: agent monitor. Where: `harold/src/agent/domain.rs`: 3; `harold/src/agent/inventory.rs`: 78; `harold/src/agent/reducer.rs`: 3; `harold/src/agent/runtime.rs`: 81; `harold/src/agent/runtime_tests.rs`: 9 uses, lines 39–4979; `harold/src/agent/screen.rs`: 3; `harold/src/agent/summary.rs`: 3; `harold/src/main.rs`: 11 uses, lines 57–1050; `harold/src/main_tests.rs`: 8 uses, lines 83–1616; `harold/src/outbound/left_out.rs`: 29, 55; `harold/src/projector_tests.rs`: 3003; `harold/src/settings.rs`: 9 uses, lines 225–1138; `harold/src/store.rs`: 7 uses, lines 1836–1984; `harold/src/store_tests.rs`: 1419, 1425; `harold/src/timed_prompts.rs`: 36, 1045, 1046, 1047, 1048; `control-tower/src/ui.rs`: 56, 71.
- "notification" alone, for three meanings. The `[notify]` setting, `notify()` and the browser's Notification API keep the word. Where: `harold/src/agent/runtime_tests.rs`: 3857; `harold/src/channels/imessage.rs`: 58, 60, 176, 179, 210, 738; `harold/src/channels/imessage_tests.rs`: 127, 130, 134; `harold/src/channels/mod.rs`: 52, 70, 182, 192; `harold/src/main.rs`: 721, 729, 734, 756; `harold/src/outbound/left_out.rs`: 22, 150, 423, 476; `harold/src/outbound/mod.rs`: 10 uses, lines 35–739; `harold/src/outbound/notify_tests.rs`: 527, 549; `harold/src/outbound/tts.rs`: 96, 193, 217; `harold/src/projector.rs`: 440; `harold/src/projector_tests.rs`: 1858, 1910; `harold/src/store.rs`: 1170.
  - **turn-end update**, where it means what the delivery handler says or sends when a turn ends: the uses in the files above.
  - **push**, where it means a lock-screen notification: `harold/static/web_app/alerts.js`: 91; `harold/static/web_app/sw.js`: 6, 9, 10. These are the browser's own API names (`Notification.requestPermission`, `showNotification`, `notificationclick`) and keep the word.
  - **phone notification**, where it means the whole set of tagged messages to the phone: the latest-notifier routing in `harold/src/inbound/` (`mod.rs`: 149, 185, 188, 190; `ownership.rs`, 14 uses, lines 5–297; `ownership_tests.rs`: 7, 18, 23, 27, 178, 491) and `harold/src/settings.rs`: 524, 713, and the install prompt `Notification channel` in `scripts/install.py`: 220.
- "summary" alone for the work summary or activity summary (128 uses; the text of a turn-end update, such as the TTS summary and `build_short_summary` in `harold/src/outbound/` and `harold/src/channels/mod.rs`, and module paths such as `crate::agent::summary`, keep the word; the `summary` layout slot in `control-tower/src/ui.rs` would become "summary strip"). It would become: work summary or activity summary. Where: `harold/config/default.toml`: 60; `harold/src/activity_summary.rs`: 223, 225, 226, 230; `harold/src/agent/mod.rs`: 7; `harold/src/agent/reducer.rs`: 8 uses, lines 80–365; `harold/src/agent/runtime.rs`: 46 uses, lines 31–1692; `harold/src/agent/runtime_tests.rs`: 2638, 2644, 4128; `harold/src/agent/screen.rs`: 25; `harold/src/agent/snapshot_tests.rs`: 8, 39, 40, 46; `harold/src/agent/summary_tests.rs`: 2; `harold/src/main_tests.rs`: 162, 199, 200, 206, 798, 811; `harold/src/settings.rs`: 1179; `harold/src/store.rs`: 1179, 2005, 2006; `harold/src/store_tests.rs`: 899, 900, 902, 903; `hooks/opencode/harold-plugin-core.mjs`: 198, 200; `hooks/opencode/harold-plugin.test.mjs`: 8 uses, lines 20–174; `control-tower/src/api.rs`: 586, 811, 817, 820, 824, 838; `control-tower/src/text.rs`: 83, 84, 85; `control-tower/src/ui.rs`: 26 uses, lines 56–4081.

- "session" for the agent (16 uses). The user-facing reply `No active agent sessions found.` is a literal that tests check. It would become: agent. Where: `harold/src/channels/imessage_tests.rs`: 142, 143, 154, 399; `harold/src/inbound/directory.rs`: 5; `harold/src/inbound/mod.rs`: 316; `harold/src/inbound/mod_tests.rs`: 264; `harold/src/inbound/ownership_tests.rs`: 34, 145, 205, 456, 457, 482.
- "focus" for the cursor, in a control-tower comment (1 use). The `focus tracking`, `FocusReading` and `focused_pane` names are the glossary's focus tracking and tmux's own term and stay. It would become: the cursor never lands on it. Where: `control-tower/src/app.rs`: 255.
- The source value "dash" leaves the program. The control-tower sends it with each knowledge search, and Harold puts it on the search span. The value stays "dash" so that trace queries on `source="dash"` keep working. It would become: control-tower, if the value changes. Where: `harold/src/tools/knowledge/service.rs`.
- "telemetry off" for export off, in comments. It would become: export off. Where: `harold/src/telemetry.rs`: 143, 179, 186, 557; `harold/src/store/event_log.rs`: 394; `harold/src/telemetry_tests.rs`: 1396.
- "task" for a thread of Harold's own, in comments (`tokio::task` names stay). It would become: background job. Where: `harold/src/inbound/mod.rs`: 230; `harold/src/timed_prompts.rs`: 40, 43, 541, 1007, 1063; `harold/src/tools.rs`: 98, 116, 266, 269; `harold/src/pod_stats.rs`: 1210; `harold/src/store/error.rs`: 30; `harold/src/store_tests.rs`: 1971; `harold/src/timed_prompts_tests.rs`: 936, 1327; `harold/src/tools/questions/test_support.rs`: 358; `harold/src/voice_input/phone.rs`: 381, 680. The field `task` of `ToolHandle` in `harold/src/tools.rs` is a name and stays.
- "note" alone for a backlog note, in comments and prose of the backlog tool (25 comment lines). The proto message `Note` and fields `notes` and `note`, the SQL table `notes`, the Rust types `Note` and `NoteKind`, the CLI words `note` and `notes`, the queue's note, and the fixtures' plain English keep the word. It would become: backlog note. Where: `harold/src/tools/backlog/markdown.rs`: 11, 38, 229, 297, 413; `harold/src/tools/backlog/markdown_tests.rs`: 24; `harold/src/tools/backlog/schema.rs`: 1, 13, 46, 188; `harold-api/proto/tools/backlog.proto` (comments only): 1, 5, 20, 23; `harold/src/tools/backlog/service.rs`: 295; `harold/src/tools/backlog/store.rs`: 10 uses, lines 78–627.
- "call" for an RPC or a request, in comments and one user-facing CLI message: `harold/src/cli.rs`: 586, 744, 859, 2366 ("the Harold daemon refused the call"); `harold/src/tools_tests.rs`: 115. It would become: RPC or request. The voice `Call` RPC (`harold-api/proto/harold.proto`) keeps its name, and "call" as a verb stays.

One line is kept as it stands:

- **Older words in ADRs, kept as written.** ADRs are records of their time, so their words stay. Words the glossary says not to use that the ADRs hold:
  - `docs/adr/0001-tools-are-modules-hosted-by-harold.md:8`: "question queue" (the question tool).
  - `docs/adr/0002-tool-services-trust-local-callers.md:8`: "peer calls" (peer RPCs).

The words "process", "target", "slot", "queue", "plugin", "watcher", "header", "record", "version", "waiting" and "held" were also searched in code and docs. They are used there in ordinary senses or as names (a build target, an HTTP header), and need no entry.

Words kept in the docs on purpose, one line each. They are plain English or a name, not the glossary's term, so the search for each finds them there:

- **"notification"** stays in names: the page name and link text "Notification" (`docs/references/notification/`, `docs/README.md`), log lines such as `notification skipped (…)` and those quoted in `docs/references/notification/README.md`, the setting `[notify]`, the dated `docs/security-audit-2026-02-27.md`, and iOS's notification permission in `docs/references/away-app/README.md`. Anywhere else the docs say "phone notification", "turn-end update" or "push".
- **"summary"** stays in compound words that name a work summary candidate ("explicit-summary", "fallback-summary prefix", "generated-summary", "completion summary update", "summary isolation"), and where it is the text of a turn-end update, such as "Harold's summary of a finished turn" (`docs/references/notification/README.md`) and the opening of `README.md`, or the heading "Summary" of `docs/security-audit-2026-02-27.md`.
- **"call"** stays for the voice call and for "calls" of a person's tool or a hook in plain English. The `Call` RPC keeps its name.
- **"version"** stays for a release or schema version, and for an event's stream version.
- **"waiting"**, **"held"** and **"header"** stay in their ordinary senses: "waiting" for time passing, "held back", an HTTP header. The senses the glossary names are reworded, except where one of the lines below says otherwise.
- **"the server"** stays for the web app's HTTP server (`docs/references/away-app/README.md`) and an example question in the control-tower README.
- **"away device"** stays only in the command and skill name `send-message-to-away-device`.
- **"dashboard"** stays for HyperDX dashboards (`docs/references/telemetry/README.md`) and the file path `tasks/tmux-agent-dashboard/`.
- **"session"** stays where it is the provider's own word: a Claude Code or OpenCode session (`session.id`, `session.status`, `claude --plugin-dir` runs), a graphical login session, a HyperDX per-session metric, and the heading `In flight with another session` of the backlog file. A tmux session and a pod session are named by the glossary. Where the agent itself was meant, the docs say "agent".
- **"process"** stays for an operating-system process (the agent process, a child process, a provider process). The glossary's "agent" is the pane's coding agent; "process inventory" and "incarnation" name the process-level facts.
- **"task"** stays for the work asked of an agent, and in HyperDX/telemetry text. Harold's own threads are "background job" in the docs.
- **"target"**, **"slot"**, **"queue"**, **"plugin"**, **"watcher"**, **"focus"**, **"header"**, **"actions"** and **"record"** stay in their ordinary senses: a build target, a telemetry slot, a message queue in HyperDX, an OpenCode plugin and the `--plugin-dir` flag, the snapshot watcher command of the hook guide, the filesystem watcher, focus tracking, an HTTP header, the proto field `action`, and "record" as a verb. The senses the glossary bans are reworded.
- **"note"** and **"call"** stay in plain English ("the file's notes", "a note of the voice file", "the call to a function"), and wherever they are the `kind` or CLI words of the backlog tool or the voice call. "note" for a backlog note is reworded to "backlog note", except in the kind-qualified forms "rule note", "history note", "held note" and "left-out note".
- **"selected"** and **"selection"** stay where they mean a choice made by configuration or the agent monitor (the selected adapter, process selection, the selected `HAROLD_ENV` overlay, a prompt candidate) or the endpoint the control-tower shows. Where the control-tower's cursor was meant, the docs say "cursor".
- **"monitor"** stays in `monitor-timeouts` (a spec's name) and the `MONITOR DEGRADED`, `MONITOR UNKNOWN` and `MONITOR HEALTHY` literals.
