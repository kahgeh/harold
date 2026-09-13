# AI Activity Summaries

## Request

Improve dashboard readability with an AI summary of the agent's activity. After comparing Codex/Luna and Claude/Sonnet, the user selected Claude/Sonnet. Use low effort and preserve separate notification and routing configuration.

## Investigation and design checkpoint

- [x] Verify the current Harold checkout and trace actual summary callers.
- [x] Inspect deployed non-secret model settings and identify the running Harold working directory.
- [x] Verify installed Codex CLI flags, sign-in method, and current official model/pricing guidance.
- [x] Verify a synthetic activity summary through Codex CLI with GPT-5.6 Luna and low reasoning effort.
- [x] Scope the approved change to dashboard summaries, following the original dashboard request; spoken and phone summaries retain existing configuration.
- [x] Define bounded hook/screen instruction and completion-reply input, separate generated candidates, revision checks, and background generation in `spec.md`. User selected Claude/Sonnet after the proposed background design and live comparison.
- [x] Keep richer scrollback acquisition separate: this slice uses existing sanitized screen instructions and explicit hooks, so raw-capture boundaries remain unchanged.
- [x] Save `implementation-plan.md`, present the implementation scope, and open branch `feat/claude-activity-summaries` before source edits.
- [x] Implement approved scope, verify focused and end-to-end behavior, and obtain completion reviewer approval.
- [x] Install the reviewed signed release, enable local Claude/Sonnet configuration, and verify the exact live process/config/store/listener.

## Verified findings — 2026-09-09

- Harold checkout is clean on `main` at investigation start. The canonical dashboard is inside this workspace.
- Running Harold PID 73303 has working directory `/Users/kahgeh/bin/harold`.
- Deployed `config/local.toml` selects `Qwen/Qwen3-32B-MLX-8bit` and `/Users/kahgeh/Dev/xn/mlx-lm` for local generation; this is configuration evidence, not a fresh successful local-model inference.
- `harold/src/outbound/tts.rs` calls local MLX generation to produce a short spoken completion description from the submitted instruction.
- `harold/src/channels/mod.rs` summarizes completion replies for phone notifications with Claude-specific arguments and a hardcoded Sonnet model.
- `harold/src/inbound/mod.rs` shares `ai.cli_path` for Claude-based inbound routing. Replacing that path with Codex alone would break these callers.
- Dashboard `work_summary` currently sanitizes/truncates submitted instructions to 160 Unicode scalars. It does not invoke an AI model.
- Styled bounded scrollback adapters remain design-only. The production screen path captures the visible grid. Missing activity context cannot be recovered by changing model settings alone.
- Installed Codex is `/opt/homebrew/bin/codex`, version 0.153.4, signed in using ChatGPT. Its local model cache lists `gpt-5.6-luna`.
- Official model guidance recommends Luna for structured summaries and low reasoning for well-scoped work. Current published Codex token rates list Luna at 5 input / 0.5 cached-input / 30 output credits per million tokens, the lowest listed text-model credit rates. This uses ChatGPT allowance/credit accounting, not a quoted API dollar price.

## Implemented design

Activity acquisition -> bounded typed activity evidence -> asynchronous summary request -> validated generated result -> durable event/reducer/projection -> dashboard.

- Configure a dedicated summarization provider, executable, model, and reasoning effort. Selected provider is Claude CLI with `sonnet` and `low`; leave inbound routing independently configured.
- Give the model the existing eligible instruction and, when supplied, the completion-hook reply. An instruction alone cannot establish which actions completed or whether tests passed.
- Keep raw terminal captures inside acquisition. This slice accepts existing sanitized screen instructions; improved submitted-history/composer discrimination remains the separate screen-adapter task.
- Keep generated descriptions distinguishable internally from source instructions. Define reducer precedence and fallback explicitly; this changes the earlier prompt-only summary contract.
- Update agent state immediately. Generate descriptions in a separate bounded queue, deduplicate unchanged evidence, limit refresh frequency, and retain an existing useful description or sanitized instruction when generation fails.
- Associate requests/results with the full pane/process incarnation and activity revision. Reject results after departure, replacement, or a newer task.
- Invoke Claude with stdin, an isolated working directory, no session persistence, no tools, and safe mode to disable customizations/hooks. Verify effective isolation before production use.
- Validate generated text against the dashboard's 160-scalar bound; do not turn unknown outcomes into success claims.
- Cover subprocess timeout/cancellation/output bounds, failure fallback, duplicate suppression, stale-result rejection, and real dashboard delivery in verification.

## Sources

- https://learn.chatgpt.com/docs/models
- https://learn.chatgpt.com/docs/pricing
- https://learn.chatgpt.com/docs/non-interactive-mode

## Review

Implementation complete on `feat/claude-activity-summaries` (base `174fba5`), initially deployed with changes left uncommitted; publication is tracked below. Baseline: 148 Harold tests passed before source edits. Final workspace gate: **347 passed, 0 failed, 1 ignored** across 12 test binaries; Harold contributes 181 passing tests. The ignored real-provider probe passed separately. Formatting, warnings-denied workspace Clippy, offline release build, and diff checks passed.

The independent completion reviewer rejected two substantive findings before acceptance: reply-only completion was skipped, and forced monitor shutdown could bypass child cleanup. Both were fixed and re-reviewed. The final verdict was **APPROVED — ready for deployment**, with no remaining findings. The port also received scoped approval. A subsequent full-suite fixture startup failure was traced to the macOS Python launcher; the runtime cleanup fixture now uses shell builtins and `exec /bin/sleep` with its original two-second readiness bound. The final parallel workspace run passed.

Live acceptance [ST-032](../tmux-agent-dashboard/screen-testing.md#st-032--claudesonnet-generated-descriptions-and-durable-replay) exercised real release Harold, authenticated Sonnet, gRPC, and the actual dashboard with synthetic input. Busy/completion acknowledgements took 174/170 ms; generated descriptions appeared after 3.515/3.502 s including polling overhead. Pending tests were preserved, reply-only outcomes worked, and the exact summary survived a same-store restart. This is not acceptance for the separate real three-provider acquisition gates.

Generation uses existing submitted instructions and explicit completion replies. Richer scrollback acquisition remains separate. Generated descriptions are bounded, revision checked, durable, and projection-only; they do not trigger notifications or change agent lifecycle state. Failures retain source fallback. No dependency, lockfile, events submodule, protobuf, or dashboard production-source changes were made.

- `cargo audit --no-fetch` exited 0 using the locally cached advisory database. It reported the existing allowed `paste 1.0.15` unmaintained warning through Turso. No dependency or lockfile change is part of this task; this is not a freshly downloaded advisory database.
- Production provider probe: `cargo test --offline -p harold activity_summary_real_claude_probe -- --ignored --nocapture` passed on 2026-09-09. The actual Rust Claude port returned "Capped widget retry loop at three attempts and added a regression test; tests not yet run." in 2,666 ms. This tests the production subprocess boundary with synthetic evidence; full runtime/gRPC live delivery is recorded separately when run.

Synthetic CLI probe passed: exit 0 in 5.16 seconds using Luna/low, stdin, `--ignore-user-config`, `--ephemeral`, a temporary working directory, read-only sandbox, disabled project document loading, and a task-specific developer instruction. Output: "Agent fixed the expired-session retry loop and is running regression tests; results are pending." This proves current account/model invocation and one useful summary; it does not prove production integration, effective tool/hook isolation, general quality, or latency under load.

### Requested CLI speed comparison

On 2026-09-09, attempted a comparison with the same synthetic input and summary instruction, fresh temporary working directories, and low reasoning/effort. Codex used the preceding isolation flags. Claude Code 2.1.243 used `--print --safe-mode --no-session-persistence --tools "" --model sonnet --effort low --output-format json --system-prompt <instruction>`, with `CLAUDECODE` removed from the child environment.

- Codex/Luna completed three runs in 4.36, 3.95, and 5.07 seconds (median 4.36 seconds, including process startup). Each produced a bounded sentence retaining the pending-test outcome.
- Claude/Sonnet failed before inference: "Failed to authenticate: OAuth session expired and could not be refreshed". Reported API time was zero; the 0.90-second failure is not an inference speed measurement. Further Claude trials were skipped.
- No relative speed conclusion is supported until Claude authentication is restored. These are three repetitions of one short synthetic input, not a general benchmark or production latency guarantee.

User-requested retry on 2026-09-09 succeeded for both providers with the same commands and input:

- Claude/Sonnet: 4.78, 2.43, 2.57 seconds; median 2.57 seconds. CLI usage metadata included `claude-sonnet-5` and `claude-haiku-4-5-20251001`; this measures the whole configured CLI path, not isolated model inference.
- Codex/Luna: 4.61, 5.23, 7.16 seconds; median 5.23 seconds.
- All six runs exited successfully and described the fix with regression tests still pending. Claude's median wall time was about 51% lower in this small repeated-input comparison. Authentication is now working; this supersedes the earlier authentication blocker for this CLI probe only, not the separate dashboard/provider live acceptance gates.


## Local deployment — 2026-09-09

- Replaced only the exact old deploy-owned daemon PID 73303 after graceful SIGTERM. The release was signed and verified before replacement. The dashboard executable required no change.
- New daemon PID **42929** runs `/Users/kahgeh/bin/harold/harold` with cwd `/Users/kahgeh/bin/harold`, explicit `HAROLD_CONFIG_DIR=/Users/kahgeh/bin/harold/config`, and `HAROLD_ENV=local`. `lsof` confirmed ownership of **127.0.0.1:50060** and the existing `/Users/kahgeh/bin/harold/data/events/harold-state.db`.
- Installed binary SHA-256: `44ba2e5a504712acd2e2091d3c648e6fc6b07a7c3934ae937a80fe81239a5c40`. Installed code signature verified successfully.
- Enabled `[activity_summary]` in both ignored repository-local and deployed local configuration: `enabled=true`, `cli_path="/Users/kahgeh/.local/bin/claude"`, `model="sonnet"`, `effort="low"`. Verified all other local configuration text is unchanged, including notification/routing settings.
- Preserved binary and configuration backups in `/Users/kahgeh/bin/harold/backups/activity-summary-20260909-233823`. Updated deployed defaults/template to the reviewed version. Deployment report: `/tmp/harold-activity-deployment.json`.
- The deployed WatchAgentStates stream returned revision 3437 with 11 panes, 8 summaries, and healthy inventory/screen status. No errors or warnings appeared after this restart at verification time. Existing rows are not retroactively resummarized merely by enabling the setting; subsequent eligible activity queues generation.
- No synthetic completion was submitted to production. Real-provider end-to-end output evidence is the isolated ST-032 run; production verification proves activation and the correct process/config/store/stream path.

## Publication — 2026-09-13

User requested committing, pushing, and opening a PR for the combined Sonnet summaries and provider screen adapters.

- [x] Inspect the full tracked/untracked scope and format the workspace.
- [x] Fetch upstream and verify `origin/main` has not diverged from the feature branch base; verify the events submodule is clean.
- [x] Rerun workspace tests (381 passed, 0 failed, 2 opt-in ignored), Clippy, and release build; final publication review confirms expected scope and artifact exclusions.
- [x] Commit the combined reviewed feature as `66ea54f`, push `feat/claude-activity-summaries`, and open [PR #1](https://github.com/kahgeh/harold/pull/1) against `main`.
- [x] Verify the remote branch commit matches the feature commit and PR #1 is open, ready for review, and mergeable. GitHub reports no checks for this PR; local verification and independent review are recorded above.
