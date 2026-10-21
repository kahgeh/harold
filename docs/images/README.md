# Screenshots

Pictures used by the front page [README](../../README.md). They show a demo Harold with made-up content only.

| File | What it shows |
|---|---|
| `dash.png` | The dash with four agents and four todos waiting. |
| `web-app-todos.png` | The web app's todo page at phone width (390 by 844, scale 3). |

`web-app-thread.png` is not taken: the thread needs the chief to send made-up agent messages, and the demo has no chief doing that.

## Sample content

- Agents: `acme-shop`, `acme-api`, `acme-docs` and the chief `acme-chief`, each a stub script whose name contains `claude`, in `Dev/<name>` under a temporary home.
- Todos: questions Q1 (`acme-shop`, date format of an export), Q2 (`acme-api`, two choices on rate limits), Q3 (`acme-docs`) and instructions I1 (`acme-shop`, try the new checkout).
- Web app: `app_name = "Harold demo"`, login `demo@example.com`.

## How to take them again

1. Build with `cargo build --locked`.
2. Make a temporary home, config folder, store folder and a short tmux folder (for example `/tmp/hd`; a tmux socket path must be short). Copy `harold/config/default.toml` into the config folder and add a `local.toml` with: `[store] path`; `[grpc] port = 50160`; `[web_app] listen = "127.0.0.1:4430"`, `allowed_login = "demo@example.com"`, `app_name = "Harold demo"`; `[telemetry] export = false`; `[chat_db] path` to a file that does not exist; `[notify]` with `imessage` and `machine_audio_output` both `[]`, and `chief` and `web_app` naming the demo chief's folder; and `[imessage]` with a made-up `recipient` and `handle_ids` (Harold refuses to start without them; with `imessage = []` nothing is sent).
3. Run every command with `env -i HOME=<temp home> PATH=<PATH> HAROLD_CONFIG_DIR=<config> TMUX_TMPDIR=<tmux folder>` in front, so nothing reaches your own Harold or tmux server. Start `target/debug/harold`.
4. In that tmux server, start one pane for each agent in `<temp home>/Dev/<name>`. Each runs a stub that prints a Busy screen (`esc to interrupt`) or an Idle one (`❯`). A work summary appears only for a prompt submitted after Harold first sees the pane, so the stub starts idle, then prints a prompt line.
5. From the chief's folder, put the questions and instructions with `harold tool questions put < file.json`.
6. Dash: run `target/debug/tmx-agent-dash --endpoint http://127.0.0.1:50160` in a 120 by 24 window, capture it with `tmux capture-pane -e -p`, turn the capture into HTML and shoot that page with headless Chrome at 1060 by 470.
7. Web app: send `http://127.0.0.1:4430/#todos` through a loopback proxy that adds the `Tailscale-User-Login: demo@example.com` header, and capture it from Chrome over the DevTools protocol with a 390 by 844 phone screen at scale 3.
8. Before committing, get each search term at search time and write them, one per line, to a file in the temporary folder (never into the repo):

   | Term | Command |
   |---|---|
   | Login name | `id -un` |
   | Home folder | `echo "$HOME"` |
   | This Mac's names | `scutil --get ComputerName`, `scutil --get LocalHostName` |
   | The other Macs and the tailnet name | `tailscale status --json`: the `HostName` of `Self` and of each `Peer`, and `MagicDNSSuffix` |
   | Phone number and email | `[imessage] recipient` in `~/bin/harold/config/local.toml`, and `git config user.email` |

   Then run `grep -F -i -f <that file>` over the capture and the page HTML; it must find nothing. Also check Harold's log for any send by iMessage or `say`, and keep each PNG under 400 KB.
9. Stop the demo Harold and tmux server and remove the temporary folders, including the search-terms file.
