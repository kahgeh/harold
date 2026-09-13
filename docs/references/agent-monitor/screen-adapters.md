# Provider screen adapters

Provider screen adapters classify current terminal state and recover submitted instructions when hooks supply no current task. Capture, provider parsing, and incarnation tracking are separate internal boundaries. They feed the existing [screen observation and reconciliation contract](README.md#reconciliation-contract); they do not add public API or storage fields.

## Configuration and provider support

Each named `[[agents]]` entry accepts:

| Key | Default when omitted | Accepted values and behavior |
| --- | --- | --- |
| `screen_adapter` | `"generic-v1"` | `"generic-v1"` or `"codex-v1"`. Unknown names fail startup. Selection is explicit, independent of provider ID. |
| `screen_history_lines` | `2000` | Integer from 1 through 10,000. Number of history rows requested before the visible grid. |

The returned snapshot can contain the configured history depth plus the pane's visible height. Harold does not expand the range automatically when a prompt is missing.

| Shipped provider | Adapter | Summary evidence |
| --- | --- | --- |
| Codex | Explicit `codex-v1` | Styled submitted `>` or `›` prompt blocks, including wrapped continuation rows. |
| Claude | `generic-v1` | Configured safe line prefixes; no Claude-specific styled parser or composer guarantee. |
| OpenCode | `generic-v1` | State only: no summary prefix is configured. The explicit plugin supplies submitted instructions. |
| Custom named provider | `generic-v1` unless overridden | Configured safe line prefixes. Omit prefixes if they cannot distinguish submitted input safely. |

Missing keys preserve the generic behavior of existing named configurations. A provider named `codex` does not select `codex-v1` implicitly. If a local overlay replaces the complete `[[agents]]` list, add the adapter selection to its Codex entry as well.

## Capture requests and timing

State classification captures the current visible grid:

```text
tmux capture-pane -p -S 0 -t <pane-id>
```

Prompt recovery captures a styled history tail. At the default depth the request is:

```text
tmux capture-pane -p -e -S -2000 -t <pane-id>
```

| Trigger | History behavior |
| --- | --- |
| Newly observed incarnation | Capture immediately to establish its baseline; emit no existing prompt. |
| Baseline capture failed | Retry no more than once every 30 seconds while present, including while Idle. The first success still establishes only a baseline. |
| Transition into Busy after baseline | Capture even if an older summary exists. |
| Busy-to-Idle after an inconclusive Busy recovery | Retry once for a newly submitted prompt. |
| Sustained Busy | Retry no more than once every 30 seconds. |
| Recovered prompt could not be appended | Keep the checkpoint unchanged. Retry on the next Busy-to-Idle edge; if an Idle retry also fails, retry no more than once every 30 seconds while the source append remains pending. |
| Ordinary Idle after baseline | Do not capture history. |
| Explicit summary clear | Permit recovery at the next eligible trigger; clearing does not make old history new. |

Visible-state evidence remains independent of prompt recovery. A history failure preserves existing summary evidence and uses the monitor's existing bounded health reasons. Hook grace controls conflicting state evidence; it does not suppress an independently recovered prompt.

## Submitted-input recognition

`codex-v1` checks the terminal styling around both the prompt marker and its following separator. In the Codex 0.153.4 rendering used to establish this grammar, submitted input styles the marker and separator together, then resets before the instruction. The composer resets before its separator. A nonempty unsent draft can therefore contain normal text and still be a composer; normal text alone does not prove submission.

The adapter accepts ASCII `>` and Unicode `›` submitted blocks, joins their wrapped continuation rows, and excludes composer, assistant, tool, shell, and inconclusive rows. Ambiguous or malformed styling produces no submitted candidate. Exact normalized configured idle placeholders cannot become summaries; a substantive instruction that merely contains the placeholder wording remains eligible.

`generic-v1` retains configurable `busy_all`, `idle_all`, and safe-prefix parsing without embedding Codex layout rules. Both adapters use visible-grid state clauses, with Busy winning when both full clauses match.

## Acquisition checkpoint and selection

Every full process incarnation owns an ephemeral ordered sequence of submitted-block fingerprints. The first successful history capture records that sequence without emitting a summary. It includes proven submitted placeholders as anchors but excludes composer and inconclusive blocks.

Later captures align the longest suffix of the previous sequence with the prefix of the current one. Only submitted blocks after that overlap are eligible. Harold chooses the newest substantive eligible instruction and sanitizes it to the existing 160-Unicode-scalar public bound. An additional identical submission is a new occurrence; another capture of the existing occurrence is not.

If a nonempty checkpoint has no overlap with the current tail, Harold replaces the checkpoint and adopts none of that snapshot's text. An empty established baseline permits the first later submitted block. A candidate-bearing checkpoint advances only after the source event appends successfully, so an append failure cannot consume recovered work.

Departure and replacement remove the checkpoint. Harold restart also establishes a fresh acquisition baseline while retaining durable candidates for the same incarnation. Work already present in that first successful capture is intentionally not recovered, including a task underway when Harold attaches late or a task submitted while initial captures failed.

## Summary precedence and privacy

A recovered occurrence updates screen-source recency and advances the activity revision even when its text repeats. The most recent substantive explicit or screen candidate is the source fallback, with explicit winning an equal-time tie. A generated Sonnet description can take display precedence only at the matching revision. See [AI activity summaries](activity-summaries.md#lifecycle).

Raw visible text, styled history, and unrelated terminal output are ephemeral. They are never logged, persisted, published over gRPC, or passed to the activity summarizer. The runtime checkpoint retains only fingerprints; only an eligible bounded instruction can enter the durable screen event. Rejected or missing evidence does not clear or refresh a meaningful summary.

For configuration and verification steps, see [Set up agent monitor hooks](../../how-tos/setup-agent-monitor-hooks.md). For why baselining sacrifices late-join recovery, see [the architecture explanation](../../explanations/architecture.md#recovering-a-submitted-task-from-scrollback).
