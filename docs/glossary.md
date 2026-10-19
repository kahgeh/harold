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

(Filled in below.)
