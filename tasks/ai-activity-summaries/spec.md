# Claude Activity Summaries

> Historical implementation record. The fresh-install consolidation in
> [fresh-install-state](../fresh-install-state/todo.md) supersedes schema-upgrade,
> historical candidate-repair, and deprecated configuration requirements here.
> The [monitor reference](../../docs/references/agent-monitor/README.md) defines the current contract.


## Accepted direction

The user selected Claude/Sonnet after a successful CLI comparison. Implement dashboard activity summaries in Harold using that provider, low effort, and the existing ChatGPT-independent Claude authentication. The existing phone and spoken notification paths retain their own configuration.

## Visible behavior

Given a submitted task, the existing sanitized instruction is available as the dashboard fallback. Harold generates a concise description in the background. When a completion hook supplies the agent's reply, Harold generates a description of the reported outcome, preserving uncertainty, failures, and pending tests. Generated output is at most 160 Unicode scalars.

This implementation uses existing lifecycle instructions, eligible screen fallback instructions, and completion hook replies. It does not claim live tool-by-tool progress when a provider has supplied only an instruction. Styled scrollback recovery remains its separately specified acquisition task.

## Boundaries

1. Existing ingress and screen adapters acquire evidence. No raw screen capture leaves those adapters.
2. The runtime constructs bounded ephemeral activity input and submits background work only after source events append successfully.
3. A dedicated Claude subprocess port generates a description. It cannot write events or mutate monitor state.
4. The runtime accepts only results for the current full incarnation and source activity revision, then appends a generated-summary event.
5. The pure reducer independently enforces the same revision/incarnation rule. The persisted projection keeps original candidates separate and selects a valid generated description for the existing dashboard API.

## Invariants

- Agent state and hook acknowledgements never wait for an AI inference. Generated results change only the summary candidate, never Busy/Idle/Unknown or lifecycle timestamps.
- Work is bounded: fixed configurable concurrency, per-pane latest pending input, subprocess timeout and output caps. Unchanged screen observations do not launch repeated inference. Accepted lifecycle and completion events establish new revisions even when the instruction text repeats.
- New source activity invalidates an older generated result immediately; a late result cannot restore it. Identical instructions in distinct submitted turns are distinct work.
- Empty/placeholder/failed AI output preserves the source fallback. A failed append is never recorded as successfully generated.
- Generated-summary events are projection-only and cannot trigger phone/speech notifications or recursive summarization.
- New incarnation/departure removes pending eligibility. Metadata changes and routine state corroboration do not unnecessarily invalidate a useful generated description.
- Persisted generated descriptions survive restart and full replay. Ephemeral prompts/jobs are not resumed automatically after restart; new evidence can schedule new work.
- CLI runs use stdin, an isolated temporary working directory, no tools, no hooks/customizations, no session persistence, and bounded sanitized error codes without raw subprocess output.
- Prompt and completion reply are evidence, not instructions to the summarizer. Instruction-only input must not be rendered as completed work.
- Existing protobuf and dashboard rendering contracts remain unchanged.
- Add a new checksum-tracked projection migration; do not edit old migration files or the events submodule.
- No new dependencies or dependency downloads.

## Configuration

Use a dedicated `[activity_summary]` section owned by typed settings. Defaults: disabled for backward compatibility, CLI `claude`, model `sonnet`, effort `low`, timeout 15 seconds, at most 2 simultaneous requests, at most 64 pending panes, maximum 4,000 instruction scalars and 8,000 completion-reply scalars. Validate positive bounded limits and supported effort/provider values. Provide an enabled example using the user's Claude executable.

## Acceptance

- Tests prove bounded subprocess success/failure, timeout/cancellation, and prompt/output sanitization.
- Tests prove nonblocking lifecycle handling, deduplication, concurrency limits, newer-task/replacement/departure rejection, and shutdown cleanup.
- Store tests prove migration upgrade/idempotency, generated result replay, and no delivery-outbox entry.
- A real Claude probe through the production summarizer produces an accurate bounded description. A fake-provider integration proves propagation through runtime, durable events, projection, and snapshot.
- Full offline workspace tests, formatting, warnings-denied Clippy, release build, and independent completion review pass.
- Live deployment is a distinct step after the change is reviewable; do not send external notifications as a test.
