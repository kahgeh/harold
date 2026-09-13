# Provider Screen Adapter Implementation

User authorization: implement the saved adapters and pass the parser, recovery/identity, and real isolated Codex tests discussed in this thread. Work in the current Harold checkout and preserve the uncommitted, reviewed Sonnet feature. No dependencies or public protobuf changes are planned. All Cargo commands run offline.

## 1. Establish evidence and contracts

- [x] Read current design/spec, lessons, screen facade, settings, runtime and reducer integration points.
- [x] Record user approval in the task and reconcile source fallback with generated-summary precedence.
- [x] Establish baseline package tests and capture current real Codex submitted/composer styling in a disposable pane before finalizing its grammar. Baseline:180 passed,1 failed,1 ignored; failure was preexisting subprocess readiness timeout (not cleanup), recorded for correction. Live Codex0.153.4 normal-text draft required prefix/reset discrimination; spec/design refined before parser implementation.

The runtime-facing facade retains `VisibleScreenPort::observe(pane, provider) -> Result<ScreenObservation, ScreenError>` for state evidence and adds `scan_prompts(pane, provider) -> Result<PromptScan, ScreenError>`. The latter exposes `PromptScan { blocks: Vec<PromptBlock> }`, where each block carries `[u8; 32]` fingerprint and `Option<String>` bounded candidate. Raw captures stay within the screen module and never implement unredacted Debug.

## 2. Capture, adapter and settings slice

Owner: screen_adapters worker. Files: `screen.rs`, `screen_tests.rs`, child parser/capture modules/tests, `settings.rs`, provider defaults/template, and provider constructors outside runtime tests.

- [x] Write failing capture tests for literal pane argv, visible `-S 0`, styled recovery `-p -e -S -2000`, error mapping, and privacy.
- [x] Implement `PaneCapturePort`, `StyledPaneCapture`, and pure configured `ProviderScreenAdapter` implementations; state observation must not bypass the checkpoint with a summary.
- [x] Add backward-compatible `screen_adapter` (generic-v1 default; codex-v1 explicitly selected in shipped Codex defaults) and `screen_history_lines` (2000 default, valid 1..=10000); reject unknown adapter names at startup.
- [x] Write RED/GREEN Codex fixtures based on live styling: submitted ASCII/Unicode prompt, wrapped continuation, dim placeholder, nonempty draft, assistant/tool/shell rows, malformed and C1 controls, exact placeholder versus legitimate containing text, deep tail and output bounds.
- [x] Preserve generic configured state/prefix behavior and state-only OpenCode; show no hidden provider-ID switch.
- [x] Record exact focused verification in `adapter-report.md`.

## 3. Runtime recovery and revision slice

Owner: screen_recovery worker. Files: `runtime.rs`, `runtime_tests.rs`, reducer and reducer tests, optional checkpoint module. Consumes the screen facade above.

- [x] Write RED/GREEN checkpoint tests: first capture baseline only; longest suffix/prefix overlap; repeated identical submission; new eligible block after placeholder; lost overlap rebaseline; empty established baseline; composer excluded from anchors.
- [x] Keep checkpoint and monotonic retry policy per full incarnation, including inventory/hook/completion discovery and snapshot restart; discard it on replacement/departure.
- [x] Acquire an immediate baseline; retry failed baseline at most once/30s even while Idle; scan on raw Busy transition, retry once on Busy-to-Idle when no new prompt, and at most once/30s in sustained Busy. Do not let hook grace suppress independent prompt evidence or poll Idle history ordinarily.
- [x] Preserve state facts when history fails and existing degraded-health reporting. Commit candidate-bearing checkpoint changes only after successful source append so failure cannot consume recovered work.
- [x] Treat each checkpoint-proven candidate as a newly submitted occurrence even when its text repeats: refresh source recency and Sonnet revision while state-only observations remain corroboration.
- [x] Add runtime tests for exact submitted source before generation, failed appends, incarnation replacement, unchanged drafts, recovery cadence and independent state/summary. Prove recovered prompt feeds Sonnet without inventing completed work.
- [x] Record exact focused verification in `recovery-report.md`.

## 4. Real acceptance and completion

Owner: coordinator. No production synthetic events or external notifications.

- [x] Launch real Codex in a disposable tmux pane and temporary cwd/config; exclude hooks from the isolated Harold instance, disable notification effects, and verify exact process/config/store/listener ownership.
- [x] Establish baseline, submit several harmless distinctive tasks, push completed prompt above substantial output within the configured tail, leave an unsent draft, and verify exact recovered source through gRPC and the actual dashboard with Sonnet disabled.
- [x] Verify restart/pane reuse baseline does not inherit prior history and new post-baseline work recovers. Verify a later eligible prompt can feed real Sonnet without choosing the draft.
- [x] Record pane IDs, process IDs, actual capture dimensions, source strings, revisions, commands and pass/fail in the screen-testing ledger. Keep raw live captures ephemeral; only sanitized synthetic fixtures may enter test assets.
- [x] Reconcile durable architecture/reference/hook documentation and task records.
- [x] Run `cargo fmt --all -- --check`, `cargo test --offline --workspace --all-targets`, `cargo clippy --offline --workspace --all-targets -- -D warnings`, `cargo build --offline --release -p harold -p tmx-agent-dash`, and `git diff --check`.
- [x] Obtain independent completion review, fix findings, and receive thumbs-up.
- [x] Install/enable reviewed adapters in the existing local deployment with backup and verify owned runtime/config/store/stream; preserve notification settings and existing Sonnet feature.
- [x] Audit every saved requirement against tests/live evidence before marking the goal complete. Leave changes uncommitted unless requested otherwise.
