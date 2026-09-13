# AI activity summaries

Harold can generate dashboard descriptions with Claude CLI. A submitted instruction supplies the task description; a completion hook can also supply the agent's reply so the description includes its reported outcome. The existing `work_summary` field carries the result, bounded to 160 Unicode scalars.

## Configuration

The dedicated `[activity_summary]` section controls dashboard generation. It is independent of `[ai]`, which continues to configure notification summaries and inbound routing.

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Start background generation for new activity evidence. |
| `cli_path` | `"claude"` | Non-empty executable path, at most 4,096 bytes, without controls. Relative names use the process search path. |
| `model` | `"sonnet"` | Non-empty model name or alias, at most 128 characters. |
| `effort` | `"low"` | `low`, `medium`, `high`, `xhigh`, or `max`. |
| `timeout_ms` | `15000` | 1–120,000 ms; subprocess deadline including input/output. |
| `max_concurrent` | `2` | 1–16 simultaneous generation jobs. |
| `max_pending` | `64` | 1–4,096 pending panes. Newer pending activity for a pane replaces its older pending input. |
| `max_instruction_chars` | `4000` | 1–32,000 instruction Unicode scalars supplied for generation. |
| `max_reply_chars` | `8000` | 1–32,000 completion-reply Unicode scalars supplied for generation. |
| `max_output_bytes` | `65536` | 1–1,048,576 captured Claude response-envelope bytes. |

Example local override:

```toml
[activity_summary]
enabled = true
cli_path = "/Users/kahgeh/.local/bin/claude"
model = "sonnet"
effort = "low"
```

Use Harold's normal configuration loading: the binary-adjacent `config` directory, `HAROLD_CONFIG_DIR`, the selected `HAROLD_ENV` overlay, or `HAROLD__ACTIVITY_SUMMARY__…` environment overrides. The Claude installation must support the configured CLI flags and have working authentication in Harold's process environment. The CLI invocation was probed with Claude Code 2.1.243.

## Evidence and output

- Lifecycle hooks supply the submitted instruction available in their `work_summary` input. Hooks may already limit this to 160 scalars; a larger configured input cap cannot restore omitted context.
- Screen fallback supplies a sanitized instruction of at most 160 scalars. The [provider screen adapter](screen-adapters.md) can recover it from bounded tmux history after proving that it is a new submission for the current incarnation. Raw history and unrelated terminal output are never supplied to Claude.
- Completion hooks supply `last_user_prompt` and `assistant_message`. The reply can describe changes, failures, tests, and unresolved work.
- Instruction-only input asks for a task description. It is insufficient evidence to claim that work completed or tests passed.
- The generated sentence is model output, grounded in supplied evidence; it is not an independent verification of the agent's claims.

The CLI receives bounded data through stdin in an isolated temporary directory. Safe mode disables customizations and hooks, tools are disabled, session persistence is disabled, and raw CLI errors/output are not written to application logs.

## Lifecycle

Source observations are appended before generation is scheduled. Agent-state updates and hook acknowledgements do not wait for Claude. Until a valid generated result is projected, the existing source instruction is available as fallback. Coalesced snapshot delivery does not guarantee that every watcher sees an intermediate fallback snapshot.

Every result carries the full agent-process incarnation and the source activity revision. Both runtime and reducer reject results that no longer match. A new task invalidates the previous generated description; pane replacement starts with no inherited description. A newly recovered submitted occurrence advances that revision even when its instruction repeats the previous task. Routine metadata refreshes and repeated captures of the same occurrence do not request fresh inference.

Successful descriptions are recorded as projection-only events. They do not invoke notification delivery or trigger another summary request. Original explicit and screen candidates remain available for fallback. A generated description changes only the summary candidate; it never changes Busy/Idle/Unknown or lifecycle timestamps.

The generated candidate survives restart and event replay. Pending generation jobs are ephemeral and are not automatically resumed after restart. Disabling generation stops new jobs; existing stored descriptions remain subject to the normal source-revision rules.

## Failure behavior

Authentication failure, subprocess failure, timeout, empty/invalid output, and output-limit violations leave the source fallback intact. Bounded queue capacity limits background work; the monitor remains responsive when generation cannot keep up.

The current hook RPC identifies a pane, not a task ID. Revision checks reject stale asynchronous summarizer results; they cannot establish that a delayed incoming completion hook itself belongs to an older task.

See the [monitor reference](README.md) for the underlying event, projection, and public API contracts.
