# Harold

Stop letting your agents nap.

Harold watches the coding agents you run in tmux and tells you when one finishes its turn. At your desk it speaks up; when you are away it sends the turn-end update to your phone, and your reply goes back to the right agent. The work keeps moving while you make coffee.

## What it does for you

- When an agent's turn ends, Harold says so at the desk.
- When you are away, it sends the turn-end update to the phone by iMessage, Telegram or the web app.
- Your reply from the phone goes to the right pane.
- The dash shows every agent's state and your todos.
- The web app holds the chief's thread, the todo page and calls.

![The dash: four agents, their states and work summaries, and four todos waiting](docs/images/dash.png)

The dash lists each agent's state and work summary, with the todos under the chief.

<img src="docs/images/web-app-todos.png" alt="The web app's todo page on a phone" width="300">

The web app's todo page: answer the chief's questions from your phone.

## Install

1. On an Apple Silicon Mac (macOS 15 or newer), run `brew install python tmux grpcurl`.
2. Run the bootstrap, which verifies the release checksum and opens the configuration wizard:
   ```sh
   curl -fsSL https://raw.githubusercontent.com/kahgeh/harold/main/scripts/bootstrap.sh | sh
   ```
3. Connect your agents with the [hook setup guide](docs/how-tos/setup-agent-monitor-hooks.md).
4. Inside tmux, start the dash: `tmx-agent-dash`.

## Docs

- [Docs index](docs/README.md)
- [Install or reinstall Harold](docs/how-tos/setup.md) and [prerequisites](docs/prerequisites.md)
- [Hook setup](docs/how-tos/setup-agent-monitor-hooks.md)
- [The dash](tmx-agent-dash/README.md)
- [Architecture](docs/explanations/architecture.md) and [agent monitor](docs/references/agent-monitor/README.md)
- [Cut or install a release](docs/how-tos/release.md)
- [Dependency audits](docs/references/dependency-audits.md): always build with `--locked`, and send any `Cargo.lock` change through the rust-supply-chain-auditor first.

## Status

Early development.
