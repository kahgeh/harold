# Provider Screen Adapters

## Status

Spec draft awaiting user review. This is a pre-implementation contract and must be checked against the final code before durable documentation is updated.

## Problem Framing

Harold can receive a work summary directly from a provider hook, but hooks may be absent, routed to another Harold instance, loaded after a pane started, or unavailable while a turn is still running. In those cases the dashboard currently shows `No work summary reported` even when tmux scrollback contains a meaningful submitted prompt.

The existing screen path has one generic extractor configured with state fragments and line prefixes. It captures only the visible grid and cannot reliably distinguish a submitted Codex input from the dim input composer. Adding provider-specific terminal rules to that generic function would mix tmux I/O, provider rendering knowledge, state classification, and summary selection.

The operator needs the most recent genuine submitted instruction available in retained tmux text. It may occur anywhere in scrollback and must not be replaced by a composer placeholder or an unsent draft.

## Goal

Introduce a clear provider screen-adapter boundary. Each adapter interprets one provider's terminal rendering and returns typed screen evidence. For Codex, summary recovery searches a configurable recent tail of tmux scrollback, recognises submitted `>` or `›` prompt blocks using preserved terminal styling, and selects the newest substantive submitted input proven to occur after Harold established the current process incarnation's baseline.

Explicit hook or plugin observations remain primary. Screen adapters provide corroboration and recovery when explicit evidence is absent.

## Non-Goals

- Reading or indexing provider transcript archives independently of a provider hook.
- Replacing `ReportAgentState`, `TurnComplete`, or the existing durable projection and reducer.
- Treating assistant responses, tool output, shell commands, status bars, or input-composer text as work summaries.
- Inferring or synthesising a summary from several user turns.
- Making OpenCode screen text authoritative when its submitted rows cannot be distinguished safely.
- Persisting raw pane captures or exposing terminal styling through the public API.

## Update Type

- Primary: architecture and design decision introducing a reusable internal module boundary.
- Secondary: operator-visible summary recovery behavior.
- Likely permanent documentation impact: update the agent-monitor architecture explanation, configuration reference, and hook setup how-to after implementation.

## Current Context

`TmuxVisibleScreen` currently invokes:

```text
tmux capture-pane -p -S 0 -t <pane-id>
```

`-S 0` starts at the top of the visible grid, so earlier scrollback is excluded. Without `-e`, tmux also removes terminal attributes. The generic extractor normalizes the resulting text and scans prefixed lines from bottom to top.

A live Codex 0.149.0 capture with `-e` demonstrated the useful distinction:

- submitted input: bold `›`, reset, then normal prompt text;
- current composer placeholder: bold `›`, reset, then dim prompt text;
- wrapped submitted text continues on following indented lines;
- completed prompt blocks can be far above the bottom of the pane.

Tmux is the only screen-text source in this task. Visible capture and bounded scrollback capture differ only in requested range and retained styling. The approved default recovery tail is 2,000 rows, with a validated maximum of 10,000.

## Behavior Contract

### Evidence precedence

1. Existing semantic-recency rules remain authoritative: the newest substantive explicit or screen candidate is effective, and explicit wins an equal-timestamp tie.
2. A substantive submitted prompt proven to occur after the current incarnation's acquisition checkpoint may therefore replace an older candidate from either source.
3. Placeholder, composer, inconclusive, or absent screen evidence does not clear or refresh an existing meaningful summary.
4. A new process incarnation begins without an inherited summary. Prompt blocks already present when that incarnation is first observed form its baseline and are not eligible for recovery.

### Capture scopes

- Frequent state classification uses the current visible grid.
- `screen_history_lines` is the number of history rows requested before the visible grid. It defaults to 2,000 per provider, accepts positive values, and rejects values above the hard maximum of 10,000. The returned snapshot may contain up to that history depth plus the pane's visible height.
- Summary recovery scans the entire returned tail, not only lines adjacent to the composer.
- A new incarnation receives an immediate baseline capture but no summary from text already present in that capture.
- If baseline capture fails, retry it no more than once every 30 seconds while the incarnation remains present, regardless of visible state, until a baseline is established. A capture that establishes the baseline never emits a summary.
- A transition into Busy triggers a bounded styled capture even when a prior screen summary exists, allowing a new submitted prompt to replace it.
- A Busy-to-Idle transition retries once when the Busy capture found no provably new prompt. While Busy, further recovery is limited to once every 30 seconds so consecutive work can still be found if a state edge was missed.
- After a baseline exists, ordinary Idle polling does not capture history. An explicit summary clear permits recovery at the next eligible trigger.

### Incarnation acquisition checkpoint

- Each incarnation owns an ephemeral ordered sequence of fingerprints for the submitted prompt blocks in its latest recovery snapshot. Raw prompt blocks and raw pane captures are not retained as the checkpoint.
- Only blocks proven submitted may participate in this sequence. Composer and inconclusive blocks are excluded; a proven submitted block that is empty or exactly equals an idle placeholder remains an anchor but cannot become the public summary.
- The first snapshot establishes the baseline and yields no candidate.
- A later snapshot aligns the longest suffix of the previous fingerprint sequence with the prefix of the current sequence. Only blocks after that overlap are new and eligible.
- Repeating the same prompt is supported because the additional occurrence appears after the aligned sequence.
- If a non-empty previous sequence has no overlap with the new bounded snapshot, provenance cannot be established. Harold replaces the checkpoint but emits no candidate from that snapshot.
- An empty established baseline permits the first later submitted block to be treated as new.
- Departure removes the checkpoint. A replacement process in the same tmux pane starts with a fresh baseline, so retained scrollback cannot be inherited as work.

### Codex summary selection

- Recognise submitted prompt blocks beginning with visible `>` or `›`.
- Use SGR styling retained by `tmux capture-pane -e` to reject dim composer text.
- Reject exact configured idle placeholders after normalisation.
- Accept a substantive prompt that merely contains placeholder words.
- Include wrapped continuation lines belonging to the same submitted prompt block.
- Search from newest to oldest across the configured retained tail and select the first valid submitted prompt block.
- Normalise controls and whitespace and apply the existing 160-Unicode-scalar public bound.

### Other providers

- This task delivers `codex-v1` and `generic-v1`.
- Claude continues through `generic-v1` in this task. A future `claude-v1` requires its own styled fixtures before Claude rendering is treated as equivalent to Codex.
- OpenCode uses `generic-v1` for state-only evidence because it has no configured safe summary prefix. Its explicit plugin remains the summary source.
- `generic-v1` preserves configurable `busy_all`, `idle_all`, and safe prefix behavior for custom agents without embedding Codex layout rules.

## Contract Surface

No public gRPC, protobuf, event-schema, storage-schema, or dashboard contract changes are required.

Internal contracts change:

| Name | Kind | Responsibility | Compatibility |
| --- | --- | --- | --- |
| `PaneCapturePort` | effect boundary | Capture visible or retained tmux text with requested terminal attributes | Replaces direct command construction inside the generic observer |
| `StyledPaneCapture` | internal value | Own captured text and capture scope without persisting it | Must not appear in public debug or event payloads |
| `ProviderScreenAdapter` | internal strategy | Classify provider state and return an ordered scan of recognised submitted prompt blocks | Selected explicitly for a configured provider |
| `PromptScan` | internal value | Carry fingerprints and optional sanitized candidates in capture order | Ephemeral; raw unrelated capture text is excluded |
| `PromptAcquisitionCheckpoint` | ephemeral runtime value | Track ordered prompt-block fingerprints for one full incarnation | Never persisted; removed on departure or replacement |
| `ScreenObservation` | existing value | Carry typed state and bounded optional fallback summary | Shape and reducer semantics remain unchanged |

Provider configuration explicitly selects a built-in adapter rather than relying on provider-ID conditionals hidden in generic parsing. The configuration keys are:

```toml
[[agents]]
id = "codex"
screen_adapter = "codex-v1"
screen_history_lines = 2000
```

Unknown adapter names, zero history limits, and limits above 10,000 must fail configuration validation at startup. Existing configurations without the new keys must continue to select the generic adapter and the 2,000-row default.

## Failure Modes

| Failure mode / trigger | Description | Expected behavior | State or persistence effect | Operator feedback | Verification |
| --- | --- | --- | --- | --- | --- |
| Tmux unavailable | `capture-pane` cannot be executed | Preserve existing monitor degradation behavior | No summary mutation | Existing degraded health reason | Port test |
| Initial baseline capture fails | The first bounded capture for an incarnation is unavailable | Retry no more than once every 30 seconds while present; the first later successful capture establishes the baseline without adopting its text | No summary mutation until later post-baseline work | Existing degraded health reason until recovery | Runtime timing test |
| Pane departed | Pane disappears between inventory and capture | Treat as departed/inconclusive | No summary mutation | Existing inventory update | Port test |
| Configured tail is large | A provider uses a large but valid scrollback bound | Search at most the configured tail without persistence or logging | Only the bounded selected summary may enter an event | None during normal operation | Large-tail test and allocation review |
| No submitted block | Scrollback has assistant output but no valid prompt | Return no summary | Preserve current meaningful summary, if any | Dashboard missing-summary copy only when none exists | Adapter test |
| Dim composer contains text | User has typed but not submitted a draft | Reject it | No summary mutation | None | Styled fixture test |
| Composer placeholder | Current input displays idle placeholder | Reject it | No summary mutation | None | Styled fixture test |
| Prompt is far above composer | Newer screen output pushed the submitted prompt upward | Find it anywhere in the configured tail | Set bounded screen candidate | Dashboard shows recovered prompt | Deep-tail fixture |
| Prompt predates configured tail | Meaningful work is older than `screen_history_lines` | Return no new summary rather than expanding without bound | Preserve current meaningful summary, if any | Missing-summary copy only when none exists | Bound test |
| New incarnation reuses pane | Retained scrollback contains prompts from the departed process | Establish a new baseline and emit nothing from pre-baseline blocks | No inherited summary | Missing-summary copy until new submitted work | Incarnation test |
| Checkpoint overlap is lost | All prior fingerprint anchors moved outside the bounded tail | Replace checkpoint without adopting ambiguous blocks | Preserve current meaningful summary, if any | None | Sliding-window test |
| Several submitted prompts | Scrollback contains multiple historical user inputs | Select newest valid submitted block only | One screen candidate | Dashboard shows most recent instruction | Adapter test |
| New turn after existing summary | A later Busy transition adds another submitted block | Recover the new block and let existing recency rules replace the prior screen candidate | Updated bounded screen candidate | Dashboard shows current work | Transition test |
| Wrapped prompt | Submitted block spans several terminal rows | Reconstruct its continuation rows in order | One normalized bounded candidate | Dashboard shows coherent instruction | Adapter test |
| Prompt mentions placeholder | Real instruction contains placeholder wording | Keep it because equality, not substring, determines rejection | Set candidate | Dashboard shows real instruction | Adapter test |
| Malformed/truncated SGR | Capture ends within a control sequence | Sanitise safely and return valid evidence only if unambiguous | No raw controls persisted | None | Parser fuzz/table test |
| Unknown adapter name | Configuration references an unavailable strategy | Reject configuration before monitor startup | No monitor state mutation | Bounded configuration error | Settings test |
| Invalid history limit | Configuration uses zero or more than 10,000 rows | Reject configuration before monitor startup | No monitor state mutation | Bounded configuration error | Settings test |
| OpenCode ambiguous row | Screen cannot distinguish composer from submitted input | Do not emit a screen summary | Plugin summary remains authoritative | Missing copy only if plugin supplied none | OpenCode adapter test |

## Test Theories

- A Codex styled fixture containing old and new completed prompt blocks, assistant output, and a dim composer returns the newest completed prompt.
- A valid Codex prompt remains recoverable near the beginning of the configured 2,000-row tail.
- A prompt older than the configured tail is not captured and does not cause an automatic unbounded expansion.
- A new incarnation baselines old prompt fingerprints without emitting them; replacement in the same pane cannot inherit them.
- Failed initial captures retry at the bounded cadence in Busy or Idle state; the first success establishes only the baseline.
- A later Busy edge recovers new work even when a prior summary exists; Busy-to-Idle retries an inconclusive edge and sustained Busy recovery is rate-limited to 30 seconds.
- Ordered overlap handles a sliding tail and repeated identical prompts; loss of all overlap is inconclusive rather than permissive.
- Wrapped continuation rows are joined, while the following assistant block is excluded.
- Plain generic fixtures retain existing state and safe-prefix behavior.
- Placeholder-only, typed-draft-only, malformed-control, and assistant-only captures return no summary.
- Capture-port tests assert exact argv for visible classification and bounded styled scrollback recovery.
- Existing reducer tests continue proving explicit/screen recency, incarnation isolation, and non-destructive inconclusive observations.

## Proposed Approach

```text
tmux pane
   |
   +-- visible capture --------------------------+
   |                                             |
   +-- bounded styled tail at recovery trigger---|
                                                 v
                                        StyledPaneCapture
                                                 |
                                      configured adapter registry
                                                 |
                            ProviderScreenAdapter
                              codex-v1 | generic-v1
                                                 |
                                  typed state + PromptScan
                                                 |
                                runtime acquisition checkpoint
                                                 |
                                         ScreenObservation
                                                 |
                                    existing reducer/projection
                                                 |
                                         WatchAgentStates
```

The effectful capture boundary returns text only to the selected adapter. Provider parsing is a pure transformation. The adapter emits typed state evidence and an ordered prompt scan; the runtime applies the incarnation checkpoint and forwards at most one bounded summary candidate. Neither can persist events or alter precedence. The existing reducer remains the only owner of durable reconciliation.

The Codex adapter parses SGR transitions only to answer whether candidate input text is dim or normal. It does not reproduce a terminal emulator or attach semantic meaning to arbitrary colours. All text still passes through the existing terminal sanitizer before comparison or persistence.

## Acceptance Criteria

- A Codex pane recovers the newest genuine post-baseline submitted prompt from anywhere in its configured tmux scrollback tail, including when an older explicit or screen summary exists.
- On late attachment to an already-running pane, Harold does not adopt a prompt present in the first successful capture. The next submitted prompt is recoverable; this is the approved safety tradeoff that prevents retained scrollback from being assigned to a replacement process.
- Text present before the current process incarnation's baseline is never adopted, including after pane reuse.
- A later submitted prompt replaces an older effective summary after a Busy transition under the existing semantic-recency rules.
- A composer placeholder or unsent dim draft is never shown as work.
- A wrapped submitted prompt is reconstructed and bounded correctly.
- Multiple historical prompts select only the most recent substantive submitted input.
- OpenCode does not gain an unsafe screen-summary heuristic.
- Existing explicit hook precedence, state classification, durable replay, and incarnation isolation do not regress.
- Raw captures and unrelated pane text are neither persisted nor logged.
- No new dependency is introduced unless separately audited and approved.
- Real tmux testing demonstrates a completed Codex prompt above substantial subsequent output, a non-empty composer draft, and correct dashboard recovery.

## Verification Plan

1. Add provider-adapter unit fixtures through RED/GREEN TDD.
2. Add exact capture-port argv and failure mapping tests, including `tmux capture-pane -p -e -S -2000 -t <pane>` for the default recovery request.
3. Run focused agent screen/runtime/reducer suites.
4. Run full offline workspace tests, formatting, warnings-denied Clippy, release build, and Git checks.
5. Obtain independent Rust completion review.
6. In disposable tmux panes, submit several distinctive Codex prompts, add enough output to move them well above the composer while retaining them inside the configured tail, type an unsent composer draft, and verify the dashboard shows the newest submitted prompt rather than the draft.
7. Record exact live commands, pane identities, capture dimensions, and observed dashboard revisions in the screen-testing ledger.

## Documentation Notes

After implementation, reconcile this spec into:

- `docs/explanations/architecture.md` for the adapter and effect/reducer boundaries;
- `docs/references/agent-monitor/README.md` for adapter configuration and provider support;
- `docs/how-tos/setup-agent-monitor-hooks.md` for fallback and live verification guidance.

The styled Codex fixtures and rejected alternatives are test material, not permanent operator documentation.

## Risks And Approved Assumptions

- Harold searches at most the configured recent tail. The approved default is 2,000 rows and the hard maximum is 10,000. Work older than that bound may remain without a recovered screen summary.
- Safe incarnation isolation intentionally sacrifices recovery of text present in the first successful capture. This includes current work when Harold first joins an already-running pane and work submitted before a failed initial baseline capture is successfully retried.
- `screen_history_lines` bounds history depth before the visible grid; total stdout rows can additionally include the current pane height.
- `tmux pipe-pane` is not used as a tail cursor: it streams raw cursor movement and redraw traffic rather than a stable rendered grid.
- Terminal styling is provider-version-sensitive. Versioned built-in adapter names and real-pane fixtures make changes reviewable without silently weakening the generic path.
- A future incremental capture cursor may improve performance, but it is not required for the first adapter boundary.
- The approved summary semantic remains the most recent substantive submitted user instruction; adapters may scan backward to skip invalid candidates but must not combine turns.
