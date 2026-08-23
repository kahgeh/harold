# Provider Screen Adapter Design

## Decision

Harold will separate tmux capture from provider interpretation. A common capture port supplies visible text or a bounded styled scrollback tail. A configured provider adapter converts that ephemeral capture into typed state evidence or an ordered `PromptScan`. The runtime applies the incarnation checkpoint and constructs the existing `ScreenObservation`; durable state remains owned by the reducer and event stream.

This avoids two forms of coupling:

- tmux commands do not need to know Codex, Claude, or OpenCode layouts;
- the reducer does not need to know terminal prefixes, colours, boxes, or composer behavior.

## Inputs And Ownership

| Input | Owner | Lifetime | May be persisted? |
| --- | --- | --- | --- |
| Tmux visible capture | `PaneCapturePort` | One classification attempt | No |
| Tmux bounded styled scrollback tail | `PaneCapturePort` | One recovery attempt | No |
| Provider settings and adapter name | Settings loader | Process lifetime | Configuration only |
| Typed state and ordered prompt scan | `ProviderScreenAdapter` output | One capture interpretation | No; the runtime forwards at most one bounded candidate through existing agent events |
| Explicit lifecycle or completion summary | Existing hook RPC ingress | Existing durable lifecycle | Yes, under current contracts |

## Component Boundaries

### `PaneCapturePort`

The only module that invokes tmux. It accepts a pane identity and capture request:

```rust
enum CaptureScope {
    Visible,
    RecentHistory { lines: u16 },
}

struct CaptureRequest {
    scope: CaptureScope,
    preserve_styles: bool,
}
```

Production maps these requests to exact tmux arguments. Visible state classification retains the small current-grid capture. The default recovery request is exactly equivalent to:

```text
tmux capture-pane -p -e -S -2000 -t <pane-id>
```

This is a point-in-time stdout snapshot, not a stream or cursor. `screen_history_lines` bounds history depth before the visible grid, so the returned text can contain those 2,000 history rows plus the current pane height. Validation rejects zero or values above 10,000.

The capture result is intentionally absent from public `Debug`, logs, events, and API responses.

### `ProviderScreenAdapter`

A pure provider-specific strategy:

```rust
trait ProviderScreenAdapter {
    fn classify_visible(&self, capture: &StyledPaneCapture) -> Option<ObservedAgentState>;

    fn scan_prompts(&self, capture: &StyledPaneCapture) -> PromptScan;
}
```

`PromptScan` contains recognised prompt blocks in capture order, with a non-reversible fingerprint and an optional sanitized, bounded candidate for each block. The exact Rust types may change during implementation planning, but these responsibilities must remain separate. An adapter cannot invoke tmux, append an event, mutate a projection, or decide source precedence.

### Adapter registry

Settings resolve `screen_adapter` once at startup. Built-in names are versioned when they encode a provider rendering contract:

- `codex-v1`
- `generic-v1`

This task delivers those two adapters. Claude and OpenCode continue through `generic-v1`; OpenCode remains state-only because it has no safe configured summary prefix. Missing keys select `generic-v1` for backward compatibility. Unknown names fail validation. Provider IDs do not trigger hidden `if provider == ...` parsing branches.

### Runtime orchestration

The monitor performs frequent visible classification. A new incarnation immediately captures a bounded styled tail to establish an acquisition checkpoint but emits no prompt already present. If that capture fails, the monitor retries no more than once every 30 seconds while the incarnation remains present, even when Idle, until the first successful capture establishes the baseline. A transition into Busy after a baseline exists captures again even when a prior screen summary exists. Busy-to-Idle retries once if the Busy scan found no provably new prompt, and sustained Busy recovery is rate-limited to once every 30 seconds. Ordinary Idle polling does not capture history after the baseline exists.

After checkpoint alignment, a successful bounded candidate enters the existing screen-observation flow. An inconclusive result leaves current state unchanged. Capture failures retain existing monitor-health behavior and the same recovery rate limit. If work is submitted before a failed initial capture can establish the baseline, that work is intentionally included in the eventual baseline and not adopted; safety across pane reuse takes precedence over late-join recovery.

### `PromptAcquisitionCheckpoint`

The runtime owns one ephemeral checkpoint per full incarnation identity. The adapter returns the ordered scan of recognised submitted prompt blocks. Raw captures and raw prompt strings are not retained in the checkpoint.

The first capture stores the fingerprint sequence and emits no summary. Later captures align the longest suffix of the previous sequence with the prefix of the current sequence. Blocks after that overlap are newly observed. This allows an identical prompt to be submitted twice because the second occurrence extends the aligned sequence.

If a non-empty previous sequence has no overlap with the current bounded tail, the window has lost its provenance anchor. The runtime replaces the checkpoint and emits no candidate from that capture. Departure deletes the checkpoint; a replacement process in the same pane cannot inherit prior prompt blocks.

## Codex `codex-v1`

### Observable grammar

The adapter uses only evidence demonstrated in a styled tmux capture:

```text
submitted: <bold>› <reset><normal prompt text>
composer:  <bold>›<reset> <dim placeholder or draft>
wrapped:   two-space-indented continuation rows until the prompt block ends
```

Both `>` and `›` are accepted visible prefixes. The adapter treats styling as evidence about the candidate text, not the prefix glyph alone.

### Selection algorithm

1. Parse capture rows into visible text plus minimal SGR state spans.
2. Locate possible prompt-block starts anywhere in the configured retained tail and associate wrapped continuation rows until the next blank/block boundary.
3. Use styling and block context to classify each block as proven submitted, composer, or inconclusive.
4. Exclude composer and inconclusive blocks entirely from `PromptScan`; they are neither candidates nor fingerprint anchors.
5. Fingerprint the proven submitted blocks in capture order.
6. Mark a proven submitted block summary-ineligible when its text normalizes empty or exactly equals a configured idle placeholder. It remains a fingerprint anchor because its submission is proven.
7. Sanitize and normalize eligible candidate text and apply the existing scalar bound.

The runtime, not the adapter, aligns this ordered scan with the incarnation checkpoint. It considers only newly observed blocks, traverses those newest to oldest, and forwards the first eligible candidate into the existing observation flow.

The adapter never selects or fingerprints assistant bullets, tool invocations, shell prompts, status bars, token-usage text, or composer content because none are proven submitted-prompt blocks.

## Other Adapters

`generic-v1` retains configured marker and safe-prefix behavior. It may use plain text when styles are unavailable, but it cannot claim provider-specific composer rejection. Claude remains generic until a later task supplies provider-specific fixtures. OpenCode's explicit plugin remains responsible for prompts because its current screen rendering does not safely distinguish a submitted row from the composer.

## Data Flow

```text
inventory tick
   |
   v
capture visible grid ------ failure ------> monitor degradation/inconclusive
   |
   v
adapter.classify_visible
   |
   +-- no recovery trigger --> emit state evidence only
   |
   `-- new incarnation / Busy edge / bounded retry
          |
          v
     capture bounded styled tail
          |
          v
     adapter.scan_prompts
          |
          v
     acquisition checkpoint
          |
          v
     ScreenObservation { state, fallback_summary }
          |
          v
     existing append -> reducer -> snapshot
```

Explicit hook observations continue through their current ingress and compete by existing semantic recency. A strictly newer substantive screen candidate replaces an older explicit candidate; explicit wins only an equal-timestamp tie. The adapter boundary does not create another source-precedence policy.

## Security And Reliability

- Never log or persist raw captures.
- Sanitize complete terminal control sequences before a selected candidate leaves the adapter.
- Keep unrelated captured text out of public error and `Debug` values.
- Treat malformed styling as inconclusive when submission cannot be proven.
- Preserve exact placeholder comparison; substring rejection would discard legitimate work.
- Baseline every full incarnation identity and require ordered fingerprint overlap before accepting later scrollback text.
- Keep history capture conditional so its configured bound does not multiply across every pane and every polling tick.

## Rejected Designs

### Add Codex branches to the generic extractor

Rejected because provider layout rules would accumulate inside shared capture and parsing code, making custom-agent behavior fragile.

### Read provider transcript archives globally

Rejected for this task because pane-to-transcript correlation, retention, permissions, and provider-specific formats create a separate subsystem. Provider hooks already receive authoritative transcript paths.

### Search all retained tmux history

Rejected because tmux history limits may be very large and repeated whole-history snapshots would create unpredictable work. Recovery searches a configurable recent tail: 2,000 rows by default and never more than 10,000.

### Stream with `tmux pipe-pane`

Rejected because it exposes raw terminal writes, cursor movement, and redraw sequences rather than a stable rendered grid. It also introduces a long-lived pipe lifecycle and does not provide the reliable scrollback cursor required here.

### Treat every `>` or `›` line as submitted work

Rejected because the current composer and placeholders use the same visible prefix. Styling and block context are required.

### Run history-tail capture on every poll

Rejected because state classification needs only the visible grid and frequent history capture scales poorly with pane count. Recovery runs on new-incarnation baselining and its bounded failure retries, Busy transitions, an inconclusive Busy-to-Idle retry, and no more than once per 30 seconds during sustained Busy state.

## Implementation Planning Notes

Implementation must use RED/GREEN TDD and should be divided into independently reviewable slices:

1. capture request/value boundary and exact tmux argv;
2. adapter registry and backward-compatible configuration validation;
3. Codex styled prompt-block parser;
4. runtime recovery trigger and reducer-preserving integration;
5. provider fixtures, full gates, live tmux acceptance, and documentation migration.

No implementation begins until this design and the companion `spec.md` are reviewed and approved.
