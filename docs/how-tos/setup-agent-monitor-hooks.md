# Set Up Agent Monitor Hooks

To add explicit busy/idle state and work summaries to Harold, configure named agent providers, run Harold, then connect each provider's lifecycle events to `ReportAgentState`. Existing stop hooks can continue calling `TurnComplete` for notifications.

## Prerequisites

- Harold, tmux, and `grpcurl` satisfy the [project prerequisites](../prerequisites.md).
- The agent runs inside tmux and inherits `TMUX_PANE`.
- `harold-api/proto/harold.proto` is available to `grpcurl`.
- Harold's gRPC listener remains on a trusted loopback address unless you provide a separate authenticated transport boundary.
- You know the executable fragments and visible-screen markers for any custom provider.

This procedure changes configuration and hook registration. It does not run `make deploy`, install a daemon, or restart a deployed Harold instance for you.

## 1. Configure named providers

Keep the shipped providers in `config/default.toml` or override the complete named list in `config/local.toml`. A custom provider has this shape:

```toml
[agent_monitor]
inventory_interval_ms = 1000
screen_interval_ms = 500
hook_grace_ms = 2000

[[agents]]
id = "custom-agent"
display_name = "Custom Agent"
command_contains = ["custom-agent"]
busy_all = ["Working"]
idle_all = ["Ready"]
summary_line_prefixes = ["> "]
```

Use marker text observed in the current visible terminal grid. Every `busy_all` fragment must match for Busy, and every `idle_all` fragment must match for Idle. If both clauses match, Busy wins. Omit `summary_line_prefixes` when a visible prefix cannot safely distinguish submitted work from a composer or placeholder.

Do not configure `id = "unknown"`; it is reserved. IDs must match `[a-z0-9][a-z0-9._-]{0,63}`.

The old presence-only form remains loadable but cannot select provider-specific screen behavior:

```toml
[agents]
command_contains = ["claude", "codex"]
```

Migrate to `[[agents]]` before relying on visible-screen state or fallback summaries.

## 2. Start or restart Harold with the configuration

For a repository run, point Harold at the repository configuration and start it:

```sh
HAROLD_CONFIG_DIR="$PWD/harold/config" cargo run --offline -p harold
```

For an installed copy, update its `config/local.toml` and restart it using your existing operating procedure. `make deploy` builds, copies, signs, and restarts the installed binary; run it only when you intend to deploy.

Harold projects the durable stream to its head and seeds the current snapshot before it listens for gRPC requests.

## 3. Preserve completion hooks where notifications are required

`ReportAgentState` does not replace `TurnComplete` notifications. Keep the Claude or Codex stop adapter described in [Setup](setup.md#4-install-agent-stop-hooks) when you need TTS or away notifications.

The shared notifier installed by `make deploy` is `~/bin/harold/hooks/harold_turn_complete.py`. Provider-specific Claude and Codex transcript adapters live in their respective user configuration directories; they are not copied from this repository by `make deploy`.

A stop adapter must select the most recent substantive submitted user instruction for `last_user_prompt`. A normalized-empty legacy prompt preserves the explicit candidate and its timestamp. The legacy RPC cannot explicitly clear a summary.

## 4. Connect lifecycle events

A lifecycle adapter calls `ReportAgentState` with:

- the current `TMUX_PANE`, such as `%22`;
- `AGENT_STATE_BUSY` or `AGENT_STATE_IDLE`;
- a bounded adapter identifier;
- an optional `workSummary`.

Use a JSON encoder and pass the request on standard input so prompt content does not appear in process arguments. This fixed-value command demonstrates the wire call:

```sh
proto_dir=/path/to/harold/harold-api/proto
grpcurl -plaintext \
  -max-time 3 \
  -import-path "$proto_dir" \
  -proto harold.proto \
  -d @ \
  localhost:50060 \
  harold.Harold/ReportAgentState <<'JSON'
{
  "paneId": "%22",
  "state": "AGENT_STATE_BUSY",
  "adapterId": "custom.lifecycle",
  "workSummary": "Review the projector"
}
JSON
```

Replace `%22` with the adapter's inherited `TMUX_PANE`; do not hard-code it in a real hook. Send Busy when submitted work starts and Idle when the provider reports completion. On an Idle report, omit `workSummary` to preserve the explicit candidate and its timestamp:

```json
{
  "paneId": "%22",
  "state": "AGENT_STATE_IDLE",
  "adapterId": "custom.lifecycle"
}
```

Presence matters:

- omit `workSummary` to preserve the explicit candidate;
- send a present empty string to clear it and reveal any screen fallback;
- send a present non-empty string to replace it after normalization, unless it exactly equals a normalized configured idle placeholder; an exact placeholder preserves the candidate.

The RPC succeeds only when inventory resolves the pane to a live configured agent incarnation. Harold, not the hook, supplies the provider and process identity.

## 5. Enable the opt-in OpenCode plugin

The repository's OpenCode plugin maps `chat.message` to Busy plus the most recent substantive submitted text, and maps `session.status`/`session.idle` to Busy or Idle. It queues reports in lifecycle order and fails open if Harold is unavailable.

It is not installed by `make deploy`. To enable it for one OpenCode process without changing global configuration:

```sh
project=/path/to/project
harold_repo=/path/to/harold
cd "$project"
HAROLD_ADDR=127.0.0.1:50061 \
HAROLD_PROTO="$harold_repo/harold-api/proto/harold.proto" \
OPENCODE_CONFIG_CONTENT="{\"plugin\":[\"file://$harold_repo/hooks/opencode/harold-plugin.js\"]}" \
opencode .
```

Run this isolated example inside the tmux pane to preserve `TMUX_PANE`. Port `50061` keeps the acceptance process separate; OpenCode's normal default remains `localhost:50060`. If `OPENCODE_CONFIG_CONTENT` already contains settings, merge the plugin entry rather than replacing the existing JSON. Do not use `opencode --pure`, which disables external plugins.

OpenCode's shipped provider configuration has busy/idle screen markers but no fallback-summary prefix. Explicit plugin summaries work; visible-screen fallback summaries do not. The plugin reports lifecycle state only and does not send `TurnComplete` notifications.

The plugin removes complete ESC and C1 CSI, OSC, DCS, SOS, PM, and APC terminal sequences before it collapses whitespace, rejects placeholders, and applies the 160-scalar bound. Harold applies its own sanitizer again at durable ingress.

## 6. Verify the snapshot stream

Start a watcher in a second terminal:

```sh
proto_dir=/path/to/harold/harold-api/proto
grpcurl -plaintext \
  -import-path "$proto_dir" \
  -proto harold.proto \
  -d '{}' \
  localhost:50060 \
  harold.Harold/WatchAgentStates
```

The first response is a complete snapshot. Confirm that the target pane includes all five incarnation fields—`paneId`, `panePid`, `agentPid`, `agentStartedAtMs`, and `providerId`—plus `state` and, when available, `workSummary`.

Submit a distinctive prompt through the configured agent and verify this sequence:

1. Busy appears with the normalized summary.
2. Idle appears after completion while the summary remains present.
3. Stopping and reconnecting the watcher immediately returns the current complete snapshot.
4. Restarting Harold returns the same projected state before later observations arrive.

To verify explicit clear behavior, send a lifecycle request whose `workSummary` is present and empty. The public summary should change to the current screen fallback or become absent. A dashboard must render an absent summary as exactly `No work summary reported`; `grpcurl` itself simply omits the optional field.

For OpenCode, run the repository tests without installing packages:

```sh
node --test hooks/opencode/harold-plugin.test.mjs
```

## 7. Run real-provider dashboard acceptance

Fixtures and fake provider processes do not satisfy this check. Use the real installed Claude Code, Codex, and OpenCode CLIs, their real hook/plugin paths, the real `WatchAgentStates` stream, and the dashboard that consumes it.

1. Build the ready revision and create a disposable state directory:

   ```sh
   cargo build --offline --workspace --release
   acceptance_dir=$(mktemp -d /private/tmp/harold-provider-acceptance.XXXXXX)
   ```

2. Start a temporary Harold instance with a known-good configuration, the disposable store, and isolated port `50061`. Do not point this run at the normal Harold store:

   ```sh
   HAROLD_CONFIG_DIR="$PWD/harold/config" \
   HAROLD__GRPC__PORT=50061 \
   HAROLD__STORE__PATH="$acceptance_dir/events" \
   target/release/harold >"$acceptance_dir/harold.log" 2>&1 &
   acceptance_harold_pid=$!
   ```

   Use a temporary config overlay if the repository config is not already valid for the local machine. This command starts an acceptance instance; it does not deploy or replace the installed Harold binary.

3. Start the real snapshot watcher and point the dashboard at the same endpoint:

   ```sh
   proto_dir="$PWD/harold-api/proto"
   grpcurl -plaintext \
     -import-path "$proto_dir" \
     -proto harold.proto \
     -d '{}' \
     127.0.0.1:50061 \
     harold.Harold/WatchAgentStates
   ```

4. Run real providers in isolated tmux panes: Claude Code in `%33`, Codex in `%34`, and OpenCode in `%35`. Point every lifecycle adapter at `127.0.0.1:50061`. Launch OpenCode process-locally so the acceptance run does not modify global configuration:

   ```sh
   project=/path/to/opencode-acceptance-project
   harold_repo=/path/to/harold
   cd "$project"
   HAROLD_ADDR=127.0.0.1:50061 \
   HAROLD_PROTO="$harold_repo/harold-api/proto/harold.proto" \
   OPENCODE_CONFIG_CONTENT="{\"plugin\":[\"file://$harold_repo/hooks/opencode/harold-plugin.js\"]}" \
   opencode .
   ```

5. Submit a different controlled task to each real provider. Give each task unique visible wording and enough duration to observe Busy. In the dashboard and watcher, require all of the following:

   - each row carries the expected pane, provider, agent PID, and process start time;
   - every provider changes from Busy to Idle;
   - each Busy row shows its own current task, with no summary copied from another pane;
   - Idle placeholder or summary absence retains that pane's current substantive summary;
   - a later substantive Busy screen prompt replaces an older completion candidate;
   - an exact normalized placeholder is rejected, while a real prompt that merely mentions the placeholder phrase remains valid.

6. Stop one real provider, wait for two complete inventory scans plus exact-incarnation revalidation, and confirm its row departs. Relaunch that provider in the same pane and confirm the row has a new incarnation and initially inherits no prior state or summary. Submit new work and verify only the new task appears.

7. Stop the temporary Harold instance and every provider process created for the run. Confirm the watcher closes, no process still listens on `127.0.0.1:50061`, and the acceptance path is the exact disposable directory before removing it.

### Recorded acceptance outcomes

The real-provider run at ready commit `d9a55ea` on `127.0.0.1:50061` produced this bounded result:

| Check | Outcome |
| --- | --- |
| Codex `%34` and OpenCode `%35` simultaneous Busy with distinct summaries | Passed at dashboard revision 571 |
| Codex `%34` and OpenCode `%35` both Idle while retaining their own summaries | Passed at revision 577, with no cross-talk |
| OpenCode `%35` departure | Passed at revision 578 |
| Process-local OpenCode `%35` rejoin as a new incarnation with no prior-incarnation leakage | Passed at revision 580 |
| Claude Code `%33` sequential acceptance | Not passed: the real CLI displayed `Login expired` / `run /login` |
| Three-provider `%33`/`%34`/`%35` concurrency | Not passed because Claude authentication blocked `%33` |

Do not treat the two-provider result as proof of Claude behavior or three-provider concurrency. Re-run those two checks after authenticating the real Claude Code process.

A later same-store run served commit `c27cd89` on the same isolated address. Projection-only repair events 591 through 594 cleared legacy exact configured idle-placeholder screen candidates for panes `%1`, `%13`, `%14`, and `%23`; the dashboard was clear by revision 594. After replay and restart against that same event store, those repaired candidates remained clear. This verifies durable repair of those historical rows, not the still-blocked Claude or three-provider checks.

Corrective commits `7523735` and `1568fd0` subsequently changed startup and ingress handling after independent review. After current-code completion review and fresh offline gates, `1568fd0` was launched from `%6` with the same fixture configuration and event store. Its first snapshot at event version 602 contained 10 rows and zero exact configured-placeholder summaries. A graceful `%6` `C-c` restart against that same store produced a replay snapshot at event version 604 with the same 10 rows and zero exact placeholders; dashboard pane `%26` also showed revision 604 with no placeholder occurrence. This proves current-code startup and replay for the existing store. It does not satisfy the still-blocked Claude or three-provider checks.

## Troubleshooting

### `INVALID_ARGUMENT`

Use a pane ID in `%` plus decimal-digits form, use only Busy or Idle, and ensure the adapter ID matches `[a-z0-9][a-z0-9._-]{0,63}`.

### `FAILED_PRECONDITION: agent incarnation not found`

Confirm the agent is currently running in that pane and its executable command matches exactly one named provider's `command_contains` fragments. The hook cannot register an agent that inventory cannot resolve.

### `UNAVAILABLE: agent state report unavailable`

Check Harold's logs and the `monitorHealth` entries. Inventory or event append may be unavailable, or the serialized monitor runtime may have stopped. Retry only after the underlying failure is corrected; an unavailable response does not claim durable acceptance.

### State remains `Unknown`

Confirm named configuration is active rather than deprecated `[agents].command_contains`. Then compare the current visible grid with every case-sensitive `busy_all` or `idle_all` fragment. Harold deliberately preserves uncertainty when neither full clause matches.

### Work summary is absent

Confirm the lifecycle event sends a present substantive `workSummary`. For screen fallback, confirm the provider has a safe `summary_line_prefixes` entry. OpenCode intentionally has no fallback prefix. An exact normalized configured idle-placeholder value is rejected and does not replace the prior screen candidate. A substantive prompt that merely contains the placeholder phrase remains valid.

### Watch exits during shutdown

This is expected. Harold closes every open `WatchAgentStates` stream during graceful shutdown. Reconnect after Harold has restarted to receive the current stored snapshot.

See the [agent-monitor reference](../references/agent-monitor/README.md) for the complete event, RPC, failure, and privacy contracts.
