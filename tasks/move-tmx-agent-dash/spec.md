# Move tmx-agent-dash Into Harold

## Outcome

Harold and `tmx-agent-dash` will live in the Harold Git repository and Rust workspace. The dashboard remains a separate binary and library crate with one compile-time dependency on `harold-api`. The existing `events/` submodule remains unchanged.

Running `make deploy` will install a matching Harold daemon and dashboard from the same workspace revision. Harold continues to run as a background daemon. The dashboard remains an interactive command that the user starts inside the tmux client it should navigate.

## Current State

The source dashboard repository is `/Users/kahgeh/Dev/p/tmx-agent-dash`. It has no remote and contains four commits on `main`, ending at `dc7c4919c3d589ef434ececca4b6a9562cfc127c`:

1. `c45dc1a` — initial dashboard TUI checkpoint
2. `42bb384` — state and integrations
3. `1bbb387` — responsive renderer
4. `dc7c491` — production runtime

The reviewed dashboard state also includes modified tracked source and untracked documentation, task records, fixtures, screenshots, and terminal fault-harness files. That state must not be reduced to the current `HEAD`. The ignored 1.8 GiB `target/` directory is build output and must not be imported.

Harold is a Rust workspace with `harold`, `harold-api`, and the `events` submodule as members. Its `make deploy` target currently installs only Harold under `~/bin/harold/`. The user's `~/bin` directory is already on `PATH`; `~/bin/harold` is not.

## Repository Shape

The final checkout will have this ownership structure:

```text
harold/
├── Cargo.toml
├── Cargo.lock
├── harold/
├── harold-api/
├── events/                 # unchanged Git submodule
├── tmx-agent-dash/         # dashboard binary and library crate
├── docs/
└── tasks/
```

`tmx-agent-dash` will remain independently understandable and testable as a package. It will consume `WatchAgentStates` through `harold-api`; it will not import Harold daemon internals or move dashboard behavior into the daemon.

## History-Preserving Import

The import will use native Git with a non-squashed subtree merge:

1. In the source dashboard repository, verify the exact working-tree inventory and checkpoint all intended modified and untracked files in a fifth commit. Do not include ignored `target/` or other generated output.
2. Fetch the source repository into Harold using a temporary local remote or equivalent local Git reference.
3. Add the source history beneath `tmx-agent-dash/` without squashing it.
4. Remove the temporary remote or local import reference after the history is reachable from Harold.
5. Keep the original sibling repository in place as a recovery copy. Do not delete, reset, or clean it.

The fifth source commit will retain the dashboard's standalone `Cargo.lock` because it records a truthful source-repository checkpoint. A later Harold integration commit will remove the nested lockfile once the crate is a workspace member.

All five source commits must remain reachable from the final Harold branch. The import must not rewrite their commit identities.

## Workspace Integration

The Harold integration commit will:

- add `tmx-agent-dash` to the root workspace members;
- retain workspace resolver `"2"`;
- change the dashboard's `harold-api` dependency path from `../harold/harold-api` to `../harold-api`;
- remove `tmx-agent-dash/Cargo.lock` in favor of the root workspace lockfile;
- resolve the root lockfile only after the required Rust supply-chain review approves Ratatui `0.30.2`, Crossterm `0.29.0`, and their newly introduced transitive dependency graph;
- preserve the dashboard as the package and binary name `tmx-agent-dash`;
- leave `events/`, its gitlink, and `.gitmodules` unchanged.

The dashboard's `README.md` will remain at `tmx-agent-dash/README.md`. Harold's root README will gain a concise dashboard description and the commands needed to build, deploy, and run it.

Dashboard project evidence will move from `tmx-agent-dash/tasks/tmux-agent-dashboard/` to the root `tasks/tmux-agent-dashboard/`, matching Harold's task-record convention. Dashboard-specific lessons will be merged into root `tasks/lessons.md` without replacing or weakening existing lessons. The redundant `tmx-agent-dash/tasks/` directory will then be removed.

## Installation and Runtime

The root build continues to use:

```sh
make build
```

Because the dashboard is a workspace member, the release build produces both:

```text
target/release/harold
target/release/tmx-agent-dash
```

`make deploy` will retain the current Harold installation and restart behavior and will additionally:

1. copy `target/release/tmx-agent-dash` to `~/bin/tmx-agent-dash`;
2. code-sign the installed dashboard with the existing `CODESIGN_IDENTITY`;
3. leave the dashboard stopped.

The installed command will therefore be available directly as:

```sh
tmx-agent-dash
```

It connects to `http://127.0.0.1:50060` by default. The user starts it inside the tmux client it should navigate. It is not a daemon, launch agent, or child process managed by Harold.

If `~/bin/tmx-agent-dash` already exists when deployment begins, the workflow will preserve a rollback copy before replacement.

## Failure Boundaries

- The source checkpoint is a separate commit, so import failure cannot erase the reviewed dashboard state.
- The source repository remains present after the migration until all history, content, build, deployment, and live-runtime checks pass.
- The dependency audit happens before editing the Harold workspace manifest or running a command that may fetch the newly introduced crates.
- A failure while checkpointing, importing, resolving dependencies, or deploying stops the migration at that boundary. The implementation must inspect the actual state and revise the plan rather than continuing through a partial migration.
- Build completion precedes installed-binary replacement. A failed build must not disturb the running Harold daemon or installed dashboard.
- Unrelated Harold changes and the `events` submodule state must be preserved.

## Verification

### Source and history

- Record the dashboard source status before checkpointing.
- Confirm the source checkpoint includes every intended modified and untracked file and excludes ignored `target/`.
- Confirm all five dashboard commits are ancestors of the final Harold branch.
- Compare the source checkpoint tree with the imported dashboard tree. Only the approved path dependency, nested lockfile, relocated task records, merged lessons, and documentation integration may differ.
- Confirm the original source repository still exists at its original path.

### Repository boundaries

- Confirm the `events` gitlink value is identical before and after migration.
- Confirm `.gitmodules` is byte-for-byte unchanged.
- Confirm no nested `.git` directory or imported `target/` exists beneath `tmx-agent-dash/`.
- Inspect the complete Git diff and run `git diff --check`.

### Rust workspace

- Run the approved supply-chain audit before dependency introduction.
- Run Cargo metadata offline and confirm all four workspace members resolve from the root.
- Run formatting checks.
- Run warnings-denied Clippy for all workspace targets and features.
- Run all workspace tests offline, including dashboard examples and the terminal fault-harness feature.
- Build debug and release workspace artifacts offline.
- Run the repository's dependency security audit without fetching.

### Installation and live behavior

- Run `make deploy` through the documented workflow.
- Verify the installed Harold and dashboard binaries match their release artifacts and have valid code signatures.
- Confirm Harold restarts successfully and listens on its configured endpoint.
- Confirm `command -v tmx-agent-dash` resolves to `~/bin/tmx-agent-dash`.
- Start the installed dashboard in a disposable tmux pane, confirm it connects to Harold and renders the agent-state view, then quit it normally and confirm terminal restoration.

### Completion review

An independent completion reviewer must inspect the complete migration, repository state, dependency changes, task-record merge, tests, deployment evidence, and live dashboard smoke test. All findings must be resolved and the reviewer must provide an explicit thumbs-up before completion is reported.

## Out of Scope

- Absorbing or rewriting the `events` submodule.
- Moving dashboard behavior into the Harold daemon.
- Running the dashboard as a daemon or launch agent.
- Adding transport authentication or changing the dashboard endpoint contract.
- Redesigning the dashboard UI or changing its behavior.
- Deleting the original dashboard repository.
