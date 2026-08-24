# Screen Testing Record

This is the source of truth for visual and terminal-screen verification. It records only tests that were actually run. Planned work stays in the final section until there is captured evidence.

Evidence classes:

- **Live terminal** — a real process rendered in a tmux pane and was inspected with `capture-pane` or direct observation.
- **Browser visual** — the HTML reference was rendered in a browser and visually inspected.
- **Deterministic buffer** — Ratatui rendered to `TestBackend`; useful render proof, but not a live terminal test.

Routine coordinator captures used only to see whether coding agents are busy are not product screen tests and are intentionally excluded.

## 2026-08-23

### ST-001 — HTML reference, wide browser render

- **Class:** Browser visual
- **Artifact:** `tasks/tmux-agent-dashboard/dashboard-visual.html`
- **Viewport:** 1440 by 1000
- **Action:** Rendered with headless Chrome and visually inspected during the design checkpoint.
- **Observed:** Signal-board hierarchy, agent inventory, selected-agent detail, counts, keyboard hints, and connection state were visible.
- **Result:** Pass for the wide reference.
- **Evidence limitation:** No screenshot was retained in the repository. The compact browser render must be rerun and retained during final Task 8 verification rather than inferred from responsive CSS.

### ST-002 — Ratatui wide renderer

- **Class:** Deterministic buffer, not a live terminal
- **Command:** `cargo test ui::tests::wide_dashboard -- --nocapture`
- **Observed:** `BUSY`, `IDLE`, `UNKNOWN`, `WORK SUMMARY`, active `/ event` search, `2 OF 3`, selected `CURRENT WORK`, `LIVE`, retained rows, and `MONITOR DEGRADED inventory:tmux_unavailable`. The buffer did not contain `EVIDENCE`, `HOOK`, or `SCREEN` labels.
- **Review regression:** The first review found empty or unknown monitor health looked healthy. A failing regression was added, then the three wide renderer tests passed with `MONITOR UNKNOWN` behavior.
- **Result:** Pass, three tests.

### ST-003 — First live dashboard demo in pane `%26`

- **Class:** Live terminal
- **Launch command:** `cargo run --example dashboard_demo`
- **Pane:** `%26`
- **Observed size:** 239 by 62 while `%26` was zoomed
- **Observed with:** `tmux capture-pane -p -t %26`
- **Observed content:** `HAROLD / TMUX CONTROL`, `LIVE`, revision `#01842`, a degraded inventory warning, busy/idle/unknown counts, active `/ event` filter, `2 OF 3 LOCAL`, work-summary rows, selected Codex details, and keyboard hints.
- **Result:** Pass for live wide rendering and example startup.
- **Limitation:** The example accepted only `q` and `Esc`; this did not test `j`, `k`, search editing, retry, or Enter navigation.

### ST-004 — Launch-path and pane-size diagnosis

- **Class:** Live terminal
- **Observed:** Pane `%26` had returned to `zsh`; the demo was no longer running. Plain `cargo run` returned `error: a bin target must be available for cargo run` because no production `src/main.rs` exists yet.
- **Observed size:** `%26` was 59 by 7 because pane `%22` was zoomed.
- **Result:** Two real usability failures identified: example-only launch wiring and an undersized hidden test pane.
- **Disposition:** Production binary wiring remains Task 7. The demo was restarted and `%26` was zoomed for immediate viewing.

### ST-005 — Demo restarted through tmux and made visible

- **Class:** Live terminal
- **Commands:**

  ```sh
  tmux send-keys -t %26 -l 'cargo run --example dashboard_demo'
  tmux send-keys -t %26 '' Enter
  tmux resize-pane -Z -t %22
  tmux select-pane -t %26
  tmux resize-pane -Z -t %26
  ```

- **Observed:** `pane_current_command=dashboard_demo`; after changing the zoom, `%26` was active at 239 by 62 and `capture-pane` again showed the dashboard header and degraded-monitor content.
- **Result:** Pass for launch via `tmux send-keys` and live visibility.
- **Limitation:** These keys launched the process only; they did not exercise application keyboard navigation.

### ST-006 — Harold/OpenCode visible-screen marker probe

- **Class:** Live terminal backend probe
- **Setup:** Harold work used a disposable tmux session running the locally installed OpenCode 1.4.10, captured only the current grid, reported the scalar count, checked candidate markers, and removed the temporary session.
- **Observed:** The idle screen contained `Ask anything`; source and live-screen investigation established provider-specific state clauses. The shared visible prefix was unsafe for work-summary extraction because prompt and user-message rows use the same prefix.
- **Result:** Pass for the OpenCode state-marker probe; summary fallback intentionally remains unconfigured for this version.
- **Privacy:** Raw captured screen text was not added to events, application state, logs, this record, or the dashboard API.

### ST-007 — Ratatui responsive and state renderer suite

- **Class:** Deterministic buffer, not a live terminal
- **Command:** `cargo test ui::tests -- --nocapture`
- **Synthetic dimensions:** Wide 140 by 38; medium 104 by 28; compact 72 by 22; undersized 59 by 17. Additional state fixtures used the same medium/wide dimensions.
- **Observed:** 31 renderer tests passed in the current shared checkout. They retained state/provider/target/summary and visible/total counts at compact width; hid detail at medium width and at 140 by 18; preserved abbreviated product identity, transport, and revision at 60 by 18; scrolled an off-viewport selected row into view; measured real wrapped detail content and fell back to full-width inventory at 140 by 27 rather than clipping; rendered the complete long detail at 140 by 44; rendered a resize instruction below the minimum; distinguished transport from healthy, unknown, degraded, recovered, stale, loading, and unavailable states; preserved rows under stale/degraded warnings; showed distinct empty and no-match copy; used exact missing-summary text; moved the selection marker after `j`; conservatively measured odd-width CJK, ZWJ-family emoji, and keycap/variation-selector grapheme clusters; handled zero-width measurement without a synthetic screen; counted 1,024 ZWJ-family clusters as 49 rows at width 43; matched Ratatui's trimmed multiple-whitespace boundary; and proved target, pane ID, directory, and work-summary spans borrow the selected agent's text. One of the 31 tests is a preserved concurrent Task 7 runtime-status fixture.
- **Result:** Pass, 31 tests. The initial run had 9 intended failures, followed by two focused compact-layout corrections, a three-test review RED/GREEN, a second two-test review RED/GREEN for compact product identity and content-aware wrapped detail, a Unicode review RED/GREEN, a hot-path review RED/GREEN, and a detail-allocation RED/GREEN. The Unicode fixture's first missing-import compile error was invalid RED; the valid odd-width CJK regression then failed at 2 measured rows versus 3 spill-safe rows. Unicode GREEN used Ratatui `styled_graphemes` plus conservative grapheme-cell measurement. For the hot-path review, the valid zero-width regression panicked in the old synthetic buffer scan; GREEN removed the area-sized buffer, scalar-derived height, and cloned lines. For the detail-allocation review, the valid borrowing regression failed on an owned TARGET span; GREEN builds the detail lines once, borrows them for measurement, and moves the same vector into rendering. A later bad summary index in the test was a fixture error and was not counted as RED.
- **Limitation:** This proves terminal-cell output from `TestBackend`; live resize and key-driven transitions still require the tmux tests below.

### ST-008 — Outside-tmux discovery reproduction attempt from coding pane

- **Class:** Live terminal environment probe.
- **Commands:**

  ```sh
  env -u TMUX -u TMUX_PANE tmux display-message -p '#{client_name}'
  env | rg '^TMUX(_PANE)?='
  tmux display-message -p -t %23 '#{pane_width}x#{pane_height}'
  ```

- **Pane/context:** Coding pane `%23`; invoking context before unsetting was `TMUX_PANE=%23`. Pane dimensions were 59 by 7. No client identity was returned by the reproduction command.
- **Observed:** The sandboxed reproduction exited 1 with `error connecting to /private/tmp/tmux-502/default (Operation not permitted)`. The ordinary process environment contained non-empty `TMUX` and `TMUX_PANE` markers.
- **Result:** Inconclusive for the coordinator-observed cross-client behavior; pass only for confirming the coding process's normal tmux markers and the sandbox limitation.
- **Limitation:** The sandbox prevented access to the tmux socket after removing the invoking context, so this run cannot replace the coordinator's live proof that an unrestricted outside-tmux process may resolve another attached client. The required post-runtime outside-tmux and isolated-client tests remain open.

### ST-009 — Production binary launch, resize boundary, and unavailable Harold

- **Class:** Live terminal
- **Commands:**

  ```sh
  tmux send-keys -t %26 -l 'cargo run'
  tmux send-keys -t %26 '' Enter
  tmux select-pane -t %26
  tmux resize-pane -Z -t %26
  tmux capture-pane -p -t %26
  tmux send-keys -t %26 -l 'r'
  tmux capture-pane -p -t %26
  ```

- **Pane/context:** Production binary `target/debug/tmx-agent-dash` in designated pane `%26`; initial size 59 by 7, then zoomed to 239 by 62. The only attached client was `/dev/ttys000` on session `tmx-agent-dash`.
- **Observed:** At 59 by 7 the live binary rendered `Terminal too small` and `Resize to at least 60x18`. After selecting and zooming `%26`, the full production screen rendered `TRANSPORT UNAVAILABLE`, revision zero, unknown monitor health, zero rows, and the sanitized reason `Harold rejected WatchAgentStates: code: 'Operation is not implemented or not supported'`. The empty-state body showed `Harold is unavailable` and `Press r to retry now`. Sending literal `r` left the process alive and returned to the same unavailable/retry state after Harold rejected the new request.
- **Result:** Pass for the normal `cargo run` target, live minimum-size boundary, resize redraw, unavailable/retry rendering, and non-crashing live `r` input.
- **Limitation:** Harold does not yet implement the streaming RPC, so this run cannot prove snapshots, rows, selection, search, reconnect state replacement, or Enter navigation. The displayed retry duration remained `5000ms` before and after `r`, so this capture proves the retry path remained operational but not a visible countdown reset.

### ST-010 — Reviewed production runtime restart and endpoint feedback

- **Class:** Live terminal
- **Commands:** Sent `q` to `%26`, then attempted a fresh `cargo run`; after observing the sequencing failure described below, sent `cargo run` and Enter separately from a confirmed clean shell prompt, selected `%26`, and zoomed it to 239 by 62.
- **Observed harness failure:** The first restart attempt produced shell input `qcargo run` and `zsh: command not found: qcargo`. The quit character had reached the restored shell instead of being fully consumed before the next literal command. No project command ran from that malformed input.
- **Observed repaired run:** The reviewed `target/debug/tmx-agent-dash` ran in `%26`. At 59 by 7 it rendered the minimum-size instruction; after `%26` became active and zoomed, it redrew the full unavailable state. Retry feedback now included the selected sanitized endpoint: `http://127.0.0.1:50060: Harold rejected WatchAgentStates: code: 'Operation is not implemented or not supported'`.
- **Result:** Pass for reviewed-binary launch, resize redraw, and endpoint-bearing sanitized retry feedback. The failed first restart is retained as test-harness evidence and produced a lesson requiring shell readiness before relaunch.
- **Limitation:** This is still the unavailable Harold path. Real agent inventory and state transitions remain pending.

### ST-011 — Retained wide HTML reference screenshot

- **Class:** Browser visual
- **Command:** `'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' --headless=new --disable-gpu --hide-scrollbars --allow-file-access-from-files --window-size=1440,1000 --screenshot=/Users/kahgeh/Dev/p/tmx-agent-dash/tasks/tmux-agent-dashboard/screenshots/dashboard-reference-1440x1000.png file:///Users/kahgeh/Dev/p/tmx-agent-dash/tasks/tmux-agent-dashboard/dashboard-visual.html`
- **Browser context:** The managed browser inventory was empty (`[]`), so the installed Google Chrome headless executable rendered the local file directly.
- **Viewport/artifact:** 1440 by 1000; `tasks/tmux-agent-dashboard/screenshots/dashboard-reference-1440x1000.png`; SHA-256 `1f26a54a5d2afe78cb7601c5b30edf0c045dbf3ab86578270621aabf59daf3c2`.
- **Observed:** The full signal-board frame fit without clipping. The active `event` filter and both `2 of 5` visible/total counts were legible. The table retained state, agent, target, work summary, and age columns; the selected Claude row matched the side-by-side `SELECTED SIGNAL` panel and complete `CURRENT WORK` copy. Connection/stale status and the `j`, `k`, `/`, `Esc`, Enter, `r`, and `q` keyboard hints were visible. No evidence, hook, or raw-screen provenance label was visible.
- **Result:** Pass for the retained wide HTML reference.
- **Limitation:** This is a static HTML design reference, not the Ratatui production renderer or a live Harold/provider integration test.

### ST-012 — Retained compact HTML reference screenshot

- **Class:** Browser visual
- **Command:** `'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' --headless=new --disable-gpu --hide-scrollbars --allow-file-access-from-files --window-size=760,1100 --screenshot=/Users/kahgeh/Dev/p/tmx-agent-dash/tasks/tmux-agent-dashboard/screenshots/dashboard-reference-760x1100.png file:///Users/kahgeh/Dev/p/tmx-agent-dash/tasks/tmux-agent-dashboard/dashboard-visual.html`
- **Browser context:** The managed browser inventory was empty (`[]`), so the installed Google Chrome headless executable rendered the local file directly.
- **Viewport/artifact:** 760 by 1100; `tasks/tmux-agent-dashboard/screenshots/dashboard-reference-760x1100.png`; SHA-256 `dc096f6dda5c9d252d2bc1e5f563d350976a0d26d15c26431f3a235897f3297e`.
- **Observed:** The masthead and connection metadata stacked cleanly; summary counts, inventory-observed time, active `event` filter, and `2 of 5` visible/total counts remained legible. The inventory table kept its work-summary column, with intentional ellipsis at this width. `SELECTED SIGNAL` moved below the table and retained the complete `CURRENT WORK` copy. The keyboard hints wrapped onto two lines without collision, and no evidence, hook, or raw-screen provenance label was visible.
- **Result:** Pass for the retained compact HTML reference and responsive stacking.
- **Limitation:** This is a static HTML design reference, not the Ratatui production renderer or a live Harold/provider integration test.

### ST-013 — Real provider process startup in isolated tmux sessions

- **Class:** Live terminal, real installed provider CLIs.
- **Topology:** `tmx-e2e-claude:0.0` pane `%33` (`/dev/ttys051`), `tmx-e2e-codex:0.0` pane `%34` (`/dev/ttys058`), and `tmx-e2e-opencode:0.0` pane `%35` (`/dev/ttys059`). Each pane used its own empty directory under `/private/tmp/tmx-agent-dash-e2e-20260823/`.
- **Commands:** Sent literal `claude`, `codex`, and `opencode` with `tmux send-keys`; every `Enter`, including each disposable-directory trust confirmation, was sent as a separate command. Captured each pane and queried `pane_id`, `pane_current_command`, `pane_pid`, `pane_tty`, and dimensions.
- **Observed:** Claude Code 2.1.241 reached its authenticated `❯` prompt in `%33`; Codex 0.149.0 reached `Ask Codex to do anything` in `%34`; OpenCode's live UI reported 1.18.15 and reached `Ask anything` with `tab agents` and `ctrl+p commands` in `%35`. All three panes remained real agent processes after startup. The earlier noninteractive `claude auth status` reported `loggedIn: false`, but the interactive client subsequently showed the signed-in organization and usable prompt, so the live UI supersedes that probe for this run.
- **Result:** Pass for isolated real-process startup and idle-screen availability.
- **Limitation:** This step preceded the development Harold listener. It does not prove inventory rows, state classification, summaries, or task execution. OpenCode's installed UI is newer than the previously probed 1.4.10 contract, so its markers must be revalidated rather than assumed.

### ST-014 — Development Harold snapshot stream and initial real-agent inventory

- **Class:** Live Harold, gRPC stream, and production Ratatui dashboard.
- **Isolation:** Development Harold commit `adae92a` ran in pane `%6` on `127.0.0.1:50061`, using `fixtures/harold-e2e-config/default.toml`, store `/private/tmp/tmx-agent-dash-e2e-20260823/harold-events`, and a temporary `osascript` no-op shim. The deployed listener on `50060` and its store/config were untouched.
- **Commands:** Started `cargo run --offline -p harold` with the isolated environment; restarted `%26` from a confirmed shell prompt with `cargo run -- --endpoint http://127.0.0.1:50061`; captured `%26`; then ran `grpcurl -max-time 1 -plaintext -import-path /Users/kahgeh/Dev/p/harold/harold-api/proto -proto harold.proto -d '{}' 127.0.0.1:50061 harold.Harold/WatchAgentStates`.
- **Observed:** Harold listened on `50061`. The dashboard rendered `TRANSPORT LIVE`, revision 19, and ten real agent panes. The three disposable rows were correctly identified as Claude `%33`, Codex `%34`, and OpenCode `%35`, with their exact tmux targets and working directories; each initially classified `IDLE`. The direct gRPC snapshot agreed with the dashboard and included the exact pane, shell PID, agent PID, process-start time, provider, directory, state, and revision for each row.
- **Failure found:** Idle Codex panes, including `%34`, exposed `Ask Codex to do anything` as `workSummary`. That is a prompt placeholder, not a useful description of prior work. Claude and OpenCode correctly had no initial summary. This live defect was sent back to Harold development with a required regression: idle prompt markers must not become or overwrite work summaries.
- **Result:** Pass for snapshot-first transport, real inventory discovery, provider identity, and initial state. Fail for Codex initial summary semantics; the comprehensive task test remains open until fixed and re-run.

### ST-015 — Real Claude and Codex controlled task probes

- **Class:** Live provider panes observed through live Harold and production dashboard.
- **Claude command:** Sent `E2E-CLAUDE: Run the non-mutating shell command sleep 12, then reply with exactly CLAUDE-E2E-DONE. Do not create or edit files.` to `%33`, with `Enter` separate.
- **Claude observed:** Claude immediately returned `Login expired · Please run /login`; no command ran and no completion was fabricated. Harold retained `IDLE` and captured the submitted prompt as a screen-summary candidate. **Result:** blocked/fail for the requested Claude busy-to-idle run until the user completes the interactive login in `%33`.
- **Codex command:** Sent the equivalent `E2E-CODEX` request to `%34`, asking for `sleep 12` and exact reply `CODEX-E2E-DONE`, with `Enter` separate.
- **Codex observed busy:** At `2026-08-23T21:48:18+1000`, `%34` showed `Working (1s · esc to interrupt)` and dashboard revision 24 changed `%34` from idle to busy. The dashboard incorrectly retained `Ask Codex to do anything` as the summary.
- **Codex observed idle:** At `2026-08-23T21:48:37+1000`, `%34` showed `Ran sleep 12` and `CODEX-E2E-DONE`; dashboard revision 25 returned `%34` to idle. Its summary was still the prompt placeholder rather than the submitted task.
- **Result:** Pass for real Codex idle-to-busy-to-idle classification and controlled non-mutating execution. Fail for summary correctness. The current UI renders a new `› Ask Codex to do anything` placeholder below the submitted prompt even while working, so the bottom-up prefix extractor selects the wrong line. Harold development received this exact failure and a required regression.

### ST-016 — Real OpenCode 1.18.15 controlled task probe

- **Class:** Live provider pane observed through live Harold and production dashboard.
- **Command:** Sent `E2E-OPENCODE: Run the non-mutating shell command sleep 12, then reply with exactly OPENCODE-E2E-DONE. Do not create or edit files.` to `%35`, with `Enter` separate.
- **Observed busy:** At `2026-08-23T21:48:54+1000`, `%35` showed the submitted task and `esc interrupt`; dashboard revision 28 changed `%35` from idle to busy. Summary remained missing, as expected from the deliberately absent unsafe screen prefix.
- **Observed completion:** By `2026-08-23T21:49:21+1000`, `%35` showed `$ sleep 12`, `(no output)`, and `OPENCODE-E2E-DONE` with a 25.1-second turn duration.
- **Failure found:** Dashboard revision 32 still reported `%35` busy 46 seconds after completion. OpenCode 1.18.15's completed screen no longer satisfied the configured idle conjunction `agents` plus `commands`, so screen evidence became inconclusive and stale busy state remained. No work summary was available.
- **Result:** Pass for real OpenCode execution and busy detection. Fail for idle transition and summary. The required fix is the isolated real OpenCode lifecycle adapter through `ReportAgentState`, with current-version screen markers retained only as supplemental evidence.

### ST-017 — Live `j`/`k` navigation and incremental search in `%26`

- **Class:** Live production Ratatui keyboard test against the real ten-row Harold snapshot.
- **Navigation:** Captured the selected `tmx-agent-dash:0.3` Codex row; sent literal `j` and captured the selection marker on `tmx-e2e-opencode:0.0`; sent literal `k` and captured it back on `tmx-agent-dash:0.3`. Detail-panel target/directory changed with the selected row.
- **Search:** Sent `/`, literal `tmx-e2e`, and a separately sent `Enter`. While editing, the dashboard rendered `FILTER / tmx-e2e [EDITING]`, `3 OF 10 LOCAL`, and only the Claude, Codex, and OpenCode disposable rows. After `Enter`, it rendered the same three rows with `[ACTIVE]`.
- **Escape semantics:** Sending `Esc` outside search editing quit the process, matching the documented `Esc`-outside-edit behavior. After a clean relaunch on port 50061, sent `/`, literal `E2E-OPENCODE`, and separate `Enter`; the live target field produced `1 OF 10 LOCAL`. Sent `/` again to enter editing, then `Esc`; the process stayed alive, cleared the query, returned to `[ACTIVE]`, and restored `10 OF 10 LOCAL`.
- **Result:** Pass for real `j`/`k`, incremental filtering during editing, separate-Enter acceptance, local matching, and edit-mode `Esc` clearing. The explicit process quit outside editing is retained as expected live evidence, not hidden as a harness failure.

### ST-018 — Live Harold disconnect, stale retention, and same-store reconnect

- **Class:** Live server lifecycle and production dashboard reconnect.
- **Commands:** Sent `C-c` to Harold pane `%6`; waited for the process to return to `zsh`; captured Harold shutdown logs and `%26`. Relaunched the same development Harold command on `50061` with the same isolated store and no-op side-effect environment; confirmed the listener with `lsof`; waited through the dashboard retry interval and captured `%26` again.
- **Observed shutdown:** Harold logged SIGINT, server shutdown, iMessage-listener shutdown, and projector shutdown. The dashboard changed from live to `TRANSPORT STALE`, retained revision 32 and all ten rows, retained selection and details, displayed the age of the last committed snapshot, and showed bounded endpoint/retry feedback.
- **Observed restart:** Harold listened again on `127.0.0.1:50061` using the existing temp store. After the dashboard retry, it returned to `TRANSPORT LIVE` at revision 34 with the same ten pane identities and retained summaries/states before current inventory reconciliation. No dashboard restart was needed.
- **Result:** Pass for graceful Harold stop, stream close, stale-row retention, retry feedback, persistent same-store recovery, snapshot-first reconnect, and live transport recovery. The already-recorded incorrect Codex/OpenCode state-summary values were faithfully retained; reconnect did not hide or repair those upstream defects.

### ST-019 — Live SIGINT and SIGTERM terminal restoration

- **Class:** Live production process signal and PTY cleanup.
- **SIGINT:** Identified the exact `%26` foreground process from its pane TTY (`target/debug/tmx-agent-dash --endpoint http://127.0.0.1:50061`, PID 95777) and sent `kill -INT 95777`. Within 1.2 seconds `%26` returned to `zsh`, `alternate_on=0`, `pane_dead=0`, and a normal shell prompt.
- **SIGTERM:** Relaunched from a confirmed shell prompt, identified the new exact foreground PID 40626, and sent `kill -TERM 40626`. Within 1.2 seconds `%26` again returned to `zsh`, `alternate_on=0`, `pane_dead=0`, and a normal prompt.
- **Explicit quit:** After another clean relaunch, sent literal `q`. Within 700 milliseconds `%26` returned to `zsh` with `alternate_on=0` and `pane_dead=0`; the subsequent command was sent only after that shell-ready check.
- **Raw-key observation:** A literal terminal `C-c` while Crossterm raw mode was active did not produce an OS SIGINT; the process remained in the alternate screen. The acceptance test therefore used actual POSIX signals against the resolved foreground PID, matching the runtime's documented SIGINT/SIGTERM contract.
- **Result:** Pass for live `q`, SIGINT, and SIGTERM restoration of the alternate screen and shell usability. `%26` was then relaunched on port 50061 and confirmed `cmd=tmx-agent-dash`, `alternate_on=1`, zoomed 239 by 62.

### ST-020 — Outside-tmux navigation disabled with an attached client present

- **Class:** Live production TUI in a separate PTY with tmux authority deliberately removed.
- **Setup:** The real attached tmux client was `/dev/ttys000`, session `tmx-agent-dash`, active pane `%26`. In a fresh PTY, launched `env -u TMUX -u TMUX_PANE target/debug/tmx-agent-dash --endpoint http://127.0.0.1:50061` while that client remained attached.
- **Observed UI:** The outside-tmux process entered its own alternate screen, connected to Harold revision 34, rendered the same ten rows, and explicitly showed `NAVIGATION UNAVAILABLE`.
- **Enter test:** Sent a real carriage-return key to the outside-tmux TUI. It stayed running and did not acquire or move a tmux client. A fresh tmux inventory still reported the only client as `/dev/ttys000`, session `tmx-agent-dash`, pane `%26`—identical to before.
- **Cleanup:** Sent literal `q`; the outside PTY process exited 0 and emitted cursor/alternate-screen restoration sequences.
- **Result:** Pass. Missing `TMUX`/`TMUX_PANE` disables navigation even while an unrelated attached client exists, and pressing Enter cannot move that client.

### ST-021 — Isolated-client Enter navigation moves only the invoking client

- **Class:** Live two-client tmux navigation test with a disposable control client.
- **Setup:** Created disposable session `tmx-nav-client` with pane `%36`; attached control-mode client `/dev/ttys065` to it while the user's ordinary client `/dev/ttys000` remained on `tmx-agent-dash` pane `%26`. Started the production dashboard in `%36` on port 50061 and captured its selected first row: busy Codex target `harold:0.3`.
- **Before Enter:** `/dev/ttys000` = session `tmx-agent-dash`, pane `%26`, control mode off; `/dev/ttys065` = session `tmx-nav-client`, pane `%36`, control mode on.
- **Action:** Sent `Enter` separately to dashboard pane `%36`.
- **After Enter:** `/dev/ttys065` moved to session `harold`, pane `%5`, matching the selected row. `/dev/ttys000` remained exactly on session `tmx-agent-dash`, pane `%26`. The dashboard process in `%36` remained alive.
- **Cleanup:** Sent literal `q` to `%36`, confirmed `zsh` and `alternate_on=0`, detached `/dev/ttys065`, confirmed only `/dev/ttys000` remained, then removed the disposable `tmx-nav-client` session.
- **Result:** Pass. Enter targets the resolved client explicitly and does not move an unrelated attached client.

### ST-022 — Wide status-colour HTML reference

- **Class:** Browser visual.
- **Command:** `'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' --headless=new --disable-gpu --hide-scrollbars --allow-file-access-from-files --window-size=1440,1000 --screenshot=/Users/kahgeh/Dev/p/tmx-agent-dash/tasks/tmux-agent-dashboard/screenshots/dashboard-status-colours-1440x1000.png file:///Users/kahgeh/Dev/p/tmx-agent-dash/tasks/tmux-agent-dashboard/dashboard-visual.html`
- **Browser context:** The managed browser inventory was empty (`[]`), so the installed Google Chrome headless executable rendered the local file directly. Pane `%26` was not used or changed.
- **Viewport/artifact:** 1440 by 1000; `tasks/tmux-agent-dashboard/screenshots/dashboard-status-colours-1440x1000.png`; SHA-256 `16d02d0c5d11f304a32d03221da9e0b20919bd3a32cd0b64251edc1f76dad865`.
- **Observed:** The table visibly distinguished amber solid `Busy`, green solid `Idle`, and muted-grey hollow `Unknown` while retaining every state word. The selected detail repeated the amber solid indicator with the word `Busy` and provider name. All three rows, the `3 of 5` filtered counts, work summaries, selected current work, and the full keyboard bar fit without clipping. The footer showed `Esc` only as search clear and `q` as quit; it did not claim that Esc quits.
- **Result:** Pass for the final wide semantic status mapping and q-only quit hint.
- **Limitation:** This is a static HTML reference, not live Ratatui key handling or provider-state evidence.

### ST-023 — Compact status-colour HTML reference and clipping correction

- **Class:** Browser visual.
- **Command:** `'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' --headless=new --disable-gpu --hide-scrollbars --allow-file-access-from-files --window-size=760,1100 --screenshot=/Users/kahgeh/Dev/p/tmx-agent-dash/tasks/tmux-agent-dashboard/screenshots/dashboard-status-colours-760x1100.png file:///Users/kahgeh/Dev/p/tmx-agent-dash/tasks/tmux-agent-dashboard/dashboard-visual.html`
- **Browser context:** The managed browser inventory was empty (`[]`), so the installed Google Chrome headless executable rendered the local file directly. Pane `%26` was not used or changed.
- **Viewport/artifact:** 760 by 1100; `tasks/tmux-agent-dashboard/screenshots/dashboard-status-colours-760x1100.png`; SHA-256 `ccb6799d399e4a7da513816e2f44b244065b246e13a343f4c9635b76e811b64a`.
- **Initial observation/result:** Fail. Adding the third visible state row pushed part of the keyboard footer below the 1100-pixel viewport. That intermediate screenshot was inspected and then overwritten by the corrected retained artifact.
- **Correction:** Reduced only compact panel/detail spacing with existing `--space-*` custom properties; no raw colour or spacing value was added.
- **Final observed:** Amber solid `Busy`, green solid `Idle`, and muted-grey hollow `Unknown` remained distinct with their words present. The selected `Busy · Claude Code` identity, complete current work, all three table rows, counts, search field, `Esc` clear hint, q-only quit hint, and the entire board/footer fit without overlap or clipping.
- **Result:** Pass after the compact token-only spacing correction.
- **Limitation:** This is a static HTML reference, not live Ratatui key handling or provider-state evidence.

### ST-024 — Live semantic status colours and final `Esc`/`q` contract in `%26`

- **Class:** Live production Ratatui keyboard and terminal-restoration test.
- **Process/topology:** Relaunched the reviewed dashboard in pane `%26` with `cargo run -- --endpoint http://127.0.0.1:50061`; tmux reported `cmd=tmx-agent-dash`, `alternate_on=1`, size `239x62`, pane PID `11550`, and TTY `/dev/ttys049`.
- **Status observation:** The real ten-row Harold inventory visibly rendered solid `● BUSY` labels in amber/orange, solid `● IDLE` labels in green, and a hollow `○ UNKNOWN` label in muted grey. The selected detail retained `● BUSY  Codex`, and the footer rendered `Esc clear` and `q quit`; every state retained its word for monochrome/accessibility use.
- **Editing-query action:** Sent `/`, literal `tmx-e2e`, then captured `FILTER / tmx-e2e [EDITING]` and `3 OF 10 LOCAL`. Sent `Escape` as its own key command. The next capture showed an empty `FILTER / [ACTIVE]`, `10 OF 10 LOCAL`, `cmd=tmx-agent-dash`, and `alternate_on=1`.
- **Empty editing action:** Sent `/` with the query already empty and captured `FILTER / [EDITING]` with `10 OF 10 LOCAL`. Sent `Escape`; the next capture showed `FILTER / [ACTIVE]`, proving edit mode closed even with an empty edit buffer while the TUI remained running.
- **Accepted-query action:** Sent `/`, literal `tmx-e2e`, and `Enter` as a separate command. Captured `FILTER / tmx-e2e [ACTIVE]` and the three disposable Claude, Codex, and OpenCode rows. Sent `Escape`; the next capture showed the accepted filter cleared directly to empty `FILTER / [ACTIVE]` and `10 OF 10 LOCAL`, with the TUI still running.
- **No-filter action:** Sent `Escape` once more with an empty non-editing query. The process remained `tmx-agent-dash` in the alternate screen and the filter/count remained empty and `10 OF 10 LOCAL`.
- **Quit/restoration:** Sent literal `q`. After one second tmux reported `cmd=zsh`, `alternate_on=0`, and `pane_dead=0`. Relaunched only after confirming the shell-ready state; `%26` returned to `cmd=tmx-agent-dash`, `alternate_on=1`, and remains running at `239x62` on port `50061`.
- **Result:** Pass. Search-edit cancellation including an empty edit buffer, accepted-filter clearing, no-filter no-op, q-only quit, terminal restoration, and all three live state treatments match the approved contract.

### ST-025 — Fresh real Codex and OpenCode lifecycle integrations on corrected Harold

- **Class:** Live real-provider processes, production Harold stream, and production Ratatui dashboard.
- **Harold restart:** Gracefully stopped only development pane `%6`, confirmed `cmd=zsh`, then relaunched the isolated port-50061 command with the existing temporary config/store and no-op side-effect shim. Harold PID `39325` listened on `127.0.0.1:50061`; deployed port `50060` remained untouched. `%26` reconnected without restart at revision 524.
- **Adapter setup:** Relaunched Codex `%34` with `HAROLD_ADDR=127.0.0.1:50061`. Relaunched OpenCode 1.18.15 `%35` with `HAROLD_ADDR=127.0.0.1:50061`, the repository `HAROLD_PROTO`, and process-local `OPENCODE_CONFIG_CONTENT` pointing to `file:///Users/kahgeh/Dev/p/harold/hooks/opencode/harold-plugin.js`; no global OpenCode config was changed.
- **Codex action:** Sent `E2E-CODEX-2: Run the non-mutating shell command sleep 12, then reply with exactly CODEX-2-DONE. Do not create or edit files.` with `Enter` separate. The first Enter left the submitted text visible but did not start work, so no false busy result was recorded; a second separately sent Enter produced the real `Working` state.
- **Codex observed:** Dashboard revision 534 showed `%34` as `● BUSY`, age 2s, with `E2E-CODEX-2…` as the work summary. The real provider ran `sleep 12` and replied `CODEX-2-DONE`. Revision 538 returned `%34` to `● IDLE`, age 4s, and retained the complete substantive submitted instruction. `Ask Codex to do anything` did not replace it.
- **OpenCode action:** Sent `E2E-OPENCODE-2: Run the non-mutating shell command sleep 12, then reply with exactly OPENCODE-2-DONE. Do not create or edit files.` with `Enter` separate.
- **OpenCode observed:** Dashboard revision 541 showed `%35` as `● BUSY`, age 2s, with the exact substantive submitted instruction. OpenCode ran the real command and completed the turn in 39.2s with `OPENCODE-2-DONE`. Revision 543 returned `%35` to `● IDLE`, age 15s, and retained the same instruction; the earlier stale-busy/no-summary defect did not recur.
- **Claude status:** `%33` still displays `Login expired · Please run /login` and `Not logged in · Run /login`. No Claude success or simultaneous three-provider result is claimed.
- **Result:** Pass for fresh real Codex and OpenCode busy/idle classification, exact work-summary semantics, retained idle summary, and independent provider isolation through the production stream. Claude and the three-provider concurrent gate remain blocked on user authentication.

### ST-026 — Concurrent Codex and OpenCode lifecycle and summary isolation

- **Class:** Live concurrent real-provider processes, production Harold stream, and production Ratatui dashboard.
- **Topology:** Codex 0.149.0 in `%34`, OpenCode 1.18.15 with the process-local Harold plugin in `%35`, and the dashboard in `%26` at 239 by 62, all using the isolated Harold listener on `127.0.0.1:50061`.
- **First action:** Sent `E2E-CONCURRENT-CODEX: Run sleep 12, then reply exactly CONCURRENT-CODEX-DONE. Do not create or edit files.` to `%34` and the corresponding `E2E-CONCURRENT-OPENCODE` request for `CONCURRENT-OPENCODE-DONE` to `%35`; each `Enter` was sent separately.
- **First observed result:** Both real providers completed and the dashboard returned both rows to `● IDLE`. Each row retained its own matching `E2E-CONCURRENT-*` submitted instruction; neither provider inherited the other provider's summary.
- **Second action:** Sent `E2E-DUAL-CODEX: Run sleep 20, then reply exactly DUAL-CODEX-DONE. Do not create or edit files.` to `%34` and `E2E-DUAL-OPENCODE: Run sleep 20, then reply exactly DUAL-OPENCODE-DONE. Do not create or edit files.` to `%35`, again with every `Enter` sent separately.
- **Simultaneous-busy observation:** Dashboard revision 557 captured both `%34` and `%35` as `● BUSY` at the same time. OpenCode showed its current `E2E-DUAL-OPENCODE` summary, but Codex still showed its prior `E2E-CONCURRENT-CODEX` summary instead of the current `E2E-DUAL-CODEX` instruction. This summary-freshness failure was sent upstream; it is not counted as a pass merely because the state transition was correct.
- **Idle observation:** Both providers completed their real commands and returned the requested `DUAL-CODEX-DONE` and `DUAL-OPENCODE-DONE` replies. At dashboard revision 563 both disposable rows were `● IDLE` and independently retained the correct current `E2E-DUAL-CODEX` and `E2E-DUAL-OPENCODE` summaries.
- **Result:** Pass for two-provider simultaneous busy/idle transitions, completion, and idle no-cross-talk. Fail for current Codex work-summary freshness during the simultaneous-busy phase. This does not satisfy the three-provider concurrent gate; Claude remains blocked on interactive authentication.

### ST-027 — Live feature-gated terminal fault and exactly-once cleanup harness

- **Class:** Live PTY failure-path and terminal-restoration test in the designated pane `%26`.
- **Build:** `cargo build --offline --features terminal-fault-harness --example terminal_fault_harness`.
- **Actions:** From a confirmed shell prompt, ran the built `target/debug/examples/terminal_fault_harness` binary directly four times with `partial-init`, `render-failure`, `panic-cleanup`, and `restoration-failure`. Each case returned to the shell before the next case was started.
- **Harness output:** Every scenario printed `outcome=expected cleanup=exactly-once`. The partial-initialization trace omitted `show_cursor`, as expected because acquisition had not reached that stage; the render-failure, panic-cleanup, and restoration-failure traces included `show_cursor`. The panic scenario printed its intentional caught-panic message before reporting the expected successful harness outcome.
- **PTY observation:** After every case, tmux reported pane `%26` as `cmd=zsh`, `alternate_on=0`, `pane_dead=0`, and `tty=/dev/ttys049`.
- **Isolation:** The harness is available only with the `terminal-fault-harness` feature. Its independent review approved the implementation; the default full gate passed 108 tests and the feature-enabled full gate passed 109 tests.
- **Result:** Pass for live partial-initialization rollback, render-failure cleanup, panic cleanup, restoration-failure handling, exactly-once cleanup reporting, and usable terminal restoration.

### ST-028 — Reviewed Harold concurrent summary-recency rerun

- **Class:** Live concurrent real-provider processes, reviewed production Harold stream, and production Ratatui dashboard.
- **Harold restart:** Stopped the old isolated Harold in `%6`, then restarted the current reviewed Harold commit `d9a55ea` in `%6` with the existing temporary config and event store. PID `16086` listened on `127.0.0.1:50061`; the deployed listener on port `50060` remained untouched.
- **Adapter setup:** Relaunched OpenCode `%35` with the reviewed process-local Harold plugin. Codex remained in `%34`; the dashboard remained in `%26` against the isolated port-50061 listener.
- **Action:** Sent `E2E-RECENCY-CODEX: Run sleep 20, then reply exactly RECENCY-CODEX-DONE. Do not create or edit files.` to `%34` and `E2E-RECENCY-OPENCODE: Run sleep 20, then reply exactly RECENCY-OPENCODE-DONE. Do not create or edit files.` to `%35`; each `Enter` was sent separately.
- **Simultaneous-busy observation:** Dashboard revision 571 captured `%34` and `%35` as `● BUSY` at the same time. Codex showed the current distinct `E2E-RECENCY-CODEX` instruction and OpenCode showed the current distinct `E2E-RECENCY-OPENCODE` instruction. The stale-prior-Codex-summary failure recorded in ST-026 did not recur.
- **Completion observation:** Codex returned exactly `RECENCY-CODEX-DONE`. OpenCode completed in 34.4 seconds and returned exactly `RECENCY-OPENCODE-DONE`.
- **Idle observation:** At dashboard revision 577 both disposable rows were `● IDLE`, each retaining its own current `E2E-RECENCY-*` instruction with no inherited or cross-provider summary.
- **Result:** Pass for two-provider simultaneous busy-to-idle transitions, current work-summary recency during the busy phase, retained idle summaries, and no cross-talk. Claude and the three-provider concurrent gate remain open.

### ST-029 — OpenCode provider departure, removal, and new-incarnation rejoin

- **Class:** Live real-provider departure/rejoin through reviewed Harold and the production Ratatui dashboard.
- **Departure action:** After ST-028, sent `C-c` to disposable OpenCode pane `%35`. Tmux reported `cmd=zsh`, `alternate_on=0`, and `pane_dead=0`.
- **Departure observation:** Dashboard revision 578 changed the authoritative inventory from ten rows to nine and removed target `tmx-e2e-opencode:0.0`. The other pane rows and their work summaries remained present.
- **Rejoin command:** Relaunched `%35` with the process-local plugin only, sending `Enter` separately:

  ```sh
  env HAROLD_ADDR=127.0.0.1:50061 \
    HAROLD_PROTO=/Users/kahgeh/Dev/p/harold/harold-api/proto/harold.proto \
    OPENCODE_CONFIG_CONTENT='{"plugin":["file:///Users/kahgeh/Dev/p/harold/hooks/opencode/harold-plugin.js"]}' \
    opencode .
  ```

- **Process observation:** Tmux reported `%35` as `cmd=opencode.exe`, `alternate_on=1`, and `pane_dead=0`.
- **Rejoin observation:** Dashboard revision 580 restored the authoritative inventory to ten rows and restored the same OpenCode pane ID as `● IDLE` with `No work summary reported`. The new process incarnation did not inherit the departed process's `E2E-RECENCY-OPENCODE` summary.
- **Result:** Pass for provider departure detection, authoritative row removal, unaffected-row retention, provider rejoin, and process-incarnation summary isolation.

### ST-030 — Durable Codex placeholder repair and same-store replay

- **Class:** Live reviewed Harold upgrade, durable state repair, restart replay, and production Ratatui dashboard observation.
- **Pre-repair observation:** At dashboard revision 588, the exact placeholder `Ask Codex to do anything` appeared as the durable work summary on four Codex rows: `events:0.3`, `events:0.4`, `home:0.1`, and `tmx-agent-dash:0.4`.
- **Reviewed build:** Harold commit `c27cd89` received independent approval after 141 Harold tests and 197 workspace tests passed; formatting, warnings-denied Clippy, and diff checks were green.
- **Upgrade action:** Stopped the isolated `d9a55ea` listener on `127.0.0.1:50061`, then started Harold `c27cd89` against the same temporary config and durable event store. The deployed listener on port `50060` remained untouched.
- **Repair observation:** Dashboard revision 594 changed all four affected rows to `No work summary reported`. A capture contained zero occurrences of the exact placeholder phrase `Ask Codex to do anything`.
- **Replay action:** Stopped and restarted Harold `c27cd89` once more against the same config and durable store. After replay, the isolated Harold PID was `95046`; the deployed port-50060 listener remained untouched.
- **Replay observation:** The dashboard returned at revision 594 with all four rows still repaired. A fresh replay capture again contained zero occurrences of the exact placeholder phrase.
- **Result:** Pass for upgrading an existing durable store, repairing all known persisted Codex prompt placeholders, and preserving that repair across a same-store restart/replay without changing the deployed Harold instance.

### ST-031 — Final reviewed durable repair with exact fixture replay

- **Class:** Superseding live acceptance for reviewed Harold code, exact-fixture isolation, durable state repair, restart replay, and production Ratatui dashboard observation.
- **Superseded reviews:** Deeper Harold reviews rejected the `c27cd89` implementation used in ST-030 and then rejected `7523735` on its own. ST-030 remains a record of what was observed, but it is not the final accepted implementation or acceptance run.
- **Final code:** Commit `7523735` starts repair before hub publication, applies one atomic all-pane batch so a 500-page boundary cannot expose partial repair, and rejects invalid ingress before serialization. Follow-up commit `1568fd04e2d04c9a73f11330b495fe173538d0fa` adds exact-match fallback for unknown or removed providers. Independent and in-session reviews both returned explicit thumbs-up. The final gates passed 148 of 148 Harold tests and 204 of 204 workspace tests; formatting, warnings-denied Clippy, and Git checks were green, with no dependency changes. Harold documentation was committed as `c0559c8`.
- **Configuration correction:** During restart, an interim replacement accidentally used repository configuration. It was explicitly discarded and stopped, and no observation from it is counted as acceptance evidence. The accepted runs below used the exact fixture configuration in `%6`, including its temporary chat database and no-op side effects.
- **First accepted run:** Started the final reviewed Harold against the existing isolated durable store on `127.0.0.1:50061`. Dashboard revision 602 contained ten rows; `events:0.3`, `events:0.4`, `home:0.1`, and `tmx-agent-dash:0.4` each showed `No work summary reported`. The exact placeholder phrase `Ask Codex to do anything` occurred zero times.
- **Replay action:** Gracefully stopped `%6` with `C-c`, then restarted the final reviewed Harold against the same fixture configuration and durable store. The replayed isolated process had PID `29008`.
- **Replay observation:** Dashboard revision 604 again contained zero occurrences of the exact placeholder phrase, and all four legacy rows remained repaired with `No work summary reported`.
- **Isolation:** The deployed listener on port `50060` was untouched throughout both accepted runs.
- **Result:** Pass for the final reviewed implementation, startup-before-publication ordering, atomic all-pane durable repair, exact-match fallback, exact-fixture isolation, and repair persistence across graceful same-store replay. This supersedes ST-030 for acceptance.

## Required live tests not yet run

These are explicitly **not complete**:

- Authenticate Claude Code interactively in `%33`, then run its controlled non-mutating task and capture its real busy-to-idle transition, provider/pane identity, and current substantive work summary. Codex and OpenCode have this sequential proof in ST-025; Claude does not.
- Repeat the controlled task with authenticated Claude, Codex, and OpenCode concurrently and prove three simultaneous busy rows settle to three idle rows without stale, inherited, or cross-provider summaries.

Each completed live test must be inserted immediately before this final section with its exact commands, pane/client identities, dimensions, captured observable result, and pass/fail outcome. Unit or fake-runner coverage must not be substituted for live evidence.
