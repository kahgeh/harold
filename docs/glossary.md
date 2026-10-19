# Glossary

This page lists the terms Harold's specs, code, logs, settings and docs use, each with one meaning and the words not to use for it. A change that names a new term adds it here, with its meaning and the words not to use, in the same change.

## Parts and people

| Term | Meaning | Not to use |
| --- | --- | --- |
| Harold | The daemon on each Mac that watches agents, notifies the user, routes replies and hosts tools | "the server" |
| haroldctl | The command that starts, stops and checks the installed Harold service | — |
| tmx-agent-dash, the dash | The terminal dashboard that lists agents and holds the chief's questions | "dashboard" for the dash (HyperDX has dashboards), "TUI" |
| agent | A coding agent running in a tmux pane: Claude Code, Codex or OpenCode | "session" for the agent, "process" for the agent |
| provider | Which kind an agent is: `claude`, `codex` or `opencode` | — |
| pane, pane id | The tmux pane an agent runs in, and its id, such as `%47` | "pane address", "target" |
| incarnation | One run of an agent in a pane, told apart by pane, process, start time and provider. A new one starts at Unknown | — |
| the chief, chief of pods | The agent in the chief's folder, which talks to the user and asks them questions | "coordinator", "main agent" |
| the chief's row | The chief's row in the dash's agent list | — |
| pod | One product's sessions: its PnEM, its pod coordination and its build workers | — |
| pod session | A pod-coordination or PnEM pane | — |
| build worker | An agent building one spec in its own worktree | "slot" for the worker |
| slot | A place for one build worker in a pod, busy or free | — |
| pod registry | The vault's `products.toml`, named by `[telemetry] pod_registry` | — |
| user | The person Harold works for | — |
| phone | The user's iPhone, reached by iMessage, Telegram or the web app | "away device", except in the skill name `send-message-to-away-device` |

## Where the user is

| Term | Meaning | Not to use |
| --- | --- | --- |
| at the desk | The Mac's screen is unlocked, so Harold speaks on the Mac | — |
| away | The Mac's screen is locked, so Harold writes to the phone | — |
| away channel | The one of iMessage and Telegram that `[notify] away_channel` picks | — |
| web app | The home-screen app on the phone that holds the chief's thread | "away app", "home-screen app" |
| thread | The web app's one conversation with the chief | — |
| push | A lock-screen notification on the phone, sent through the phone's push service | "alert" for a push |
| machine name | This Mac's name, as tagged on its messages | — |
| hub, member | With Telegram on two Macs: the hub reads the chat and forwards replies, a member gets them from the hub | — |
| routing tag | The `[machine/pane]` text at the start of a reply that picks its pane | — |

## Watching agents

| Term | Meaning | Not to use |
| --- | --- | --- |
| agent monitor | The part of Harold that finds agents and works out their state | "monitor" alone |
| inventory, process inventory | The agent monitor's regular scan of tmux panes and the agent processes in them | — |
| screen adapter | The per-provider reader of an agent's visible screen, such as `claude-v1` | — |
| hook | A small script a provider runs to tell Harold what an agent did | — |
| Claude Code mod | The Claude Code plugin in `hooks/claude-mod` that reports a working pane | "plugin" for it. OpenCode's is an OpenCode plugin |
| Busy, Idle, Unknown | An agent's state | — |
| turn | One request to an agent and its work until it stops | — |
| task | The work the user asks an agent to do. Its short text is the agent's work summary | "task" for a thread of Harold's own, say "background job" |
| work summary | Short text saying what an agent is working on | "task description", "current work", "dashboard description", "summary" alone |
| activity summary | A work summary that Claude Code writes from the agent's evidence, when `[activity_summary]` is on | "generated description" |
| waiting agent | An agent that went from Busy to Idle while the user wasn't looking at it, marked `◆` | — |
| focus tracking | The dash's check, once a second, of which pane the user's tmux client shows | — |

## Talking to the user

| Term | Meaning | Not to use |
| --- | --- | --- |
| turn-end update | What Harold says or sends when an agent's turn ends, as delivery work that the `TurnCompleted` event creates | "completion notification", "notification" alone |
| agent message | Text an agent asks Harold to send to the phone (`send-message-to-away-device`) | — |
| spoken line | A line the chief asks Harold to say (`harold speak`) | — |
| voice note | A spoken line sent to the phone as audio | — |
| left out | A turn-end update Harold chose not to say or send | — |
| dropped | An agent message or spoken line Harold chose not to send | — |
| voice, voice mute | Harold's speech at the desk, and the switch that silences it (`Voice: mute`) | — |
| messaging pause | The switch that stops all phone traffic in both directions (`Messaging: pause`) | "kill switch" as its name |
| call | A voice conversation with the chief, at the Mac or in the web app | "call" for an RPC or a function |
| talk now | The key, `t`, that cuts the chief's answer short in a call | — |
| voice worker | The process that turns lines into audio and speech into text | — |
| timed prompt | Text Harold types into a pane at a set time each day | — |
| alert | A HyperDX alert that Harold types to the chief as `harold alert: …` | "alert" for a push |

## The dash

| Term | Meaning | Not to use |
| --- | --- | --- |
| masthead | The dash's top line: version, transport, REV, voice and the messaging pause | "header" for it |
| summary strip | The line of BUSY, IDLE and SERVER TIME counts under the masthead | "summary" alone |
| transport | The dash's connection to Harold: `LIVE`, `STALE` and the like | — |
| filter | The dash's search, which hides agent rows that don't match | — |
| agent row | A row of the agent list for one agent | — |
| cursor | The row the dash's keys act on, marked `▶` | "focus", "selection" |
| Now line | The dim `NOW` line under the masthead with the chief's Now item | — |
| detail panel | The panel beside the agent list, on a wide terminal, about the agent under the cursor | — |
| held keys | Keys the dash ignores after a question changed under a note, until Esc | "held" alone |

## Questions

| Term | Meaning | Not to use |
| --- | --- | --- |
| question | Something the chief asks the user, with an id such as `T1` and one or more decisions | — |
| waiting question | A question not yet answered, set aside or dismissed | "waiting" alone |
| question rows | The rows the questions take in the agent list, under the asking agent's row: the closed row, or the question header and every row under it | "question block" |
| closed row | The one row `▸ n questions waiting` that stands for the questions while closed | — |
| question header | The first of an open question's rows: `▾ T1 · 1 of 2 waiting · …` | — |
| edge marker | `▲` or `▼` with the count, at the list's edge when the question rows are scrolled out of view | — |
| decision | One choice within a question, with its ask and options | — |
| ask | A decision's own question text | — |
| option, option row | One answer a decision offers, with its key, label and impact, and its row | — |
| impact | An option's one line saying what choosing it means | — |
| suggested | The option the chief suggests, marked `(suggested)` | — |
| pick | The option chosen for a decision, marked `●` and made with Space | — |
| free option | An option that takes the user's own text | — |
| text field | Where the user types a free option's text | — |
| action row | One of the rows under the options: `send`, `later`, `revise…`, `reject…` | "actions" alone |
| send | The action row that sends the answer with every decision's pick, at once | "record", "send now" |
| later, set aside | The action row `later`, and what it does: the question waits until no other question is waiting | "defer" and "deferred" in what the user reads. `DeferQuestion` is the API's name |
| revise…, reject… | The action rows that open a note asking for a change, or saying why not | — |
| note | The text typed for `revise…` or `reject…` | "note" for a backlog note |
| notice | A short line under the question rows, such as `T1 changed, your picks were cleared` | — |
| question version | The count that rises each time the chief puts a question again | "version" alone |
| answers line | The line starting `ANSWERS` that Harold types into the chief's pane | "batch line" |
| settle | The 3 seconds Harold waits after the newest answer before typing an answers line, for answers not sent at once | — |

## Tools

| Term | Meaning | Not to use |
| --- | --- | --- |
| tool | A module Harold hosts, with its own storage and gRPC service (ADR 0001) | — |
| question tool | The tool that holds the chief's questions and the user's answers | "question queue" as its name |
| backlog tool | The tool that holds each product's backlog and the chief's queue | — |
| backlog | A product's top 5 items, ranked, and its backlog notes | — |
| item | One entry in a backlog | — |
| backlog note | A note kept with a backlog: a rule, history, held or left out | "note" alone |
| queue | The chief's own list of things to do, with one Now item | — |
| Now item | The queue's current item | — |
| evidence | A pointer, such as a file and line, that backs an item | — |

## Storing and delivering

| Term | Meaning | Not to use |
| --- | --- | --- |
| event stream | Harold's ordered record of the observations and messages it accepted, kept in an OpenData log | — |
| `TurnCompleted` | The event recorded when an agent's turn ends | — |
| `InboundMessageReceived` | The event recorded when a phone reply arrives | — |
| projector | The part of Harold that reads the event stream and updates the state database and the outbox | — |
| reducer | The rules the projector applies to an agent's observations to work out its state | — |
| checkpoint | The projector's saved position in the event stream | — |
| state database | The Turso database `harold-state.db` that holds the current agent view, the outbox and the checkpoint | "SQLite" for it |
| outbox | The stored list of delivery work, such as a turn-end update to send or a phone reply to route | "queue" for it |
| delivery handler | The part of Harold that reads the outbox, checks the notification rules and whether the user is at the desk, then leaves out, speaks or sends a turn-end update, or routes a phone reply to its pane | "dispatcher", "delivery worker" |
| channel listener | The part of Harold that receives phone messages from the away channel and records each in the event stream | "poller", "watcher" for it |
| snapshot | The complete current view of every agent, as the dash receives it | — |
| snapshot publisher | The part of Harold that sends snapshots to clients | — |
| `WatchAgentStates` | The gRPC stream that sends a client a snapshot when it connects, then each new one | — |
| grace period | How long a hook's or a report's state wins over the screen: `hook_grace_ms`, 2 seconds, and `report_grace_ms`, 30 seconds | — |
| baseline | The prompt fingerprints Harold records on its first history capture of a pane, so text already there is never taken as new work | — |
| prompt fingerprint | A compact identifier for knowing a submitted block of text again | — |
| `PaneTyping` | The one way Harold's code types into a pane (ADR 0001) | — |
| `Writer` | Who is typing through `PaneTyping`, such as `Writer::Tool(name)` for a tool | — |
| peer RPCs | `ReportNotification` and `ForwardReply`, the two gRPC calls between Harold on two Macs, which carry a shared secret (ADR 0002) | "peer calls" |

## Words with two senses

Each of these words has one sense in Harold. Anything else is named another way:

| Word | Harold's sense | For the other sense, say |
| --- | --- | --- |
| alert | A HyperDX alert typed to the chief | push (the phone) |
| call | A voice call with the chief | RPC (such as the peer RPCs), or the function's name |
| summary | Never alone | work summary, activity summary, summary strip |
| note | The dash's revise or reject text | backlog note |
| dash | tmx-agent-dash | HyperDX dashboard |
| monitor | Never alone | agent monitor, HyperDX alert |
| session | A tmux session, or a pod session | agent, for the agent itself |
| task | The work asked of an agent | background job, for Harold's own threads |
| version | Never alone | question version, release version |
| waiting | Never alone | waiting agent, waiting question |
| held | Never alone | held keys, held backlog note |
| header | The question header | masthead, for the dash's top line |

## Not yet consistent

Each place in code, log lines, setting names, proto names and tests that uses a word this glossary says not to use, found with a search for each such word (`git grep -i`, leaving out docs). This change renames none of them; each is for a later change, which removes its entry. A search for the other "Not to use" words ("TUI", "coordinator", "main agent", "kill switch", "poller", "completion notification", "delivery worker", "away app", "pane address") found no use in code. `sqlite3` in `harold/src/channels/imessage.rs` is the macOS command that reads Apple's `chat.db`, not Harold's state database, so it stays.

- The word `dispatcher` (variables and `fake_dispatcher`) names the delivery handler. It would become: delivery handler. Where: `harold/src/main_tests.rs`: 11 uses, lines 1913–2215; `harold/src/projector.rs`: 58 uses, lines 33–945; `harold/src/projector_tests.rs`: 307 uses, lines 8–4109; `harold/src/telemetry_tests.rs`: 383.
- The word `dashboard` for the dash or for activity summaries (the HyperDX dashboards in `harold/src/telemetry_tests.rs` and `harold/src/pod_stats_tests.rs` keep it). It would become: the dash, or activity summary. Where: `harold/config/local.template.toml`: 21; `harold/src/activity_summary.rs`: 15; `harold/src/agent/screen_tests.rs`: 105, 117; `harold/src/voice_input.rs`: 130; `hooks/opencode/harold-plugin.test.mjs`: 21, 129, 137; `scripts/install.py`: 347; `scripts/tests/test_install.py`: 516, 799, 932; `tmx-agent-dash/examples/dashboard_demo.rs`: 90; `tmx-agent-dash/src/api.rs`: 519, 529; `tmx-agent-dash/src/app.rs`: 168; `tmx-agent-dash/src/runtime.rs`: 63, 3397; `tmx-agent-dash/src/terminal.rs`: 284; `tmx-agent-dash/src/ui.rs`: 23 uses, lines 1633–2712.
- `send_now` is the proto field, store column and migration `002_answer_send_now.sql` of the question tool. It is Harold's API and stays. The dash's `send` is the action row. The phrase "send now" appears in a comment and in dash test strings. It would become: send (comment and test strings only). Where: `harold-api/proto/tools/questions.proto`: 124; `harold-api/tests/questions_contract.rs`: 208, 261, 267, 271, 274, 278; `harold/src/tools/questions/batch.rs`: 6; `harold/src/tools/questions/batch_tests.rs`: 78, 85, 193, 198; `harold/src/tools/questions/migrations/002_answer_send_now.sql`: 1; `harold/src/tools/questions/service.rs`: 145, 151; `harold/src/tools/questions/service_tests.rs`: 51, 60, 69, 413, 425, 439; `harold/src/tools/questions/store.rs`: 13 uses, lines 22–767; `harold/src/tools/questions/store_tests.rs`: 72, 74, 141, 157, 570, 612, 622; `harold/src/tools/questions/test_support.rs`: 186, 192, 283, 288; `harold/src/tools_tests.rs`: 495; `tmx-agent-dash/src/api.rs`: 418, 1471, 1472, 1479, 1481; `tmx-agent-dash/src/app.rs`: 3299; `tmx-agent-dash/src/questions.rs`: 79, 618, 1083, 1094, 1180, 1224, 1246, 1483; `tmx-agent-dash/src/runtime.rs`: 2492, 3069, 3073; `tmx-agent-dash/src/ui.rs`: 4082, 4312, 4316.
- "question block" in a comment and dash test strings. It would become: question rows. Where: `tmx-agent-dash/src/app.rs`: 966; `tmx-agent-dash/src/ui.rs`: 3694, 3741, 4080.
- The dash's `CURRENT WORK` detail panel label and its tests. It would become: work summary. Where: `tmx-agent-dash/src/ui.rs`: 1249, 1715, 1871, 1905, 2287, 2473, 2484, 2509.
- "away device" in comments, a proto comment, help text and log lines such as `spoken line sent to the away device`. The command name `send-message-to-away-device` stays. It would become: phone. Where: `harold-api/proto/harold.proto`: 15; `harold/src/away_message.rs`: 1; `harold/src/channels/mod.rs`: 208; `harold/src/cli.rs`: 595; `harold/src/main.rs`: 765; `harold/src/outbound/fake.rs`: 120; `harold/src/outbound/left_out.rs`: 1; `harold/src/outbound/mod.rs`: 193; `harold/src/projector.rs`: 474, 476, 537; `harold/src/projector_tests.rs`: 1917, 1974; `harold/src/settings.rs`: 479; `harold/src/store.rs`: 70.
- The function `batch_line`. It would become: answers line. Where: `harold/src/tools/questions/batch.rs`: 91, 314; `harold/src/tools/questions/batch_tests.rs`: 481.
- "peer call" in comments and the log text `peer call rejected`. It would become: peer RPC. Where: `harold/src/main.rs`: 72, 331, 345; `harold/src/peers.rs`: 63.
- "question queue" for the question tool in proto and code comments and dash text. It would become: question tool. Where: `harold-api/proto/tools/questions.proto`: 1; `harold/src/tools/questions/mod.rs`: 1; `harold/src/tools/questions/service.rs`: 1; `harold/src/tools/questions/store.rs`: 1; `tmx-agent-dash/src/api.rs`: 251, 431; `tmx-agent-dash/src/app.rs`: 629; `tmx-agent-dash/src/questions.rs`: 3; `tmx-agent-dash/src/runtime.rs`: 406, 1030, 1050, 1113.
- "The server trusts local callers" in two proto comments, and a test comment, where "the server" means Harold. It would become: Harold. Where: `harold-api/proto/tools/backlog.proto`: 37; `harold-api/proto/tools/questions.proto`: 23; `harold/src/tools_tests.rs`: 811.
- "home-screen app" for the web app in a setting's comments. It would become: web app. Where: `harold/config/local.template.toml`: 108; `harold/src/settings.rs`: 425, 837.
- "alert" for a push, wherever it appears in code: the button label `Turn on alerts`, `alerts.js`, and comments, constants and test names in the web app and the store. HyperDX alerts (`harold/src/alerts.rs`, telemetry, timed prompts, settings) and JavaScript's `alert()` in test strings keep the word. It would become: push. Where: `harold/src/web_app/flow_tests.rs`: 80; `harold/src/web_app/origin.rs`: 101; `harold/src/web_app/pages.rs`: 18, 181; `harold/src/web_app/push.rs`: 1, 3, 6, 38, 39, 344, 345, 346; `harold/src/web_app/push_tests.rs`: 6, 293, 295, 296; `harold/src/web_app/replies.rs`: 35; `harold/src/web_app/route.rs`: 2, 8, 50; `harold/src/web_app/route_tests.rs`: 62, 66, 83; `harold/src/web_app/server_tests.rs`: 233, 248; `harold/static/web_app/alerts.js`: 1, 8; `harold/static/web_app/sw.js`: 1; `harold/templates/web_app/thread.html`: 13, 19; `harold/src/outbound/mod.rs`: 320, 322, 330; `harold/src/projector_tests.rs`: 4047; `harold/src/store.rs`: 117, 128.

- "selected" and "selection" for the cursor in the dash (177 uses; the inventory's process selection and the vendored `datastar-1.0.2.js` keep the word). It would become: cursor. Where: `tmx-agent-dash/examples/dashboard_demo.rs`: 60, 85, 117; `tmx-agent-dash/src/app.rs`: 101 uses, lines 249–3961; `tmx-agent-dash/src/runtime.rs`: 1219, 2875, 2885, 2913, 3164; `tmx-agent-dash/src/ui.rs`: 68 uses, lines 557–4503.
- "monitor" alone for the agent monitor (62 uses; the `MONITOR …` dash literals, `monitor-timeouts` and `monitor-trouble` names, and HyperDX monitors keep the word). It would become: agent monitor. Where: `harold/src/agent/domain.rs`: 3; `harold/src/agent/inventory.rs`: 78; `harold/src/agent/reducer.rs`: 3; `harold/src/agent/runtime.rs`: 81; `harold/src/agent/runtime_tests.rs`: 9 uses, lines 39–4979; `harold/src/agent/screen.rs`: 3; `harold/src/agent/summary.rs`: 3; `harold/src/main.rs`: 11 uses, lines 57–1050; `harold/src/main_tests.rs`: 8 uses, lines 83–1616; `harold/src/outbound/left_out.rs`: 29, 55; `harold/src/projector_tests.rs`: 3003; `harold/src/settings.rs`: 9 uses, lines 225–1138; `harold/src/store.rs`: 7 uses, lines 1836–1984; `harold/src/store_tests.rs`: 1419, 1425; `harold/src/timed_prompts.rs`: 36, 1045, 1046, 1047, 1048; `tmx-agent-dash/src/ui.rs`: 56, 71.
- "notification" alone for a turn-end update (48 uses; `[notify]`, `notify()`, the browser's Notification API, `ReportNotification` and its log lines, the latest-notifier routing in `harold/src/inbound/` and `harold/src/settings.rs`, the hub report in `harold/src/channels/telegram.rs`, and the install prompt `Notification channel` in `scripts/install.py` keep the word until the glossary has a term for any tagged message to the phone). It would become: turn-end update, or push for a lock-screen notification. Where: `harold/src/agent/runtime_tests.rs`: 3857; `harold/src/channels/imessage.rs`: 58, 60, 176, 179, 210, 738; `harold/src/channels/imessage_tests.rs`: 127, 130, 134; `harold/src/channels/mod.rs`: 52, 70, 182, 192; `harold/src/channels/telegram.rs`: 7 uses, lines 186–816; `harold/src/main.rs`: 721, 729, 734, 756; `harold/src/outbound/left_out.rs`: 22, 150, 423, 476; `harold/src/outbound/mod.rs`: 10 uses, lines 35–739; `harold/src/outbound/notify_tests.rs`: 527, 549; `harold/src/outbound/tts.rs`: 96, 193, 217; `harold/src/projector.rs`: 440; `harold/src/projector_tests.rs`: 1858, 1910; `harold/src/store.rs`: 1170.
- "summary" alone for the work summary or activity summary (128 uses; the text of a turn-end update, such as the TTS summary and `build_short_summary` in `harold/src/outbound/` and `harold/src/channels/mod.rs`, and module paths such as `crate::agent::summary`, keep the word; the `summary` layout slot in `tmx-agent-dash/src/ui.rs` would become "summary strip"). It would become: work summary or activity summary. Where: `harold/config/default.toml`: 60; `harold/src/activity_summary.rs`: 223, 225, 226, 230; `harold/src/agent/mod.rs`: 7; `harold/src/agent/reducer.rs`: 8 uses, lines 80–365; `harold/src/agent/runtime.rs`: 46 uses, lines 31–1692; `harold/src/agent/runtime_tests.rs`: 2638, 2644, 4128; `harold/src/agent/screen.rs`: 25; `harold/src/agent/snapshot_tests.rs`: 8, 39, 40, 46; `harold/src/agent/summary_tests.rs`: 2; `harold/src/main_tests.rs`: 162, 199, 200, 206, 798, 811; `harold/src/settings.rs`: 1179; `harold/src/store.rs`: 1179, 2005, 2006; `harold/src/store_tests.rs`: 899, 900, 902, 903; `hooks/opencode/harold-plugin-core.mjs`: 198, 200; `hooks/opencode/harold-plugin.test.mjs`: 8 uses, lines 20–174; `tmx-agent-dash/src/api.rs`: 586, 811, 817, 820, 824, 838; `tmx-agent-dash/src/text.rs`: 83, 84, 85; `tmx-agent-dash/src/ui.rs`: 26 uses, lines 56–4081.

- "session" for the agent (16 uses). The user-facing reply `No active agent sessions found.` is a literal that tests check. It would become: agent. Where: `harold/src/channels/imessage_tests.rs`: 142, 143, 154, 399; `harold/src/channels/telegram.rs`: 682, 683, 708; `harold/src/inbound/directory.rs`: 5; `harold/src/inbound/mod.rs`: 316; `harold/src/inbound/mod_tests.rs`: 264; `harold/src/inbound/ownership_tests.rs`: 34, 145, 205, 456, 457, 482.
- "focus" for the cursor, in a dash comment (1 use). The `focus tracking`, `FocusReading` and `focused_pane` names are the glossary's focus tracking and tmux's own term and stay. It would become: the cursor never lands on it. Where: `tmx-agent-dash/src/app.rs`: 255.
- "task" for a thread of Harold's own (8 uses; `tokio::task` names stay). It would become: background job. Where: `harold/src/inbound/mod.rs`: 230; `harold/src/timed_prompts.rs`: 40, 43, 541, 1007, 1063; `harold/src/tools/questions/batch.rs`: 1; `harold/src/tools/questions/batch_tests.rs`: 1111.
- "note" alone for a backlog note, in the backlog tool and its proto (168 uses; the CLI words `note` and `notes`, and the `kind` names, are the API and stay). It would become: backlog note. Where: `harold-api/proto/tools/backlog.proto`: 7 uses, lines 1–256; `harold/src/tools/backlog/fixtures/backlog.md`: 15, 25, 73, 77, 88; `harold/src/tools/backlog/fixtures/queue.md`: 10; `harold/src/tools/backlog/markdown.rs`: 33 uses, lines 7–477; `harold/src/tools/backlog/markdown_tests.rs`: 17 uses, lines 24–297; `harold/src/tools/backlog/migrations/001_backlog.sql`: 17; `harold/src/tools/backlog/schema.rs`: 9 uses, lines 1–199; `harold/src/tools/backlog/service.rs`: 16 uses, lines 14–633; `harold/src/tools/backlog/service_tests.rs`: 8 uses, lines 119–748; `harold/src/tools/backlog/store.rs`: 36 uses, lines 13–1108; `harold/src/tools/backlog/store_tests.rs`: 35 uses, lines 120–795.
- "call" for an RPC in comments (2 uses). It would become: RPC. Where: `harold-api/proto/harold.proto`: 20; `harold/src/agent/runtime.rs`: 100.

Two lines are kept as they stand:

- **`docs/explanations/architecture.md`** is not reworded here. harold-architecture-file rewrites it to the glossary's terms.
- **Older words in ADRs, kept as written.** ADRs are records of their time, so their words stay. Words the glossary says not to use that the ADRs hold:
  - `docs/adr/0001-tools-are-modules-hosted-by-harold.md:8`: "question queue" (the question tool).
  - `docs/adr/0002-tool-services-trust-local-callers.md:8`: "peer calls" (peer RPCs).

Words kept in the docs on purpose, one line each. They are plain English or a name, not the glossary's term, so the search for each finds them there:

- **"notification"** stays in these places: the page name and link text "Notification" (`docs/references/notification/`, `docs/README.md`), log lines such as `notification skipped (…)`, the setting `[notify]`, the page `docs/references/telemetry/README.md`, the dated `docs/security-audit-2026-02-27.md`, the log lines quoted in `docs/references/notification/README.md`, and the sync, routing and Telegram pages (`docs/references/inbound-message-routing/README.md`, `docs/how-tos/setup-telegram.md`, `docs/how-tos/setup.md`, `docs/explanations/channel-security.md`) where it means any tagged message Harold sends to the phone (a turn-end update, an agent message or a spoken line). The glossary has no word for that set. A later change that adds one replaces them. iOS's notification permission in `docs/references/away-app/README.md` is the operating system's word.
- **"summary"** stays in compound words that name a work summary candidate ("explicit-summary", "fallback-summary prefix", "generated-summary", "completion summary update", "summary isolation"), and where it is the text of a turn-end update, such as "Harold's summary of a finished turn" (`docs/references/notification/README.md`) and the opening of `README.md`, or the heading "Summary" of `docs/security-audit-2026-02-27.md`.
- **"call"** stays for the voice call and for "calls" of a person's tool or a hook in plain English. The `Call` RPC keeps its name.
- **"version"** stays for a release or schema version, and for an event's stream version.
- **"waiting"**, **"held"** and **"header"** stay in their ordinary senses: "waiting" for time passing, "held back", an HTTP header. The senses the glossary names are reworded, except where one of the lines below says otherwise.
- **"the server"** stays for the web app's HTTP server (`docs/references/away-app/README.md`) and an example question in the dash README.
- **"away device"** stays only in the command and skill name `send-message-to-away-device` and the log line `spoken line sent to the away device`.
- **"dashboard"** stays for HyperDX dashboards (`docs/references/telemetry/README.md`) and the file path `tasks/tmux-agent-dashboard/`.
- **"cursor"** is also the projector's saved position in the event stream, in `docs/references/operation/README.md` and `docs/references/agent-monitor/README.md` ("the state database's cursor", "cursor replay"). That is the glossary's checkpoint. These uses stay for now; the dash's cursor is the one this glossary names.
- **"session"** stays where it is the provider's own word: a Claude Code or OpenCode session (`session.id`, `session.status`, `claude --plugin-dir` runs), a graphical login session, a HyperDX per-session metric, and the heading `In flight with another session` of the backlog file. A tmux session and a pod session are named by the glossary. Where the agent itself was meant, the docs say "agent".
- **"process"** stays for an operating-system process (the agent process, a child process, a provider process). The glossary's "agent" is the pane's coding agent; "process inventory" and "incarnation" name the process-level facts.
- **"task"** stays for the work asked of an agent, and in HyperDX/telemetry text. Harold's own threads are "background job" in the docs.
- **"target"**, **"slot"**, **"queue"**, **"plugin"**, **"watcher"**, **"focus"**, **"header"**, **"actions"** and **"record"** stay in their ordinary senses: a build target, a telemetry slot, a message queue in HyperDX or Telegram, an OpenCode plugin and the `--plugin-dir` flag, the snapshot watcher command of the hook guide, the filesystem watcher, focus tracking, an HTTP header, the proto field `action`, and "record" as a verb. The senses the glossary bans are reworded.
- **"note"** and **"call"** stay in plain English ("the file's notes", "a note of the voice file", "the call to a function"), and wherever they are the `kind` or CLI words of the backlog tool or the voice call. "note" for a backlog note is reworded to "backlog note", except in the kind-qualified forms "rule note", "history note", "held note" and "left-out note".
- **"selected"** and **"selection"** stay where they mean a choice made by configuration or the agent monitor (`[notify] away_channel`, the selected adapter, process selection, the selected `HAROLD_ENV` overlay, a prompt candidate) or the endpoint the dash shows. Where the dash's cursor was meant, the docs say "cursor".
- **"monitor"** stays in `monitor-timeouts` (a spec's name) and the `MONITOR DEGRADED`, `MONITOR UNKNOWN` and `MONITOR HEALTHY` literals.
