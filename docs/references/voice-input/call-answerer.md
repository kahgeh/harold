# The call answerer

In a call, a small model answers each thing you say within a few seconds, and passes real work to the chief. This page describes that small model, the **call answerer**: what it is given, what it can do, what happens when it fails, and what it logs. The call itself is described in the [voice input reference](README.md).

## One turn

You ask "Is anything building?" and stop talking. Harold hears the stop 1.2 s later, transcribes the turn in about 0.5 s, and then runs the call answerer once:

1. Harold reads the **call facts**: which agents are busy, what each pod is building, your todos, the chief's queue, and what has been said in this call.
2. Harold starts one `claude -p` run on Haiku with the rules, the call facts and your words.
3. The run returns JSON with `say`, the sentence to speak, and, only when you asked for work, `hand_over`, your request in your own words.
4. Harold makes `say` into audio with the voice worker and plays it. The line does not pass through the delivery queue, so another agent's update cannot hold it up.
5. Harold listens again.

From your last word to the first sound takes about 5 to 6 s. The first turn of a call can take a few seconds more while the voice worker loads. While a run takes longer than 3 s, the call plays its soft tick every 3 s.

You can speak again while a run goes. The new turn waits for the run, then runs with the first turn in this call's history. Two runs never overlap.

## What a run is

One `claude -p` run per turn, with these flags: `--print --safe-mode --no-session-persistence --tools "" --model <model> --output-format json --json-schema <schema> --max-budget-usd <budget> --system-prompt <rules>`. The prompt goes in on standard input.

- `--tools ""` gives it no tools, so it cannot run a command, read a file or reach the web.
- `--no-session-persistence` leaves no Claude Code transcript.
- The run uses the program `[activity_summary] cli_path` and the environment `ActivitySummaryEnvironment` (`HOME`, `PATH`, the locale and the Anthropic login variables), which Harold reads at start-up whether or not `[activity_summary] enabled` is true.
- It runs in its own process group, which Harold kills when the run is over time.

The output schema:

```json
{"type": "object", "properties": {"say": {"type": "string"}, "hand_over": {"type": "string"}}, "required": ["say"], "additionalProperties": false}
```

## Settings

```toml
[call_answerer]
enabled = true
model = "haiku"
timeout_seconds = 15
run_budget_usd = 0.02   # --max-budget-usd for each run
```

`enabled = false` sends every turn to the chief, as before the call answerer existed.

## The call facts

Harold builds them fresh for each turn and sends them, with your spoken words, to Anthropic's hosted model.

| Facts | Where Harold reads them |
|---|---|
| Each agent: its folder's name, Busy, Idle or Unknown, for how long, and its work summary cut to 200 characters | the agent monitor's snapshot |
| Each product's pod: slots busy and free, and each spec's id and state, for the specs that are drafted, waiting for approval, approved or building, and those merged or rejected in the last 24 hours | Harold's pod facts, refreshed every minute |
| Your todos: id, product, title and progress (`picks 2/2`, `steps 0/6`) | the question tool's gRPC service |
| The chief's queue: whether a note is Now or Parked, its id, and its first sentence cut to 120 characters | the backlog tool's gRPC service |
| This call: each hand-over with its state, and what you, the answerer and the chief said (the last 12 remarks, each cut to 600 characters) | the call |
| The local time | the Mac's clock |

The pods list only live specs because a prompt with every spec of every pod, merged ones and `<id>.revisions.md` files included, came to about 10k tokens for each turn. A file whose name has a second dot, such as `<id>.revisions.md`, is not a spec, since spec ids are kebab-case. With only the live specs the facts are about 3k tokens. The cost of a day's turns is on the telemetry card "Call answerer spend, by day" ([telemetry reference](../telemetry/README.md)); act when its median `input_tokens` is above 6,000, which means the pods' spec count has grown.

A todo's title is the instructions' title, or, for a question, the spec it is about. The facts never hold a backlog note's body, a queue note's full text, a todo's question text or options, a message from the thread, or a transcript of an earlier call.

The question tool and the backlog tool each have 1 s to answer. A part that does not answer is left out, the facts say "could not be read", and the answerer says so if asked.

## The rules

The answerer's system prompt tells it to:

- answer in one or two short spoken sentences, from the facts only;
- put a request for work, or a question the facts cannot answer but the chief could, in `hand_over`, and say only that it was passed to the chief;
- never say work is done or when it will be done, and never guess;
- answer "I can only help with your pods and agents." to a question outside them;
- say about a hand-over only what the facts say: it is waiting, or it was typed into the chief's pane, never that the chief received or read it.;
- answer in English, even when the person's words look like another language, because misheard speech can come out in another language (`[voice_input] language` tells Whisper to expect English; see the [voice input reference](README.md)).

When `say` is empty and `hand_over` is set, Harold says "I've passed that to the chief." itself.

## Hand-overs

1. Harold numbers the hand-over within the call (`H1`, `H2`, …).
2. Harold types `🎙 hand-over H1: <your request>` and Enter into the chief's pane, even while the chief is working. Claude Code reads a line typed while it works after its current step.
3. The typing goes through `Writer::HandOver` in `harold/src/timed_prompts.rs`. Like a spoken turn it is not held back by another writer's one-minute hold. It is the only writer that may type into a Claude Code pane that is `Busy`. It still waits when the pane list cannot be read, when the chief's folder has no agent, when the agent is not Claude Code, when its state is `Unknown`, when the input box is not showing or holds text, and for the pane lock. Once it has typed, it sets the usual one-minute hold.
4. When a check refuses, Harold tries again every 5 s, for 30 minutes, also after the call ends. If the line is still not typed, Harold sends an away message from the chief's pane, `Harold couldn't pass hand-over H1 to the chief.`, which the web app's thread shows. With no chief running there is no pane to send it from, and only the log line records it. A restart of Harold drops a hand-over not yet typed.

"Typed" is all Harold knows: tmux accepted the keys, not that the chief read the line. No receipt check follows it: agent-message-tool, which would have added one, was set aside (ADR 0009, note of 2026-10-23).

## The chief reports back

A `🎙 hand-over Hn:` line is your request from a call. The chief does it, or hands it to a pod, and then says `harold speak "H1: <result>"`. During a call, Harold plays that line between turns, never over your speech or another sound, after the phrase "From the chief.". The line also goes into the next turn's facts. After the call, `harold speak` works as it always did: a voice note or a thread line, with a push. The report goes through the delivery queue, so it can wait 4 to 38 s behind another agent's update.

## When the answerer fails

A run that cannot start, exits with an error, gives no `say`, or takes more than `timeout_seconds` ends the answerer's part in that turn. Harold kills a run still going, and the turn goes to the chief as every turn did before: typed as `🎙 <your words>`, held for a busy chief with "The chief is busy. I'll pass it on when it's free.", answered by the chief's `harold speak` or the turn-end update. The next turn tries the answerer again. When the program cannot be started (`not_started`), the call stops trying the answerer until it ends. If you spoke another turn while the failed run went, the failed turn is passed on in the background instead. When the call ends while a run goes, the run is let finish: its hand-over is typed, or, if it fails, your words are typed into the chief's pane when it is free.

## Telemetry and logs

`harold.call_turn` has `answered_by` (`answerer`, `chief` or `none`). On an answerer turn `result` is `answered` or `handed_over`, `answer_ms` and `first_sound_ms` run from the cut to the start of the answerer's audio, and no turn-end update runs.

`harold.call_answer`, a child of `harold.call_turn`, is one run:

| Attribute | Meaning |
|---|---|
| `result` | `said`, `handed_over` or `failed` |
| `reason` | on a failure: `not_started`, `exited`, `timed_out` or `no_answer` |
| `facts_ms` | time to gather the call facts |
| `model_ms` | time of the `claude -p` run |
| `cost_micros`, `input_tokens`, `output_tokens` | from the run's JSON |

A failed run also writes the WARN line `call answer failed` with `reason`, and the exit status in `error`, which stays in `harold.log`.

Hand-over lines carry `hand_over_id` and never the request: `hand-over typed` (`pane_id`, `typed_while_busy`, `wait_ms`), `hand-over waiting` (once, with `reason`, the first time a check refuses) and `hand-over not typed` (WARN, after 30 minutes).

No log line, span or event holds your words, the answerer's line, the facts or the request. The [telemetry reference](../telemetry/README.md) has the queries and dashboard cards.
