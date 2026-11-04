# Operation

Harold runs as a per-user service on macOS. It starts when you log in, and launchd restarts it after a crash, within limits. On Linux it runs as a systemd user service, as [Linux and the systemd user service](#linux-and-the-systemd-user-service) describes. The installer provides `haroldctl` to start, stop, restart, and check that service. `haroldctl stop` keeps it off, across logins too, until `haroldctl start`.

The `imessage` and `calls` features are chosen when Harold is built. `harold --build-features` prints the features of the installed build, one on each line.

## Problem

A listening port alone does not tell you which Harold process owns it or which configuration and database it uses. Starting a second daemon from a hook can also race the first process. Installation, login, crash restarts and hook startup therefore use one service owner, one launchd job, and the same readiness check. That job keeps three properties:

- **Two Harolds never race.** Login, launchd's restarts and `haroldctl` all start the same job, with one label, and the store's `harold.lock` refuses a second process.
- **A hook never starts a second Harold.** A hook asks `haroldctl ensure`, which starts that job or nothing.
- **Harold never runs with the wrong configuration or store.** The job carries the managed environment and store path, whichever of them starts it.

## Architecture

The installer writes one launchd job definition, `service.plist`, and puts it in two places: inside the Harold bundle (`~/bin/harold/service.plist`), for reference and rollback, and in `~/Library/LaunchAgents/<label>.plist`, which launchd loads into the user's graphical domain at every login. The label is `com.harold.agent.` followed by 16 hex digits of the prefix's SHA-256. launchd starts the `harold` binary itself, with the environment, working directory and log file the plist names.

| Plist key | Value | Effect |
| --- | --- | --- |
| `RunAtLoad` | true | launchd starts Harold when it loads the job at login |
| `KeepAlive` | `{ SuccessfulExit = false }` | launchd starts Harold again after an exit other than 0 |
| `ThrottleInterval` | 30 | launchd waits at least 30 seconds between starts |
| `EnvironmentVariables` | the managed environment plus `HAROLD_LAUNCHD=1` | tells Harold that launchd started it (below) |

| Component | Responsibility |
| --- | --- |
| Installer | Build, stage, sign, validate, replace files, and verify startup |
| `haroldctl` / `service.py` | Serialize control operations and manage the exact launchd service |
| gRPC server | Accept hook reports and stream current agent snapshots |
| Agent monitor | Resolve agent identity and record lifecycle/screen observations |
| Event handler | Project state, stage delivery work, and deliver turn-end updates and replies |
| Channel listener | Collect incoming messages for the event stream |

The runtime components share Harold's store and shutdown signal. See [Architecture: Harold](../../explanations/architecture.md) for their data flow.

## macOS permissions

Harold with the iMessage channel needs two macOS permissions: Full Disk Access, to read `~/Library/Messages/chat.db`, and control of Messages, to send. macOS charges a permission to the program launchd started, which is `harold`. It stores each grant with `harold`'s designated requirement, the rule `codesign -dr -` prints. The installer signs `harold` and `control-tower` with the fixed identifiers `harold` and `control-tower`, so a build signed with the same certificate keeps both grants; an ad-hoc build does not. [Keep permissions across installs](../../how-tos/setup.md#keep-permissions-across-installs) has the steps.

`haroldctl` stays a Python script and needs neither permission: `start`, `stop`, `restart`, `status` and `ensure` do not read the Messages database or send to Messages.

## Installation commands

Run these from the repository:

| Command | Behavior |
| --- | --- |
| `./scripts/install.sh` | Install or replace programs; retain existing local settings and managed data |
| `./scripts/install.sh --reinstall` | Archive the old bundle; retain settings and create fresh managed data |
| `--config PATH` | Import local TOML settings instead of retaining or prompting for them |
| `--prefix PATH` | Set executable directory; defaults to `~/bin` |
| `--signing-identity ID` | Sign with this keychain identity and save it for later installs; defaults to the saved identity, else ad-hoc `-` |
| `--offline` | Build with cached locked Cargo dependencies |

`make install` and `make deploy` call the normal installer. `make reinstall` selects fresh storage. Pass installer flags using `INSTALL_ARGS`. Missing prerequisites and invalid configuration stop installation before the running service is replaced. A readiness failure returns nonzero and reports where to find the installed log and configuration.

Reinstall operates only on the managed bundle. It does not convert database schemas or remove an external store mentioned in a copied configuration. Backups are named uniquely and their locations are printed. A normal install moves retained managed data into the new bundle; its archived previous bundle is not a second copy of that data.

## What runs where in a release

Three places run tests and builds. Each has one job.

| Place | What it runs |
| --- | --- |
| Push checks | On each push to `main`, `linux-tests.yml` and `macos-checks.yml` run the tests and the feature mixes. They use a cache of the compiled dependencies: a restore step before the build and a save step after it. The save step runs even when the build step fails or times out. The `Test` step has a limit of 40 minutes. The `Check the feature mixes` step has a limit of 30 minutes. |
| The Mac | `make release` runs only the timing tests: `cargo test --workspace --locked --features harold/timing-tests,control-tower/timing-tests`. They need the real Mac. |
| Release run | `build.yml` runs on the published release. It only builds, checks the binaries and uploads them. It uses no cache, so a published binary holds nothing from an old cache. |

The trust rule: `make release` goes on only when HEAD equals `origin/main` and is the commit released. Both `linux-tests.yml` and `macos-checks.yml` must have a `push` run for that exact sha with the conclusion `success`.

Any other conclusion (`failure`, `cancelled`, `skipped`, `timed_out`, `neutral`) counts as failed. A run still in progress stops the release. A commit with `[skip ci]` gets no run and stops as missing. Each stop names the run or the missing workflow, and makes nothing public.

The release run does not test the commit again. See [ADR 33](../../adr/0033-a-release-trusts-the-checks-of-each-push.md).

## Release attempts

A **release attempt** is one run of `make release`. At its end, `make release` records the attempt with the [telemetry tool](../tools/telemetry.md). Use it to see how long a release takes and how often it fails.

An attempt has five steps. Each step has a time.

| Step | What it holds |
| --- | --- |
| `checks` | The checks before the tests, with the push checks of the commit (Linux and macOS) |
| `local_tests` | The local test suites |
| `publish` | The release notes and the pre-release on GitHub |
| `github_build` | The wait for the GitHub build |
| `install` | The mark as latest, and the install on this Mac |

The result of an attempt is `ok`, `failed` or `stopped`. A step that raises an error gives `failed`. An interrupt gives `stopped`. The step is the one that raised the error or ran when the interrupt came. An `ok` attempt has the step `none`. Every exit path of `make release` records.

| Signal | Type | Attributes | Series |
| --- | --- | --- | --- |
| `harold.release.attempts` | counter | `result` and `step` (the five steps and `none`) | 18 |
| `harold.release.seconds` | counter | `result` and `step` (the five steps) | 15 |

Each attempt sends both declarations. The tool accepts the same declaration again with no change. Then it counts one attempt. Then it counts the seconds of each step that ran. A step that takes less than half a second records 1 second.

A failure to record never changes the result or the exit code of `make release`. All the calls share a budget of 20 seconds. Each call has a limit of 5 seconds. A call that fails runs once more after 5 seconds, if the budget allows. After a second failure, or when the budget is spent, the other calls do not run. Then `make release` prints one line to stderr:

```
release attempt not recorded: <reason>
```

A missing `harold` command (exit code 127) is not tried again. A second Ctrl-C or a signal during the recording stops the recording. The exit code stays the code of the release.

`make release` runs in Harold's folder or in the chief's pane. Both may write the `harold.` prefix. `make install-release` records nothing.

## Configuration and environment

The daemon loads configuration in this order:

1. `config/default.toml`, required.
2. `config/local.toml`, optional user settings for an installed service.
3. `HAROLD__<SECTION>__<KEY>` environment overrides.

For a directly run binary, `HAROLD_CONFIG_DIR` selects the directory and `HAROLD_ENV` selects the overlay name. By default, configuration is adjacent to the executable.

The installed service controls its own config directory, `local` overlay, PATH, UTF-8 locale, and store at `<prefix>/harold/data/store`. `haroldctl` and the installer clear inherited Harold settings overrides before applying that environment to their configuration and readiness checks, so they stay pointed at the installation even when the invoking shell contains development overrides. launchd gives the daemon the same environment from the plist. A variable set for all of launchd with `launchctl setenv` also reaches the daemon; the plist's own variables win on a clash.

`service.json` records the service identity, its environment, and the signing identity the installer used. Channel credentials belong in `config/local.toml`, which the installer writes with owner-only permissions. Edit that file and run `haroldctl restart` to apply changes.

## The store

Harold keeps everything it records in one folder: `<prefix>/harold/data/store/` for the installed service, and `[store] path` for a directly run binary. The folder holds four entries:

| Entry | What it holds |
| --- | --- |
| `log/` | The event stream. Each event, such as a reply arriving or an agent finishing a turn, is one record in an OpenData log under the key `harold/main`. Its value is a JSON envelope with the event's `id`, `type`, `payload`, `created_at_ms`, `trace_id`, `span_id` and `actor`. The log stores its data with SlateDB on the local filesystem, under the prefix `harold` inside `log/`. Harold sets no retention, so the log keeps every event. |
| `log-settings.toml` | The log's SlateDB settings. Harold writes this file each time it opens the store, with no settings in it, so SlateDB's defaults apply. Neither an edit to this file nor a `SlateDb.toml` in the working directory changes the log. |
| `harold.lock` | A lock Harold holds while it runs. A second Harold started on the same folder stops at start with `store is in use by another Harold`. macOS releases the lock when Harold exits, including when it is killed. |
| `harold-state.db` and `harold-state.db-wal` | The state database, opened with turso 0.8.3. Its five data tables hold the agent panes and agent monitor health, the delivery outbox, the settings (mute, pause and each timed prompt's last day), and the checkpoint: the version of the last event projected into these tables. |

An event's version is its log sequence number plus one, so the first event in an empty store is version 1 and version 0 means no event yet. Versions only rise, but they have gaps, because the log reserves sequence numbers in blocks each time it opens: in a store whose first event is version 1, the first event after a restart is version 4097. `harold --check-ready` prints `through_event_version` in these numbers.

Harold delivers only events that the log has written to the filesystem. Each append returns after the log's `flush()` has written the record to the filesystem, and Harold opens the log so that its reads return only flushed records (`ReadVisibility::Remote`). A record the log has not flushed is therefore never projected or delivered. Flushed records survive Harold being killed. The log's local store does not call `fsync`, so whether they survive a power loss is untested.

On a disk fault, such as a full disk, Harold marks its store failing within 30 seconds and carries on without it until the disk recovers. A write to the disk can hang: SlateDB retries a failed write to the filesystem with no limit, 100 ms to 1 s apart, so the log's `flush()` does not return until the write succeeds. Harold therefore gives each append 30 seconds. An append still waiting then returns an error, and Harold marks the store failing. The delivery loop's three writes to `harold-state.db` (recording a failed try, marking a row undeliverable, marking a row delivered) mark it failing too, when one fails or takes over 30 seconds. Harold logs `store failing` at warning level, with `component = "store"` and `reason` (`write_timed_out` or `state_write_failed`). With the alert address on, it types `harold alert: store-failing firing` into the chief's pane (see the [telemetry reference](../telemetry/README.md#how-an-alert-reaches-the-chief)). On a full disk `harold.log` may not be written, but the exported copy of each line reaches telemetry over the network.

While the store is failing, Harold refuses every event-log append and every one of those three delivery writes at once, with one exception: the first write tried 30 seconds after the last try ended is let through as a new try, with the same 30-second limit. When a try succeeds, Harold logs `store recovered` at info level (`reason = try_succeeded`), types `harold alert: store-failing cleared` if it typed `firing`, and every write goes through again. Meanwhile each part of Harold that writes does this:

- The delivery loop sends nothing, and checks again every 100 ms. A message it sent but could not mark delivered is marked once the store recovers, and is not sent again. A row whose failed try or give-up could not be recorded is tried again after recovery, with its try count as it was. Once the store recovers, the loop carries on from the oldest undelivered row.
- The iMessage poll moves past a reply whose write timed out. That write may still land when the disk recovers, and the reply is then delivered once; if it never lands, the reply is lost. A reply whose write was refused keeps its place: the poll reads no later row of that direction in the same poll, so none moves past it, and reads it again at the next poll.
- The hooks get an error back, as for any store error.
- The agent monitor logs its usual failure line and carries on.

Recovery needs a write to be tried. While the store is failing, the delivery loop writes only to mark a message it already sent, so a quiet Harold, with no agent changes, hooks or replies and no such message, stays failing, with deliveries paused, until something writes. A try that writes only to `harold-state.db` can succeed while the event log still hangs; the store then recovers, and the next append that hangs marks it failing again.

Each event is projected once and has one row in the delivery outbox. Harold projects an event into `harold-state.db` and moves the checkpoint past it in one transaction, so a crash between the two rolls both back, and the next start projects that event again from the checkpoint. If the same envelope is ever written to the log twice, both copies carry one `id`, and the delivery outbox keeps one row per `event_id`, so the second copy adds no row. No other table records which ids Harold has seen. Delivery is a separate step: the event handler sends a message and then marks its row delivered, so a crash after the send and before the mark sends that message again at the next start.

Harold refuses to start on a folder whose `harold-state.db` checkpoint is past every version its `log/` can hold, as after `log/` is deleted, or replaced with an older log or another folder's log. It stops with `harold-state.db has read the event log up to version <checkpoint>, but the log in <folder>/log ends before it; the store folder is inconsistent`.

A folder with a log and no `harold-state.db` opens, and Harold builds a new `harold-state.db` from every event in the log. The new delivery outbox has nothing marked delivered, so deleting `harold-state.db` while keeping `log/` puts every stored reply, turn end, agent message and spoken line back in the outbox for delivery again.

A record whose value is not a Harold envelope does not stop the events after it. Harold reads such a record as an event of type `UndecodableEvent` and logs `stored record is not a Harold event`. The event goes to the delivery outbox like any other. The event handler has no delivery for that type, so it marks the event permanently undeliverable and logs `event is permanently undeliverable; continuing`. The events after it are projected and delivered as usual.

## Starting fresh

Builds before the OpenData log keep their store in `<prefix>/harold/data/events/`. Harold never reads or deletes that folder. When Harold opens a store folder that holds neither `harold-state.db` nor a log, it creates an empty store there, so the first start on `data/store/` keeps none of `data/events/`:

- **Event history.** It stays in `data/events/`, where only an earlier build reads it. No feature of Harold needs that history.
- **Waiting messages.** Messages still waiting in the delivery outbox of `data/events/` are not sent.
- **Mute and pause.** Both start off: voice is on and messaging is unpaused.
- **Timed prompts.** Harold counts each timed prompt whose time today has passed as handled today. It records the prompt with the reason `new_store` and logs `timed prompt marked handled (new store)` with the prompt's name. An install at 08:30 therefore does not type a 07:50 prompt a second time, and a prompt set for later in the day still runs. If the earlier Harold had not yet typed a prompt whose time had passed, that prompt is skipped for the day.

A normal install (without `--reinstall`) over a bundle that has `data/events/` and no `data/store/` keeps `data/events/` in the bundle and ends with these lines, with the bundle's own paths:

```text
New store: ~/bin/harold/data/store
Not carried over from ~/bin/harold/data/events: messages still waiting to be delivered, and the mute and pause settings (voice is on, messaging unpaused). That folder is left as it was.
```

`--reinstall` prints neither line: it moves the whole bundle, `data/events/` included, to the `harold.backup-*` folder it names.

## Rollback

To go back to a build that uses `data/events/`, install that build again with a normal install. It opens `data/events/`, which holds no event recorded only in `data/store/`. After a `--reinstall`, `data/events/` is in the `harold.backup-*` folder, not in the bundle.

When the Harold you go back to was built before 36a7c94, also run `rm -f ~/bin/harold/data/pod/tips`. An older Harold counts merges without moving that file, so the next upgrade would count them again from the old tip. Deleting it is safe: the next Harold counts from the current tip.

## Accepted dependency exceptions

The dependency audit of Harold's `Cargo.lock` accepts five advisories:

| Advisory | Crate | Why it is accepted |
| --- | --- | --- |
| RUSTSEC-2026-0194, RUSTSEC-2026-0195 | quick-xml 0.38.4 | Compiled in through object_store 0.12.5, which SlateDB uses. Harold's store is local only, so the affected code is not reached. No fixed release exists upstream. |
| Unmaintained | bincode 1.3.3, paste 1.0.15, unsafe-libyaml 0.2.11 | Notices that the crates are unmaintained. |

`opendata-log` turns on tokio's `test-util` feature, which compiles the functions that pause and advance tokio's clock into Harold. The test `no_code_outside_tests_pauses_or_advances_tokio_time` fails if code outside test files uses them.

## Start, stop, and inspect

For the default installation:

```sh
~/bin/haroldctl start
~/bin/haroldctl status
~/bin/haroldctl restart
~/bin/haroldctl stop
```

| Command | launchd calls | Result |
| --- | --- | --- |
| `start` | `enable`, then `bootstrap` from the LaunchAgents copy and `kickstart` when the job is not running | Turns start at login back on, deletes the crash file (below), and waits for readiness |
| `stop` | `disable`, then `bootout` | Stops Harold and keeps it off, at the next login too, until `start` |
| `restart` | `bootout`, then as `start` | Applies an edited `config/local.toml`; a restart that fails leaves start at login on |
| `ensure` | `bootstrap` and `kickstart` only when needed | Starts Harold unless `stop` disabled it or it gave up after a crash loop; then prints why and exits 1 |
| `status` | `print`, `print-disabled` | Prints JSON with `ready`, `pid`, `grpc_addr` and `autostart`: `on`, `off (stopped with haroldctl stop)` or `off (gave up after a crash loop)` |

`start` is idempotent. It succeeds only when the launchd service's PID owns the configured listening socket and a read-only gRPC request receives the initial `WatchAgentStates` snapshot. It does not use `TurnComplete` or the turn-end update diagnostics as a probe. An unrelated listener is an error, not a process to kill. A start that fails readiness unloads the job and leaves it enabled.

Hooks call `ensure` before sending a completion, so a Harold stopped with `haroldctl stop` stays off and the hook skips its report. They use the managed endpoint and do not spawn a daemon directly.

```mermaid
sequenceDiagram
    participant Caller as User or hook
    participant Control as haroldctl
    participant Launchd as User launchd domain
    participant Harold
    Caller->>Control: start (user) or ensure (hook)
    opt ensure, and the job is disabled or gave up after a crash loop
        Control-->>Caller: off, exit 1
    end
    opt Service is not running
        Control->>Launchd: load the LaunchAgents plist and start job
        Launchd->>Harold: start harold with the plist's environment
        Harold->>Harold: validate config, open store, catch up snapshot
    end
    Control->>Launchd: find managed PID
    Control->>Control: verify PID owns listening socket
    Control->>Harold: WatchAgentStates
    Harold-->>Control: initial snapshot
    Control-->>Caller: ready
```

## Start at login and after a crash

launchd starts Harold at login and after an exit other than 0. Some starts fail the same way every time, so restarting them would loop for ever. When launchd started it (`HAROLD_LAUNCHD=1`), Harold refuses four kinds of start: it logs `startup refused` with a `reason` and exits 0, which `KeepAlive` does not restart.

| `reason` | Cause |
| --- | --- |
| `invalid_config` | The configuration fails to load or validate |
| `store_in_use` | Another Harold holds the store's `harold.lock` |
| `store_incompatible` | The store's state database has a schema this Harold does not know |
| `crash_loop` | This is the third start within 10 minutes after an unclean exit |

The crash count lives in `<bundle>/data/crashes`, beside the store. Its first line is `running` while Harold runs, `clean` after a clean shutdown, or `gave-up`; later lines hold the times of recent starts that followed an unclean exit. A start that finds `running` counts one. At the third within 10 minutes Harold writes `gave-up`, and every later start, at login too, refuses with `crash_loop` until `haroldctl start` deletes the file. A clean shutdown resets the count.

A configuration can still hold a setting Harold does not read: a `[telegram]` section, a `[peers]` section, `notify.telegram`, a `notify.away_channel` other than `"imessage"`, or a `grpc.host` that is not a loopback address (`127.0.0.0/8`, `::1` or `localhost`). Harold names each such setting in a fixed sentence, never with its value. Under launchd it prints `invalid configuration: ` followed by the sentences before the `startup refused` line. `harold --check-config` and `--check-ready` print the sentences to stderr and exit 1, and the installer prints them and keeps the running Harold. `notify.away_channel = "imessage"` is the one that does not stop Harold: it writes one warn line, `setting no longer read`, naming the key, and the key can be removed.

Run any other way, from a terminal or as `--check-config`, Harold exits 1 on these errors. The `startup refused` line carries only fixed text and `reason`. It reaches telemetry only when the exporter has started, which is after the configuration loads; the chief cannot be alerted, because the alert address is not open yet.

The first time the installer writes the LaunchAgents copy, macOS shows a "Background Items Added" notice naming `harold`, and lists Harold in System Settings > General > Login Items & Extensions > Allow in the Background. Three ways turn start at login off:

- `haroldctl stop` keeps Harold off until `haroldctl start`.
- Switching Harold off under Allow in the Background stops launchd loading it at login; `haroldctl start` still starts it for the current login.
- Deleting `~/Library/LaunchAgents/<label>.plist` removes start at login until the next install; `haroldctl start` then loads the bundle's `service.plist` for the current login.

To roll back, run the commands in the archived bundle's `ROLLBACK.md`. Before their last command, `./haroldctl start`, they put back that bundle's LaunchAgents copy, or delete the copy when that bundle does not start at login, and run `launchctl enable gui/$(id -u)/<label>`: their first command, `./haroldctl stop`, disables the label, and an older `haroldctl start` does not enable it. A `ROLLBACK.md` written by an installer without start at login lacks those two steps; run `rm -f ~/Library/LaunchAgents/<label>.plist` and the `launchctl enable` command before its `./haroldctl start`.

## Linux and the systemd user service

On Linux, Harold runs as a stand-alone Harold. The service manager is the systemd user manager. The installer writes the unit `~/.config/systemd/user/harold.service`. It then runs `systemctl --user daemon-reload`. Then it runs `systemctl --user enable harold` and `systemctl --user start harold`.

| Unit setting | Value | Meaning |
| --- | --- | --- |
| `Restart` | `on-failure` | systemd starts Harold again after an exit other than 0 |
| `RestartSec` | 30 | systemd waits 30 seconds between starts |
| `StartLimitIntervalSec` and `StartLimitBurst` | 600 and 5 | After 5 starts in 10 minutes, systemd stops starting Harold |
| `Environment` | the managed environment plus `HAROLD_LAUNCHD=1` | Tells Harold that the service manager started it. The name stays. ADR 0019 explains why |
| `WantedBy` | `default.target` | systemd starts Harold when the user manager starts |

The `PATH` in the unit holds the folder of `rex`, `~/.local/bin`, `/usr/local/bin` and `/usr/bin`.

`haroldctl` picks the backend by the operating system. On Linux it calls `systemctl --user enable`, `start`, `stop`, `disable --now`, `show` and `is-enabled`. The readiness check is the same as on the Mac. The launchd backend does not change.

`haroldctl status` on Linux prints the same JSON as on the Mac, with two more fields:

| Field | Meaning |
| --- | --- |
| `active_state` | The unit's `ActiveState`, such as `active` or `failed` |
| `n_restarts` | The unit's `NRestarts`: how many times systemd started Harold again |

`journalctl --user -u harold` shows the start and stop lines of systemd. The lines of Harold are in `harold.log` in the installation, as on the Mac.

systemd stops the user's services when the user logs out, unless lingering is on for that user. The installer checks this and prints a warning. It never changes the setting. To keep Harold running after logout, run `loginctl enable-linger` yourself.

Not yet on Linux: speech, the web app, and the line to the phone. The installer writes `[tts] command = ""`, so a `harold speak` line is logged as not spoken. It writes `[telemetry] export = false`, because the local collector runs on the Mac. The installer needs `rex`, `grpcurl`, `lsof`, `systemctl` and `curl`. It stops when no Rex server answers.

## The slave link

A **slave** Harold reports its agents, turn ends and questions to a **master** Harold over one gRPC stream, the **slave link**. The Mac's Harold is the master. The Harold on omarchy is the slave. [ADR 34](../../adr/0034-a-slave-harold-reports-to-the-master-over-one-link.md) has the decision. The slave opens the stream and keeps it open. The master never opens it.

### Settings

| Setting | Machine | Meaning |
| --- | --- | --- |
| `[slaves] listen` | master | The address of the link listener. Loopback only. The default is `127.0.0.1:4333`. With no `[slaves]` table, the master opens no link listener |
| `[slaves] names` | master | The slaves that may connect. Each name is a label in `[agent_monitor] rex_hosts` |
| `[master] address` | slave | The master's `<host>:<port>`. With no `[master]` table, Harold is no slave |

The slave tells the master its own name: the `machine_name` setting of the slave. That name must equal a label in the master's `[slaves] names`. Harold reads these settings at start, so restart Harold after a change.

### Pair a slave, once

1. On the master, run `harold slaves pair omarchy`. It makes a 32-byte secret, the **slave pairing secret**. It keeps the secret in `slaves/omarchy.secret` in the data folder (mode 0600) and prints it once.
2. On the slave, run `harold master pair <secret> --address <the master's tailnet address and port>`. It keeps the secret in `master.secret` (mode 0600) and writes `[master] address` in `local.toml`.
3. Add `[slaves]` with `names = ["omarchy"]` to the master's `local.toml`. Restart Harold on both machines.
4. Publish the link listener on the tailnet. The chief runs this one command on the master, after the release that brings the slave link:

```sh
tailscale serve --bg --tcp 4333 tcp://127.0.0.1:4333
```

Port 4333 is the default of `[slaves] listen`. Harold never runs `tailscale` and never changes Tailscale. `tailscale serve` hands each connection to the listener as a local caller, so the secret is the only check. The listener serves `SlaveLink` and nothing else. It refuses a stream with no secret or a wrong secret before it reads a message.

To remove a slave, run `harold slaves unpair omarchy` on the master. It removes the secret, and the next connect is refused.

The tailnet path is not tested until real use. The first real run of the slave link is its first test.

### What happens while the link is up

- The master takes the slave's agents from the slave's snapshot and skips its Rex inventory of that label. Each agent keeps its host pane id, `rex:omarchy/block:<id>`, so its row does not move.
- A turn end or a question from a slave's agent goes through the master's own turn-end path. It reaches your phone as one from the Mac does.
- The master counts each report once. It keeps the last report number it applied for each slave in `slaves/<name>.json`.
- The lock of the slave's machine decides if you are at the desk or away for the slave's agents.

### What happens while the link is down

- The slave keeps working alone. It keeps each report it has not yet sent in `report-backlog.db`, for up to 24 hours or 1,000 reports. It sends the reports the master has not had when the link comes back. It drops older reports and counts them (the `slave report dropped` line).
- The master reads the slave's agents through Rex again, read-only, as before.
- Until the link comes back, a turn end or a question from the slave's agents does not reach your phone.
- A reply that you write on your phone for a slave's agent cannot be typed yet. Requests from the master to the slave are planned in 2c.2.

### A paired slave tells you nothing itself

A paired slave sends no phone message, no push, no spoken line and no todo for a turn end or a question. It records the turn in its own store and sends it to the master. Only the master delivers it. An unpaired Linux Harold keeps its stand-alone behavior.

### No recap crosses the link

The answer to the open question is No. The recap text of a slave's agent does not cross the link. The master's row shows the agent's state and step, with no recap. The slave sends no transcript path.

### When `slave-link-down` fires

The alert `slave-link-down` fires when a slave's `harold.slave_link.connected` stays 0 for 30 minutes. Do these steps.

1. Check that the slave's machine is on and awake.
2. Check that Harold runs on the slave: `harold` is up and `haroldctl status` answers.
3. Read the `slave link` lines in the slave's `harold.log`. `slave link not opened` has a `reason`: `no_secret` (the slave has no `master.secret`), `refused` (the secret does not match) or `version` (the versions differ).
4. Read the `slave link refused` and `slave link closed` lines in the master's `harold.log`. A `refused` line with `unknown_slave` means the slave's `machine_name` is not in `[slaves] names`, or the master has no secret for it.
5. Check that the `tailscale serve` command of the pairing step 4 is still active: `tailscale serve status`.
6. Pair again when the secret does not match: run `harold slaves unpair` and `harold slaves pair`, then `harold master pair`.

To silence the alert for a long time off, remove the name from `[slaves] names`. [The telemetry reference](../telemetry/README.md#slave-link) has the metrics and the log lines.

## Shutdown

`haroldctl stop` disables the job, unloads it, and waits for its process to stop within a bounded period. A clean shutdown exits 0 and marks the crash file `clean`. Within Harold, SIGINT or SIGTERM closes the shared shutdown signal, ends snapshot streams, drains in-flight RPCs, and joins the event handler and listener. The agent monitor has a bounded shutdown deadline.

An RPC that appends after the handler has stopped remains durable and is projected at the next start. Event-stream and state-database writes commit as operations complete; shutdown does not require a separate final checkpoint.

## Read-only daemon checks

| Command | Result |
| --- | --- |
| `harold --check-config` | Validates resolved configuration and prints only `grpc_addr` and `store_path` as JSON; does not open storage |
| `harold --check-ready` | Receives one snapshot within five seconds and prints `ready` and `through_event_version`; does not print pane contents or append events |
| `harold accept EVENT --pane ID --adapter NAME` | Reports one event for a pane to the running daemon. `EVENT` is `turn-started` or `still-working`, which report working, or `turn-completed` or `subagents-finished`, which report done; any other name is an argument error. Sends no work summary; exits 0 when accepted, otherwise prints one line on standard error and exits 1; waits 3 seconds at most; never starts a daemon |
| `harold get THING` | Prints one thing of the Harold state as one line of JSON, from one `GetHaroldState` call to the running daemon. See [`harold get`](#harold-get) |

These direct commands use the invoking process's normal configuration environment. `haroldctl status` runs the readiness check with the installed environment and adds process ownership verification.

## `harold get`

`harold get THING` tells the current value of one item of the Harold state. It calls `GetHaroldState` on the daemon over loopback. It reads nothing from the screen. It reads from the config file only the address of the daemon. Each value is the value Harold uses now.

| Thing | Prints on standard output | Meaning |
| --- | --- | --- |
| `voice` | `{"state":"on","engine":"server"}` | `state` is `off` when the voice mute is on, and `on` when it is off. `engine` is the TTS engine, `server` or `command`. |
| `messaging` | `{"state":"on"}` | `state` is `off` when the messaging pause is on, and `on` when it is off. |
| `version` | `{"release_version":"0.72.0"}` | Harold's release version. |
| `machine` | `{"machine_name":"laptop-mac","host_icon":"mac"}` | The machine name and its host icon. The host icon is empty with none. |
| `multiplexer` | `{"multiplexer":"rex"}` | `tmux` or `rex`. |
| `hosts` | `{"hosts":[{"label":"omarchy","host_icon":"arch"}]}` | The Rex hosts. The list is empty with none: `{"hosts":[]}`. |
| `chief` | `{"chief_session":"chief"}` | The chief's session. The value is empty with none. |

For `voice` and `messaging`, `off` always means that the switch stops the thing. Standard output holds JSON only, and only with exit 0. Standard error holds every error line.

| Case | Line on standard error | Exit |
| --- | --- | --- |
| No thing, extra words, or a name that is not in the table | `unknown thing "NAME": voice, messaging, version, machine, multiplexer, hosts, chief` | 2 |
| The daemon does not answer within 3 seconds | `the Harold daemon did not answer within 3 seconds` | 1 |
| The configuration holds an invalid daemon address | `invalid configuration` | 1 |
| No daemon on the address | `the Harold daemon is not reachable` | 1 |
| An older Harold without the call (`Unimplemented`) | `this Harold is too old for harold get` | 1 |
| Another gRPC error | `the Harold daemon refused the call` | 1 |

A script that needs the voice state runs `harold get voice`. On any exit that is not 0, the script treats the voice as possibly on. Do not read the voice state from the control-tower screen.

The answer never holds a token, key, pairing code, HyperDX key, phone number, iMessage recipient, TTS command line or path.

## Turn-end update diagnostics

`harold --diagnostics [--delay N]` explicitly tests screen-lock detection, TTS, and the selected away channel. It can speak or send a message. It runs as its own process and does not consult the messaging pause, so it sends on the away channel even while messaging is paused. A bare `--delay` defaults to ten seconds, allowing time to lock the screen.

Use diagnostics only when intentionally testing turn-end update delivery. See [installation and setup](../../how-tos/setup.md) for the manual macOS permissions and provider-hook steps that remain after the service is ready.

## Pausing messaging

Harold has a messaging pause for the away channel. While messaging is paused the daemon keeps running, and so do the agent monitor, the control-tower stream and at-desk TTS, but nothing moves over iMessage:

- no away turn-end update, confirmation or error reply is sent;
- no inbound message is routed to a pane, including messages already waiting in the delivery outbox;
- messages that arrive are discarded, not queued. The listener still moves its cursor past them, so nothing is replayed on resume;
- `harold send-message-to-away-device` is refused: it prints `messaging is paused` and exits 1. A message the daemon stored before the pause is skipped and is not sent on resume.

Use it when the away channel misbehaves, for example when Harold keeps answering its own messages. Pausing keeps the agent monitor, the control-tower stream and at-desk TTS running, which `haroldctl stop` turns off with the rest of Harold. The pause is stored as a `MessagingPausedChanged` stream event and projected into the `settings` table of the Turso state database, so it survives a restart and stays on until you resume.

From the control-tower: press `/`, type `messaging`, and run `Messaging: pause`. The masthead of every connected control-tower then shows `■ MSG PAUSED`. `Messaging: resume` turns messaging back on.

Without the control-tower, call the RPC with `grpcurl` from the repository's `harold-api/proto` directory:

```sh
grpcurl -plaintext \
  -import-path . \
  -proto harold.proto \
  -d '{"paused": true}' \
  localhost:50060 \
  harold.Harold/SetMessagingPaused
```

Send `{"paused": false}` to resume. The RPC handler flips the in-process switch itself once the event is stored, without waiting for the projection; the event handler then confirms the value from the projected snapshot, so the switch is settled within one delivery cycle. A delivery that is already being dispatched at that moment still completes. Overlapping requests are applied one at a time, in the order they are stored.

## Inventory timeout

If the control-tower reports `MONITOR DEGRADED inventory:timeout`, check
`~/bin/harold/harold.log` for the component's failure and recovery messages.
Inventory scans can take longer when macOS schedules Harold in the background.
The inventory deadline defaults to 3000 ms and can be overridden in local.toml:

```toml
[agent_monitor]
inventory_timeout_ms = 5000
```

Restart with `~/bin/haroldctl restart` after changing it. This controls process
inventory scanning and identity resolution, and the next tick's scan retries a
failed one with the same deadline; a screen capture has a fixed 500 ms deadline,
and 2,000 ms when it is retried. A worker still finishing after a timeout is not
launched again; the previous health state and panes remain until another
acquisition completes.
