# Portable Harold installation

## Goal

From a checkout on another Mac, one command installs Harold and its dashboard,
collects or imports machine settings, starts a per-user service, and proves that
the installed service is ready. A reinstall starts with fresh state. Existing
schema compatibility is not required.

## User workflow

```sh
git clone --recurse-submodules https://github.com/kahgeh/harold.git
cd harold
./scripts/install.sh
# Rebuild/reinstall while retaining configuration and starting fresh state:
./scripts/install.sh --reinstall
```

The user explicitly selected manual startup, with no automatic login startup.
Use a per-user launchd service loaded on demand by `haroldctl start`; its plist
lives inside the installation, not in an auto-loaded LaunchAgents directory.
The installer runs as the current user, without sudo. It checks prerequisites and
prints actionable missing-tool instructions; it does not automatically install
third-party dependencies. Cargo builds use the existing lockfile; `--offline`
supports an already-cached checkout.

## Contract

- `scripts/install.sh` launches a Python standard-library installer (Python 3.9+).
- Options: `--reinstall`, `--config PATH`, `--prefix PATH` (default `~/bin`),
  `--signing-identity ID` (ad-hoc `-` by default), and `--offline`.
- First install prompts for iMessage recipient/handle IDs or Telegram token/chat ID
  unless `--config` supplies a local TOML file. Tokens are entered without echo and
  never printed. Optional Claude features remain off until explicitly configured.
- Existing `local.toml` is retained unless `--config` supplies a replacement.
- Install checks macOS, Python, Rust/Cargo, tmux, grpcurl, protoc, codesign,
  launchctl, plutil and lsof, and the checked-out events submodule.
- Build both release binaries, stage assets, sign and validate configuration
  before disrupting the existing process. Do not claim signing grants macOS privacy
  permissions or proves remote-channel delivery.
- Installed layout: `<prefix>/harold/{harold,harold.proto,hooks/,config/,service.py,
  service.json,data/events/,harold.log}`, `<prefix>/tmx-agent-dash`, and
  `<prefix>/haroldctl`. Preserve established default hook/binary paths.
- The managed service owns its config directory, `local` overlay, store at
  `<prefix>/harold/data/events`, PATH and UTF-8 locale. Clear inherited HAROLD
  overrides before adding these explicit settings. Probe and launch use the same
  environment. Store override is intentional, documented, and prevents external
  or copied machine-specific paths from becoming reinstall deletion targets.
- The launchd service label is stable for an install prefix, unique for custom test
  prefixes. Its plist is written inside the bundle with a structured serializer,
  with absolute paths. The installer starts it once for readiness verification;
  subsequent login sessions do not automatically load it.
- `haroldctl start|stop|restart|status` operates on that service only. Startup is
  idempotent, shutdown bounded, and no broad process-name kill is permitted.
- Stop an existing unmanaged installed daemon only after attributing the listener
  PID to the exact installed executable. An unrelated listener is an error.
- Normal install retains managed data. `--reinstall` archives the old installation
  (including data and configuration) and creates fresh managed data; it never
  recursively deletes an arbitrary configured path. Reject unsafe/symlinked target
  paths before mutation. Backups have unique names and are reported.
- A startup failure returns nonzero and identifies the log/config/control path.
  Do not leave a known-broken service in a repeated restart loop.
- Readiness requires the LaunchAgent's PID to own the listening socket and a
  successful initial `WatchAgentStates` snapshot from that endpoint. Probes must
  not call notification/diagnostic RPCs or display pane contents.
- Shared hooks request startup through `haroldctl`; they never spawn a competing
  daemon. Installed hooks resolve paths relative to their installed module.
- Provider-specific lifecycle/transcript hook registration and macOS privacy
  permissions remain explicit setup steps documented with links; do not invent
  absent provider adapters or overwrite existing provider configuration.
- Existing `make deploy` routes through the same installer preserving data;
  `make install` and `make reinstall` expose the two operations.

## Read-only daemon CLI

`harold --check-config` uses normal typed configuration loading and validation,
prints one JSON object `{ "grpc_addr": "127.0.0.1:50060", "store_path": "..." }`,
and exits without opening storage or starting tasks. Do not output secrets.

`harold --check-ready` uses that same configuration and a bounded gRPC client to
receive one `WatchAgentStates` snapshot, prints a compact success JSON object,
and exits. It neither appends events nor prints the snapshot's pane contents.
No new RPC or dependencies are required.

## Failure and verification requirements

| Trigger | Required result |
| --- | --- |
| Missing tools/submodule, invalid flags/config, build/sign failure | Exit before stopping/replacing an existing install |
| No input terminal and no usable configuration | Explain `--config`, exit without prompting indefinitely |
| Supplied config has credentials, quotes, or spaces | Preserve TOML safely; never interpolate through a shell |
| Inherited Harold overrides or non-UTF-8 locale | Managed launch and probes use controlled consistent environment |
| Symlinked destination or unsafe managed storage | Reject before archive, replacement, or removal |
| Port belongs to an unrelated process | Fail without killing it or declaring success |
| Existing current store, normal install | Preserve stored state |
| Reinstall | Archive existing managed data, start empty state with retained settings |
| Service fails readiness | Nonzero exit, bounded cleanup, actionable log path |
| Hook fires during startup | One launchd-owned daemon; hook waits for managed startup |

Tests use temporary prefixes and standard-library process fakes for failure paths.
Live verification uses a unique LaunchAgent/prefix and loopback port, real built
Harold, no configured agents, and a nonexistent fixture Messages DB; it sends no
notifications. Verify initial install, preserved-state install, fresh reinstall,
exact process/listener ownership, and cleanup of that test service. Do not touch
the production install while testing.
