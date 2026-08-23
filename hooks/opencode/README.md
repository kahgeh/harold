# OpenCode lifecycle adapter

This opt-in OpenCode plugin reports the current tmux pane's lifecycle to
Harold without changing the user's global OpenCode configuration.

## Inputs and boundaries

- `chat.message` is the only work-summary input. The adapter selects the most
  recent substantive submitted user `TextPart`; it does not concatenate
  turns or inspect the current composer. Empty, synthetic, ignored and known
  UI-placeholder parts are skipped. The selected text has controls and excess
  whitespace removed and is capped at 160 Unicode scalar values. The adapter
  does not invent fallback copy such as `Ask OpenCode to do anything`.
- The generic `event` hook maps `session.status` (`busy`, `retry`, `idle`) and
  the legacy `session.idle` event to lifecycle observations. Idle reports do
  not contain `workSummary`, so they preserve the last task description.
- `TMUX_PANE` identifies the pane. If it or a session ID is unavailable, the
  adapter does nothing.
- Harold resolves `provider_id=opencode` from its configured tmux inventory.
  `ReportAgentStateRequest` deliberately has no provider field that an
  adapter could spoof; this plugin identifies itself with the bounded
  `adapter_id` `opencode.lifecycle`.

Reports are queued in lifecycle order, but an OpenCode hook does not wait for
the RPC. `grpcurl` is launched directly with `shell: false`; the JSON request
is written to stdin rather than exposed in process arguments. Output is
discarded, failures are fail-open, and prompt content is never logged.

## Isolated setup

The installed OpenCode 1.18.15 discovers JavaScript modules under either
`.opencode/plugin/` or `.opencode/plugins/`. Its project loader discovers the
`.js` entrypoint (not `.mjs`) and treats every exported function as a plugin,
so `harold-plugin.js` intentionally exports one function while the tested
helpers remain in the non-discovered core module.

For an isolated run, inject the checked-in entrypoint into only that OpenCode
process. This avoids global configuration changes and also avoids OpenCode's
automatic `.opencode/node_modules` bootstrap:

```sh
project=/path/to/project
cd "$project"
HAROLD_ADDR=127.0.0.1:50060 \
HAROLD_PROTO=/Users/kahgeh/Dev/p/harold/harold-api/proto/harold.proto \
OPENCODE_CONFIG_CONTENT='{"plugin":["file:///Users/kahgeh/Dev/p/harold/hooks/opencode/harold-plugin.js"]}' \
opencode .
```

`HAROLD_ADDR` defaults to `localhost:50060`. When this repository copy of the
plugin is used, `HAROLD_PROTO` defaults to the checked-in proto beside it.
Pass `HAROLD_PROTO` explicitly if the plugin is copied elsewhere. Do not use
`opencode --pure`, because that intentionally disables external plugins.
If `OPENCODE_CONFIG_CONTENT` is already in use, merge this single `plugin`
entry into that process-local JSON rather than replacing the existing value.

## Isolated pane `%35` live run

For the current dashboard acceptance project:

```sh
project=/private/tmp/tmx-agent-dash-e2e-20260823/opencode
cd "$project"
HAROLD_ADDR=127.0.0.1:50060 \
HAROLD_PROTO=/Users/kahgeh/Dev/p/harold/harold-api/proto/harold.proto \
OPENCODE_CONFIG_CONTENT='{"plugin":["file:///Users/kahgeh/Dev/p/harold/hooks/opencode/harold-plugin.js"]}' \
opencode .
```

Run those commands inside pane `%35`, so OpenCode and its plugin inherit
`TMUX_PANE=%35`. Submit a distinctive real prompt. The expected sequence is:

1. `chat.message` reports BUSY with that normalized prompt as `workSummary`.
2. `session.status` may repeat BUSY; the adapter suppresses the duplicate.
3. `session.status: idle` or `session.idle` reports IDLE with `workSummary`
   absent, leaving the displayed summary unchanged.

Before the live run, this read-only command verifies that the installed CLI
discovers and loads the isolated entrypoint without creating project files:

```sh
cd "$project"
OPENCODE_CONFIG_CONTENT='{"plugin":["file:///Users/kahgeh/Dev/p/harold/hooks/opencode/harold-plugin.js"]}' \
opencode debug info
```

The `plugins:` section must list `harold-plugin.js`, with no `failed to load
plugin` message.

## Tests

The suite uses only Node's built-in test runner and does not install anything:

```sh
node --test hooks/opencode/harold-plugin.test.mjs
```

It covers prompt normalization, JSON optional-field presence, lifecycle event
mapping, multi-session state, argv safety, repository proto fallback,
fail-open behavior, and missing-context no-ops.
