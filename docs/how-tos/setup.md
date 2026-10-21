# Install or Reinstall Harold

Install Harold and the dash from the latest macOS release. This is the recommended path: it downloads ready-built programs, checks their checksum, and asks for your channel settings. Rust and a source checkout are unnecessary.

The installer starts Harold to verify installation, and launchd starts it again at every login; `haroldctl stop` keeps it off until `haroldctl start`. The first install shows a macOS "Background Items Added" notice naming `harold`: that is Harold starting at login.

## Prerequisites

- An Apple Silicon Mac running macOS 15 or newer, with an active graphical login session. Run the installer as your normal user.
- Python 3.9 or newer, tmux, and grpcurl.
- A Messages account signed into iMessage, for the away channel.

With Homebrew available, install the runtime tools:

```sh
brew install python tmux grpcurl
```

The installer checks prerequisites and reports missing tools; it does not install packages for you. It also uses macOS tools such as codesign and launchctl. See [prerequisites](../prerequisites.md) for channel requirements and optional AI features.

## 1. Install

<a id="install-a-published-release-without-rust"></a>

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh
```

This downloads the latest stable release and verifies its SHA-256 checksum before installation. To compile local changes instead, use [Build from source](#build-from-source).

## 2. Configure

On the first run, enter the iMessage recipient and handle IDs when asked. Alternatively, supply a complete local configuration file:

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh -s -- --config /path/to/local.toml
```

Without a terminal, supply `--config PATH` for the first installation. Existing configuration is retained by default.

The installer validates configuration before replacing the installed program. If `local.toml` holds a setting Harold does not read (`[telegram]`, `[peers]`, `notify.telegram`, a `notify.away_channel` other than `"imessage"`, or a `grpc.host` that is not a loopback address), the installer prints one sentence per setting, stops, and leaves the running Harold in place. Remove the named settings, then run the install again. It signs Harold with the code-signing certificate you name once, and with no certificate (ad-hoc) until you name one; see [Keep permissions across installs](#keep-permissions-across-installs).

For iMessage, Harold needs two macOS permissions: Full Disk Access, to read your replies from the Messages database, and permission to control Messages, to send. After an install, the installer says whether both carried over from the previous install or prints the steps to grant them.

The default layout is:

```text
~/bin/harold/
  harold
  harold.proto
  service.py
  service.json
  service.plist
  config/default.toml
  config/local.toml
  config/local.template.toml
  hooks/harold_turn_complete.py
  hooks/claude_turn_complete.py
  hooks/codex_turn_complete.py
  skills/send-message-to-away-device/SKILL.md
  data/store/
  data/events/     kept from an earlier install, if present
  harold.log
~/bin/haroldctl
~/bin/tmx-agent-dash
```

`local.toml` holds your channel and optional feature settings. The installed service uses its own `config` directory and its store in `data/store`, regardless of paths copied from another machine or Harold overrides in the invoking shell. It records the PATH and UTF-8 locale needed by its tools. Variables set for all of launchd with `launchctl setenv`, such as `HAROLD__GRPC__PORT`, also reach the service; the seven variables the service sets itself (`HAROLD_CONFIG_DIR`, `HAROLD_ENV`, `HAROLD__STORE__PATH`, `PATH`, `LANG`, `LC_ALL` and `HAROLD_LAUNCHD`) win on a clash.

Add `~/bin` to your shell's PATH if needed, or use the absolute commands below. The installer prints the service definition and installation paths. The service definition stays in the installation, with a copy in `~/Library/LaunchAgents` that launchd loads at login. A custom location can be selected with `--prefix /absolute/path/to/bin`.

### Keep permissions across installs

launchd starts `~/bin/harold/harold` itself, so macOS charges Full Disk Access and Messages control to `harold`. macOS stores each grant with a rule that names the program by its signature. `codesign -dr - ~/bin/harold/harold` prints that rule, the program's designated requirement:

| Harold is signed with | Rule macOS stores | After installing a new build |
| --- | --- | --- |
| No certificate (ad-hoc, `-`) | `cdhash H"…"`: a fingerprint of this exact file | The fingerprint differs, so both permissions must be granted again |
| A certificate | `identifier harold and certificate leaf = H"…"` | The name and certificate are the same, so both permissions carry over |

The installer always signs with the identifiers `harold` and `tmx-agent-dash`, so only the certificate decides.

#### Create a self-signed certificate

Once per Mac, in Keychain Access:

1. Choose Keychain Access > Certificate Assistant > Create a Certificate.
2. Enter a name, for example your own. Set Identity Type to Self Signed Root and Certificate Type to Code Signing. Click Create.
3. Check that it is listed:

   ```sh
   security find-identity -v -p codesigning
   ```

The certificate expires on the date set when it is created. A new certificate means granting both permissions once more. Other Macs do not trust it, which Harold does not need. For a second Mac, create one there the same way, or export this one from Keychain Access (File > Export, `.p12`) and import it there.

#### Name it once

Give the certificate's name on one install:

```sh
./scripts/install.sh --signing-identity 'Your Certificate Name'
make install INSTALL_ARGS="--signing-identity 'Your Certificate Name'"
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh -s -- --signing-identity 'Your Certificate Name'
```

The installer saves the name in `~/bin/harold/service.json`, and later installs use it when none is given. `--signing-identity -` goes back to signing with no certificate and saves that.

The first time, macOS may ask "codesign wants to sign using key … in your keychain". Enter your login password and click Always Allow; Allow on its own asks again at every install. The installer waits until the question is answered.

If the keychain has no certificate by that name, or two with the same name, `codesign` refuses and the install stops with `Install failed:` and `codesign`'s message, before the running Harold is touched. For two certificates with one name, pass the certificate's SHA-1 fingerprint from `security find-identity` instead of the name.

An Apple Developer ID Application certificate works the same way: pass its name. It costs US$99 a year through the Apple Developer Program. It adds trust on other Macs and Apple notarisation, which matter only if other people install Harold builds you sign.

#### After an install

With iMessage, the installer prints whether the permissions carried over. When they did not, after a first install, a new certificate, or any new build signed with no certificate, it prints these steps:

1. Full Disk Access: open System Settings > Privacy & Security > Full Disk Access. If `harold` is in the list, select it and click "−". Click "+", press Cmd+Shift+G, enter the path the installer printed (`/Users/<you>/bin/harold/harold`), press Enter and click Open. Check that the switch next to `harold` is on. Harold retries every 5 seconds, so no restart is needed: `iMessage listener started` appears in `~/bin/harold/harold.log` after the `cannot read the Messages database` line.
2. Messages: the first time Harold sends a message, macOS asks whether `harold` may control Messages. Click Allow; until then Harold cannot send. If it was answered Don't Allow, switch it on in System Settings > Privacy & Security > Automation > harold > Messages.
3. Once per Mac, after the first install that starts `harold` directly: earlier installs started Harold through Python, so macOS charged both permissions to Python. In Full Disk Access, switch `python3.13` off, or select it and click "−" if nothing else on the Mac needs it. Under Automation, `python3.13` > Messages can be switched off too.

The installer cannot check that these steps were done: reading macOS's permission records needs Full Disk Access itself.

### Which agents Harold lets speak

Harold gives each way it can reach you its own list of the folders whose agents may use it, and names the chief's folder. Out of the box only an agent in a folder called `chief-of-pods` is heard on any channel, so **on a Mac where no agent runs in such a folder Harold hears nothing from any agent until you say otherwise.** Add this to `local.toml`, with your own folders (full paths, or `~/…`):

```toml
[notify]
chief = "/Users/you/Dev/chief-of-pods"                    # which pane is the chief
chief_session = "chief"                                   # the chief's tmux session
imessage = ["/Users/you/Dev/chief-of-pods"]               # who may reach the phone by iMessage
web_app = ["/Users/you/Dev/chief-of-pods"]                # who may reach the phone through the web app
machine_audio_output = ["/Users/you/Dev/chief-of-pods"]   # who may be spoken aloud on the Mac
```

An empty list (`machine_audio_output = []`) lets no agent use that channel, and `["*"]` lets every agent use it. An agent held back is not told: its command still exits 0. The old key `away_messages_only_from` is still read, as `chief` and as every list you do not set, with a warning at start; replace it with these keys. See [Channels](../references/notification/README.md#channels).

### Several Macs on iMessage

If you run Harold on more than one Mac signed into the same Apple ID, each Mac needs to know the names of all your Macs, so that a reply from your phone reaches only the Mac that runs the agent you meant.

1. Read each Mac's name. Harold uses the output of this command unless you set `machine_name`:

   ```sh
   hostname -s
   ```

2. On every Mac, add the same list of all your Macs' names to `~/bin/harold/config/local.toml`. The list includes this Mac's own name. For two Macs named `studio` and `laptop`:

   ```toml
   machines = ["studio", "laptop"]
   ```

   To use a name other than the hostname, set `machine_name = "studio"` on that Mac. A name is not empty and has no whitespace, `/`, `:`, `[` or `]`. Put both settings above the first `[section]` header in the file.
3. Upgrade and configure all Macs together. A Mac running an older Harold sends phone notifications without a machine name, and the other Macs cannot tell it apart.
4. Turn on Messages in iCloud on every Mac, so that a phone notification one Mac sends also appears in the other Macs' Messages. Each Mac decides who owns an untagged reply by reading the latest phone notification in its own Messages database (`~/Library/Messages/chat.db`), including phone notifications sent by the other Macs. Without this sync, each Mac sees only its own phone notifications and owns every untagged reply, as it does without `machines`.
5. Check the sync. Trigger a phone notification on one Mac, then look for the same message on the other Mac: it must have the same `guid` (the message's unique id) and the same `date` (its send time) in both Macs' `chat.db`. You may inspect your own `chat.db` with `sqlite3`, for example `sqlite3 ~/Library/Messages/chat.db "select guid, date from message order by date desc limit 3"`. The terminal needs Full Disk Access to read it.
6. Restart Harold on each Mac with `~/bin/haroldctl restart`. Harold refuses to start when `machines` does not include this Mac's name.

To check it, lock your screen on one Mac and trigger an agent turn completion. The turn-end update starts with `🤖 [<machine_name>/…]`. Reply without a tag: only the Mac that sent the latest phone notification answers. A reply that starts with `[<machine_name>]` or `[<machine_name>/<pane>]` goes to that Mac.

The [inbound routing reference](../references/inbound-message-routing/README.md#several-macs) explains the rules and the failure modes.

## 3. Verify and control the service

```sh
~/bin/haroldctl status
```

Success requires both the managed process owning the listening socket and Harold returning a current agent-state snapshot. The installer performs this same check before reporting success. It does not send a test phone notification.

Start the dash from inside tmux:

```sh
~/bin/tmx-agent-dash
```

If you configure a different gRPC endpoint, pass it explicitly, for example `--endpoint http://127.0.0.1:50061`.

Service commands are:

```sh
~/bin/haroldctl stop
~/bin/haroldctl start
~/bin/haroldctl restart
```

After changing `~/bin/harold/config/local.toml`, restart Harold. For a startup failure, inspect `~/bin/harold/harold.log`; configuration errors and an incompatible existing database require correction before readiness can succeed.

## 4. Reinstall or update

To replace the programs while retaining your settings and managed database, run the installer again:

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh
```

To retain settings but start with a fresh database:

```sh
curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh -s -- --reinstall
```

Reinstall archives the previous installation, including its data, and prints the backup location. It creates a fresh managed store; it does not convert old schemas or delete an external store named in a copied configuration. Use `--config` as well if you want to replace the local settings.

### Upgrading from a build with data/events

A normal install (without `--reinstall`) whose bundle has `data/events/` and no `data/store/` starts Harold on an empty store in `data/store/` and leaves `data/events/` as it is. `--reinstall` moves the whole bundle, `data/events/` included, to the `harold.backup-*` folder it prints. [Starting fresh](../references/operation/README.md#starting-fresh) lists what the empty store does not carry over. Two steps keep the loss small:

1. Install when nothing is waiting to be delivered. Messages still waiting in the delivery outbox of `data/events/` are not sent; the outbox normally empties within seconds of a message arriving.
2. After the install, set mute or pause again in `tmx-agent-dash` if you want them. Both start off.

To go back, install the earlier build again with a normal install. It opens `data/events/`, which holds no event recorded only in `data/store/`. After a `--reinstall`, `data/events/` is in the backup folder, not in the bundle.

## 5. Connect agent hooks

Starting with v0.1.1, the installer provides both Claude Code and Codex completion scripts, plus the shared notifier. Register the installed scripts using the commands below. The installer does not overwrite your Claude, Codex, or OpenCode settings. It changes two things there. It adds the Claude Code lifecycle mod's folder to `CLAUDE_CODE_PLUGIN_DIRS` in `~/.claude/settings.json`, after saving a copy as `settings.json.before-harold`. And it creates a link named `send-message-to-away-device` in each of `~/.claude/skills` and `~/.codex/skills` that points to `~/bin/harold/skills/send-message-to-away-device`, so Claude Code and Codex agents can message your phone with `harold send-message-to-away-device`. A skills folder that does not exist is not created. Anything already at the link's path is left alone, and the installer then prints the `ln -s` command to run yourself. The [agent-monitor hook guide](setup-agent-monitor-hooks.md) covers lifecycle reporting, the Claude Code lifecycle mod and the opt-in OpenCode plugin.

Harold is notified of completed turns by agent-specific Stop hooks. The hook layout keeps Harold integration shared and leaves transcript parsing to each agent adapter:

```
~/bin/harold/hooks/harold_turn_complete.py   # shared Harold notifier, installed by the installer
~/bin/harold/hooks/claude_turn_complete.py   # reads Claude completion data
~/bin/harold/hooks/codex_turn_complete.py    # reads Codex completion data
```

The adapter parses its agent's hook payload and transcript, then calls the shared notifier with:

| Field               | Source                                                        |
| ------------------- | ------------------------------------------------------------- |
| `cwd`               | Agent hook payload or current working directory               |
| `last_user_prompt`  | Last user message from that agent's transcript format         |
| `assistant_message` | Current turn's final assistant/agent message                  |

The shared notifier adds the Harold-specific fields:

| Field          | Source                                                       |
| -------------- | ------------------------------------------------------------ |
| `pane_id`      | `TMUX_PANE` environment variable                             |
| `pane_label`   | `tmux display-message` (e.g. `harold:0.3`)                   |
| `main_context` | Git branch name, or repo name when on `main`                 |

The shared notifier asks `~/bin/haroldctl ensure` to start the managed service unless `haroldctl stop` turned it off, then sends `TurnComplete` through `grpcurl` to the installed endpoint. It does not launch another daemon.

Both scripts call the same shared notifier after translating their provider's completion data. They resolve it beside themselves, so a custom install prefix works too. Python 3.9+ is sufficient; uv is not required.

### Register Claude Code

Add the Stop hook to `~/.claude/settings.json`. Use the absolute path for your installation. Replace an existing Harold Stop hook command rather than adding a duplicate:

```json
{
  "hooks": {
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /Users/<you>/bin/harold/hooks/claude_turn_complete.py"
          }
        ]
      }
    ]
  }
}
```

The Claude adapter skips `SubagentStop`, prefers `last_assistant_message` from the hook payload, and falls back to parsing the Claude JSONL transcript.

### Register Codex

Enable Codex hooks and register the Stop hook in `~/.codex/config.toml`. Use the absolute path for your installation. Replace an existing Harold Stop hook command rather than adding a duplicate:

```toml
[features]
codex_hooks = true

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = "python3 /Users/<you>/bin/harold/hooks/codex_turn_complete.py"
timeout = 15
statusMessage = "Notifying Harold"
```

The Codex adapter parses Codex transcript events and supports both `event_msg` user/agent messages and `response_item` role-based messages.

## 6. Test turn-end updates when ready

The service readiness check establishes that Harold is running. It does not prove that macOS permissions, provider hooks, or channel credentials work end to end. After completing those steps, submit a controlled task through a configured agent and verify the dash and the expected turn-end update.

The daemon also provides an explicit diagnostic command that can speak or send an away-channel message. Run it only when you intend to test those effects:

```sh
~/bin/harold/harold --diagnostics
```

## Build from source

Use this path to install local code changes. In addition to the runtime tools above, install Xcode Command Line Tools, Rust/Cargo, and protoc:

```sh
xcode-select --install
brew install rust protobuf
```

Get the source:

```sh
git clone https://github.com/kahgeh/harold.git
cd harold
./scripts/install.sh
```

The source installer builds both programs using the checked-in Cargo lockfile, then performs the same configuration and readiness checks as the release installer.

Run `./scripts/install.sh` again to install the current checkout while retaining settings and data. Use `./scripts/install.sh --reinstall` for fresh storage with retained settings.

`make install` and `make deploy` use the same normal-install path. `make reinstall` selects fresh storage. Additional flags can be passed as `INSTALL_ARGS`, for example:

```sh
make reinstall INSTALL_ARGS='--offline'
```

`--offline` requires the locked Cargo dependencies to be cached already.
