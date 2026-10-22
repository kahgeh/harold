# 9. A call is answered by a small model, and real work goes to the chief

Date: 2026-10-23
Status: Accepted (fast-call-voice)

## Context

The chief is a large model, often in the middle of a turn, so a call's answer took about 30 s and did not come at all while the chief was busy. Most questions on a call are about state Harold already holds. Harold already runs Claude Code in safe mode for activity summaries and for the watch tool (ADR 0005).

## Decision

- Each turn in a call is answered by one `claude -p` run of a small model (Haiku), in safe mode with no tools and no saved transcript, from call facts Harold builds for that turn.
- A request for work becomes a hand-over that Harold types into the chief's pane. The run itself never reaches the chief or any other tool.
- The pane rule: a hand-over is the one writer that may type into a Busy Claude Code pane. It is not held back by another writer's hold, as a spoken turn is not. It still needs the input box showing and empty, a known state, and one Claude Code agent in the folder. Once typed, it holds the pane against the other writers for a minute. Timed prompts, alerts and tools still type only into an Idle pane.
- The hand-over's typing and retry are a stopgap. When agent-message-tool lands, hand-overs go through the message tool with its receipt check, and `Writer::HandOver` and the retry loop are removed.
- When a run fails or times out, the turn goes to the chief as before.
- The facts sent are the wider set, chosen by the user (Choice C(b)): agent folder names, states and work summaries, pod spec ids and states, this call's hand-overs and chief lines, todo ids, titles and progress, and the first sentence of each queue note. The answerer is not a tool, so ADR 0005 does not cover it; this ADR does.

## Consequences

- Answers take about 5 to 6 s and cost about $0.0003 a turn.
- The answerer knows only the facts Harold gives it, and can say a hand-over was typed, never that the chief read it.
- The chief reports back with `harold speak`.
- Your spoken words and the call facts go to Anthropic's hosted model on every turn of every call: agent and pod facts, this call's hand-overs and chief lines, todo ids, titles and progress, and the first sentence of each queue note (the user's choice, C(b)). Note bodies and todo question text and options are never sent.
- Harold gains a second path that types into a busy pane, until the message tool replaces it.
