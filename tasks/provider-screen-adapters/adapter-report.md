# Capture and adapter implementation evidence

Owned slice: `screen.rs`, `screen_capture.rs`, `screen_codex.rs`, `screen_tests.rs`, provider settings/default/template and non-runtime test constructors. Existing activity-summary edits preserved; no dependency or lockfile changes.

## Changes

- `PaneCapturePort` owns visible or styled bounded history capture. Default recovery argv: `tmux capture-pane -p -e -S -2000 -t <literal pane>`.
- Captures are UTF-8 checked and limited to 4 MiB and two seconds. Nonblocking Unix socket stdout avoids a reader thread surviving timeout; error paths kill and wait for the owned child. Raw output is never written to a temporary file, logged, or given Debug.
- Typed `ScreenAdapter` selects `generic-v1` or `codex-v1`; defaults remain generic/2000, shipped Codex config explicitly selects codex-v1. Unknown enum values and history limits outside 1..=10000 fail loading/validation.
- Production visible observation returns state only. Recovery separately returns ordered fingerprint/candidate blocks so runtime must apply its incarnation baseline.
- Generic adapter retains configured safe prefixes; OpenCode with no prefixes remains state only.
- Codex grammar uses current 0.153.4 live evidence. Submitted input styles the marker AND separator before reset; a normal-text unsent composer resets before the separator. Dim candidate text, unstyled/ambiguous rows, malformed controls, composer and assistant rows are excluded. Wrapped continuation rows join before sanitization and the 160-scalar public bound. Full normalized blocks are fingerprinted, so distinct long prompts do not alias at the display cap.

## RED/GREEN

Observed failures before implementation:

1. `capture_uses_only_the_current_grid_with_literal_pane_argument`: visible observation incorrectly returned `Review tests` rather than None.
2. `screen_history_rejects_zero_and_excessive_limits`: invalid new limits were silently ignored.
3. `history_capture_preserves_styles_and_uses_only_the_configured_tail`: default scan returned zero blocks rather than one.
4. `codex_current_styled_submission_excludes_normal_text_unsent_composer`: empty adapter returned zero blocks rather than submitted task.
5. `malformed_wrapped_row_cannot_emit_a_truncated_submission`: malformed continuation incorrectly emitted first row; now invalidates pending block.
6. `placeholder_matching_is_exact_before_public_truncation`: truncated candidate incorrectly equaled placeholder despite substantive suffix; comparison now uses full normalized input.

GREEN: `cargo test --offline -p harold screen -- --nocapture` passed 43 tests before the final opt-in tmux test was added. `cargo clippy --offline -p harold --all-targets -- -D warnings` passed. Current test count can increase while runtime owner adds tests.

## Additional coverage

Exact default/custom capture argv, literal pane arguments, invalid direct limits, command failure/oversize output, successful child drainage and timeout, generic provider-ID independence/OpenCode state-only, exact placeholder equality, repeated fingerprints, wrapped prompt/assistant separation, C1 CSI, RGB color values distinct from SGR attributes, malformed/truncated controls, and a valid prompt above 1990 following output lines.

Opt-in real production capture-port test creates a uniquely named isolated tmux server, retains 4000 rows, renders a synthetic submitted prompt followed by 2050 output rows, and proves the 2000-row scan excludes it while the 4000-row scan recovers it. The server is removed by an owned Drop guard. This is a real tmux bound test, not real Codex lifecycle acceptance (parent records that separately).

## Scoped review correction and final slice checks

The scoped completion reviewer found a malformed SGR state leak (`ESC[1?m` followed by an unstyled prompt-shaped row). The new `malformed_sgr_cannot_lend_submission_style_to_a_later_row` test failed before the fix. Malformed controls now clear style trust; later rows stay inconclusive until an explicit reset. SGR updates are transactional, preventing unknown trailing parameters from partially changing attributes.

Final slice checks: 22 screen tests passed, one real-tmux test intentionally ignored in the ordinary suite; warnings-denied Clippy passed. The ignored `real_tmux_capture_excludes_prompt_older_than_configured_history_tail` was run explicitly and passed (one test, 0.09 seconds). Full workspace and real Codex/dashboard acceptance belong to the parent integration gate.
