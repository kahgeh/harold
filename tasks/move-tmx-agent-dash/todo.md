# Move tmx-agent-dash Into Harold Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve the complete `tmx-agent-dash` source and history inside Harold, make it a workspace member, and install it as the on-demand `tmx-agent-dash` command through `make deploy`.

**Architecture:** Create a fifth checkpoint commit in the standalone dashboard repository, then use a non-squashed native Git subtree import under `tmx-agent-dash/`. Integrate the imported package only through the root Cargo workspace and `harold-api`; keep the `events/` submodule unchanged. Extend the existing deployment shell to install and sign the dashboard at `~/bin/tmx-agent-dash` without running it as a daemon.

**Tech Stack:** Git subtree, Rust 2024, Cargo resolver 2, Ratatui 0.30.2, Crossterm 0.29.0, Tokio 1.49.0, Tonic 0.14.5, GNU Make, tmux 3.6a, macOS codesign.

**Spec:** `tasks/move-tmx-agent-dash/spec.md`

## Global Constraints

- Preserve the dashboard commits `c45dc1a`, `42bb384`, `1bbb387`, and `dc7c4919c3d589ef434ececca4b6a9562cfc127c` without rewriting their identities.
- Preserve the dashboard's complete reviewed working state in a fifth source commit before importing.
- Never import `/Users/kahgeh/Dev/p/tmx-agent-dash/target/` or a nested `.git/` directory.
- Keep the original dashboard repository at `/Users/kahgeh/Dev/p/tmx-agent-dash` as a recovery copy.
- Keep the `events` gitlink at `a23c70c13588beeb9ebd4a248d4b91f5bad8bd46` and keep `.gitmodules` SHA-256 `8a32225f720343cd59eb8d0d6219e42145da61c29680a49865dbf0e2db1ee60d`.
- Do not edit a manifest or run a command that may fetch newly introduced crates until the Rust supply-chain auditor approves Ratatui 0.30.2, Crossterm 0.29.0, and their transitive graph.
- Retain `tmx-agent-dash` as an independent binary and library crate whose only Harold compile-time coupling is `harold-api`.
- Retain Cargo workspace resolver `"2"` and use the root `Cargo.lock`; do not retain `tmx-agent-dash/Cargo.lock` after integration.
- Install the dashboard at `~/bin/tmx-agent-dash`; do not start or supervise it from Harold.
- Preserve unrelated user work. If observed state differs from the baselines in this plan, stop and re-plan before mutating it.
- Use `apply_patch` for normal file edits and project formatters only for mechanical formatting.
- Do not delete, reset, clean, or move the source dashboard repository after migration.

---

### Task 1: Audit the dependencies and revalidate both repositories

**Files:**
- Modify: `tasks/move-tmx-agent-dash/todo.md`

**Interfaces:**
- Consumes: the standalone dashboard's pinned `Cargo.toml` and `Cargo.lock`
- Produces: an explicit supply-chain approval or a hard stop before dependency introduction

- [x] **Step 1: Re-read the approved specification and relevant lessons**

Read `tasks/move-tmx-agent-dash/spec.md`, root `tasks/lessons.md`, and `/Users/kahgeh/Dev/p/tmx-agent-dash/tasks/lessons.md`. Confirm the execution still matches the approved repository, history, installation, and verification boundaries.

- [x] **Step 2: Revalidate the Harold baseline**

Run:

```sh
git status --short --branch
git rev-parse HEAD:events
shasum -a 256 .gitmodules
git log -3 --oneline --decorate
```

Expected: no unplanned worktree changes; `events` is `a23c70c13588beeb9ebd4a248d4b91f5bad8bd46`; `.gitmodules` has the approved SHA-256. The task spec may have the one planned wording correction staged or committed.

- [x] **Step 3: Revalidate the dashboard baseline**

Run:

```sh
git -C /Users/kahgeh/Dev/p/tmx-agent-dash status --short --branch
git -C /Users/kahgeh/Dev/p/tmx-agent-dash rev-parse HEAD
git -C /Users/kahgeh/Dev/p/tmx-agent-dash remote -v
git -C /Users/kahgeh/Dev/p/tmx-agent-dash diff --check
```

Expected: branch `main`; HEAD `dc7c4919c3d589ef434ececca4b6a9562cfc127c`; no remote; exactly the six known modified tracked files and the known README, example, terminal module, and task tree untracked; diff check passes.

- [x] **Step 4: Dispatch the required Rust supply-chain auditor**

Give the `rust_supply_chain_auditor` the immutable direct versions, exact features, standalone lockfile, and intended root-workspace introduction:

```toml
ratatui = { version = "=0.30.2", default-features = false, features = ["crossterm_0_29"] }
crossterm = { version = "=0.29.0", default-features = false, features = ["events"] }
tokio = { version = "=1.49.0", default-features = false, features = ["macros", "rt-multi-thread", "signal", "sync", "time"] }
tonic = { version = "=0.14.5", default-features = false, features = ["channel", "codegen"] }
```

The audit must cover Ratatui, Crossterm, every dependency newly introduced to Harold through them, provenance, maintenance, unsafe code, build scripts, platform behavior, and known advisories. Tokio and Tonic already exist in Harold but their resolved features must also be checked for workspace unification effects.

- [x] **Step 5: Record the audit result and stop on rejection**

Append the auditor's approved versions, conditions, and evidence to `## Review` below. If any package or version is rejected, mark the task blocked and re-plan; do not checkpoint, import, edit manifests, or run Cargo resolution.

- [x] **Step 6: Commit the recorded audit gate**

```sh
git add tasks/move-tmx-agent-dash/todo.md tasks/move-tmx-agent-dash/spec.md
git diff --cached --check
git commit -m "docs: approve dashboard migration dependencies"
```

### Task 2: Checkpoint and verify the standalone dashboard

**Files:**
- Modify: `/Users/kahgeh/Dev/p/tmx-agent-dash/Cargo.toml`
- Modify: `/Users/kahgeh/Dev/p/tmx-agent-dash/examples/dashboard_demo.rs`
- Modify: `/Users/kahgeh/Dev/p/tmx-agent-dash/src/app.rs`
- Modify: `/Users/kahgeh/Dev/p/tmx-agent-dash/src/runtime.rs`
- Modify: `/Users/kahgeh/Dev/p/tmx-agent-dash/src/terminal.rs`
- Modify: `/Users/kahgeh/Dev/p/tmx-agent-dash/src/ui.rs`
- Create: `/Users/kahgeh/Dev/p/tmx-agent-dash/README.md`
- Create: `/Users/kahgeh/Dev/p/tmx-agent-dash/examples/terminal_fault_harness.rs`
- Create: `/Users/kahgeh/Dev/p/tmx-agent-dash/src/terminal/fault_harness.rs`
- Create: `/Users/kahgeh/Dev/p/tmx-agent-dash/tasks/lessons.md`
- Create: `/Users/kahgeh/Dev/p/tmx-agent-dash/tasks/tmux-agent-dashboard/**`

**Interfaces:**
- Consumes: the approved dirty dashboard worktree at `dc7c491`
- Produces: one clean source commit containing the complete reviewed dashboard tree

- [x] **Step 1: Record the exact intended source inventory**

Run:

```sh
git -C /Users/kahgeh/Dev/p/tmx-agent-dash status --porcelain=v2
find /Users/kahgeh/Dev/p/tmx-agent-dash \
  -path /Users/kahgeh/Dev/p/tmx-agent-dash/.git -prune -o \
  -path /Users/kahgeh/Dev/p/tmx-agent-dash/target -prune -o \
  -type f -print | sort
```

Expected inventory: `.gitignore`, `Cargo.lock`, `Cargo.toml`, `README.md`, two example files, eleven files under `src/` including `src/terminal/fault_harness.rs`, `tasks/lessons.md`, and the dashboard spec, plan, todo, screen ledger, fixture, HTML visual, and four PNG screenshots.

- [x] **Step 2: Verify the standalone source before checkpointing**

Run from `/Users/kahgeh/Dev/p/tmx-agent-dash`:

```sh
cargo fmt --all -- --check
cargo test --all-targets --all-features --offline
cargo clippy --all-targets --all-features --offline -- -D warnings
cargo build --release --all-targets --all-features --offline
git diff --check
```

Expected: all 106 library tests and example tests pass, formatting and warnings-denied Clippy pass, the release build succeeds, and Git reports no whitespace errors. Record actual counts rather than copying historical counts if they differ.

- [x] **Step 3: Stage only the intended dashboard state**

Run:

```sh
git -C /Users/kahgeh/Dev/p/tmx-agent-dash add \
  .gitignore Cargo.lock Cargo.toml README.md examples src tasks
git -C /Users/kahgeh/Dev/p/tmx-agent-dash diff --cached --check
git -C /Users/kahgeh/Dev/p/tmx-agent-dash status --short
git -C /Users/kahgeh/Dev/p/tmx-agent-dash ls-files --stage
```

Expected: every intended modified and untracked file is staged; `target/` and `.git/` are absent; no other file is staged.

- [x] **Step 4: Create the fifth source commit**

```sh
git -C /Users/kahgeh/Dev/p/tmx-agent-dash commit -m "fix: harden dashboard terminal lifecycle"
```

Record the resulting full commit ID as `DASHBOARD_CHECKPOINT` in `## Review`.

- [x] **Step 5: Verify the recovery repository**

Run:

```sh
git -C /Users/kahgeh/Dev/p/tmx-agent-dash status --short --branch
git -C /Users/kahgeh/Dev/p/tmx-agent-dash log -5 --oneline --decorate
git -C /Users/kahgeh/Dev/p/tmx-agent-dash show --stat --oneline HEAD
```

Expected: the source worktree is clean, the fifth commit sits above the four original commits, and all reviewed files are committed.

### Task 3: Import the five-commit history beneath `tmx-agent-dash/`

**Files:**
- Create: `tmx-agent-dash/**`

**Interfaces:**
- Consumes: `DASHBOARD_CHECKPOINT` from Task 2
- Produces: a non-squashed subtree merge with all five source commits reachable

- [x] **Step 1: Confirm the destination is ready**

Run from the Harold execution checkout:

```sh
test ! -e tmx-agent-dash
git status --short --branch
git rev-parse HEAD:events
shasum -a 256 .gitmodules
```

Expected: `tmx-agent-dash/` does not exist; the worktree is clean; both immutable submodule baselines match the global constraints.

- [x] **Step 2: Fetch the local source history**

```sh
git remote add tmx-agent-dash-import /Users/kahgeh/Dev/p/tmx-agent-dash
git fetch tmx-agent-dash-import main
test "$(git rev-parse tmx-agent-dash-import/main)" = "$(git -C /Users/kahgeh/Dev/p/tmx-agent-dash rev-parse HEAD)"
```

- [x] **Step 3: Add the non-squashed subtree**

```sh
git subtree add \
  --prefix=tmx-agent-dash \
  tmx-agent-dash-import main \
  -m "chore: import tmx-agent-dash history"
```

Expected: Git creates one subtree merge commit; it does not report `--squash`; the imported files appear only below `tmx-agent-dash/`.

- [x] **Step 4: Remove the temporary remote and verify ancestry**

```sh
git remote remove tmx-agent-dash-import
DASHBOARD_CHECKPOINT=$(git -C /Users/kahgeh/Dev/p/tmx-agent-dash rev-parse HEAD)
git merge-base --is-ancestor c45dc1a HEAD
git merge-base --is-ancestor 42bb384 HEAD
git merge-base --is-ancestor 1bbb387 HEAD
git merge-base --is-ancestor dc7c4919c3d589ef434ececca4b6a9562cfc127c HEAD
git merge-base --is-ancestor "$DASHBOARD_CHECKPOINT" HEAD
git log --graph --oneline --decorate -12
```

Expected: all ancestry checks exit zero and the graph shows the dashboard chain as the subtree merge's second parent.

- [x] **Step 5: Verify excluded repository internals**

```sh
test ! -e tmx-agent-dash/.git
test ! -e tmx-agent-dash/target
test -f tmx-agent-dash/Cargo.lock
git status --short --branch
```

Expected: no nested repository or build output; the historical standalone lockfile is present until Task 4; worktree clean after the subtree commit.

### Task 4: Integrate the Cargo workspace and task records

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `tmx-agent-dash/Cargo.toml`
- Delete: `tmx-agent-dash/Cargo.lock`
- Delete: `tmx-agent-dash/tasks/lessons.md`
- Move: `tmx-agent-dash/tasks/tmux-agent-dashboard/` to `tasks/tmux-agent-dashboard/`
- Modify: `tasks/lessons.md`
- Modify: `tasks/move-tmx-agent-dash/todo.md`

**Interfaces:**
- Consumes: standalone package `tmx-agent-dash` and local crate `harold-api`
- Produces: root workspace member `tmx-agent-dash` resolved by the root lockfile

- [x] **Step 1: Demonstrate the missing workspace integration**

Run:

```sh
cargo metadata --offline --no-deps --format-version 1 \
  | rg '"name":"tmx-agent-dash"'
```

Expected: the assertion fails because the root workspace does not yet include the imported crate.

- [x] **Step 2: Add the workspace member and correct the API path**

Use `apply_patch` to make these exact manifest changes:

```toml
[workspace]
members = ["harold", "harold-api", "events", "tmx-agent-dash"]
resolver = "2"
```

```toml
harold-api = { path = "../harold-api" }
```

Do not change the dashboard's package name, feature flags, direct dependency versions, or features.

- [x] **Step 3: Remove the nested lockfile and relocate task evidence**

```sh
git rm tmx-agent-dash/Cargo.lock
git mv tmx-agent-dash/tasks/tmux-agent-dashboard tasks/tmux-agent-dashboard
```

- [x] **Step 4: Merge lessons without overwriting Harold lessons**

Identify dashboard bullets not already present:

```sh
comm -23 \
  <(tail -n +3 tmx-agent-dash/tasks/lessons.md | sed '/^$/d' | sort) \
  <(tail -n +3 tasks/lessons.md | sed '/^$/d' | sort)
```

Use `apply_patch` to append each returned bullet to root `tasks/lessons.md`, preserving every existing line. Then remove the imported dashboard lessons file and empty task directory:

```sh
git rm tmx-agent-dash/tasks/lessons.md
rmdir tmx-agent-dash/tasks
```

- [x] **Step 5: Resolve the workspace lockfile offline**

```sh
MIGRATION_METADATA=$(mktemp)
cargo metadata --offline --format-version 1 > "$MIGRATION_METADATA"
cargo check --workspace --all-targets --all-features --offline
```

Expected: Cargo updates only the root `Cargo.lock`; metadata contains exactly `harold`, `harold-api`, `events`, and `tmx-agent-dash` as workspace members; all targets check successfully without network access.

- [x] **Step 6: Prove the workspace integration**

```sh
cargo metadata --offline --no-deps --format-version 1 \
  | rg '"name":"tmx-agent-dash"'
test ! -e tmx-agent-dash/Cargo.lock
test ! -e tmx-agent-dash/tasks
test -d tasks/tmux-agent-dashboard
cargo tree -p tmx-agent-dash --depth 1 --offline
```

Expected: the dashboard is a member, has no nested lock/task directory, task evidence is at the root, and its five direct dependencies are Ratatui, Crossterm, Tokio, Tonic, and local `harold-api`.

- [x] **Step 7: Commit the workspace integration**

```sh
git add Cargo.toml Cargo.lock tmx-agent-dash/Cargo.toml \
  tasks/lessons.md tasks/tmux-agent-dashboard tasks/move-tmx-agent-dash/todo.md
git diff --cached --check
git diff --cached --submodule=log
git commit -m "build: integrate dashboard workspace crate"
```

### Task 5: Install the dashboard and document its use

**Files:**
- Modify: `Makefile`
- Modify: `README.md`
- Modify: `tmx-agent-dash/README.md`
- Modify: `tasks/move-tmx-agent-dash/todo.md`

**Interfaces:**
- Consumes: `target/release/tmx-agent-dash`, `CODESIGN_IDENTITY`, and `~/bin` already on `PATH`
- Produces: signed on-demand command `~/bin/tmx-agent-dash`

- [x] **Step 1: Demonstrate that deployment does not install the dashboard**

Run:

```sh
make -n deploy | rg 'target/release/tmx-agent-dash|bin/tmx-agent-dash'
```

Expected: the assertion fails because the existing recipe contains no dashboard copy or signing command.

- [x] **Step 2: Extend the deploy recipe**

Use `apply_patch` to add these variables without changing the existing Harold paths:

```make
INSTALL_DIR      := $(HOME)/bin
DEPLOY_DIR       := $(INSTALL_DIR)/harold
BINARY           := target/release/harold
DASHBOARD_BINARY := target/release/tmx-agent-dash
DASHBOARD_INSTALL := $(INSTALL_DIR)/tmx-agent-dash
```

After `mkdir -p $(DEPLOY_DIR)` and before stopping Harold, add these exact effects:

```make
	if [ -f $(DASHBOARD_INSTALL) ]; then cp -p $(DASHBOARD_INSTALL) $(DASHBOARD_INSTALL).pre-deploy; fi
	cp $(DASHBOARD_BINARY) $(DASHBOARD_INSTALL)
	codesign --force --sign "$(CODESIGN_IDENTITY)" $(DASHBOARD_INSTALL)
```

This ordering ensures build and dashboard installation finish before the existing `pkill` restarts Harold. Do not launch the dashboard from Make.

- [x] **Step 3: Prove the dry-run installation chain**

```sh
make -n deploy | rg -n \
  'cargo build --release|target/release/tmx-agent-dash|tmx-agent-dash.pre-deploy|codesign|pkill'
```

Expected order: release build; optional dashboard rollback copy; dashboard copy; dashboard code-sign; Harold stop/copy/sign/start. No command launches `tmx-agent-dash`.

- [x] **Step 4: Update repository documentation**

Use `apply_patch` to add a `## Dashboard` section to root `README.md` after `How it works`. It must state:

````markdown
## Dashboard

The [`tmx-agent-dash`](tmx-agent-dash/README.md) terminal dashboard shows Harold's current agent-pane projection and can switch the invoking tmux client to a selected pane.

Build and install Harold and the dashboard from the same workspace revision:

```sh
make deploy
```

Then start the dashboard inside the tmux client it should navigate:

```sh
tmx-agent-dash
```
````

Update `tmx-agent-dash/README.md` so its prerequisite/build text refers to the containing Harold workspace, `harold-api` at `../harold-api`, root `make build`, and root `make deploy`. Retain the standalone Cargo development commands and all behavioral documentation.

- [x] **Step 5: Commit installation and documentation**

```sh
git add Makefile README.md tmx-agent-dash/README.md \
  tasks/move-tmx-agent-dash/todo.md
git diff --cached --check
git commit -m "build: install tmx agent dashboard"
```

### Task 6: Verify history, content, workspace behavior, and security

**Files:**
- Modify: `tasks/move-tmx-agent-dash/todo.md`

**Interfaces:**
- Consumes: the imported and integrated repository
- Produces: reproducible evidence that history, content, builds, tests, and boundaries are intact

- [x] **Step 1: Verify immutable repository boundaries**

```sh
test "$(git rev-parse HEAD:events)" = \
  a23c70c13588beeb9ebd4a248d4b91f5bad8bd46
test "$(shasum -a 256 .gitmodules | awk '{print $1}')" = \
  8a32225f720343cd59eb8d0d6219e42145da61c29680a49865dbf0e2db1ee60d
test ! -e tmx-agent-dash/.git
test ! -e tmx-agent-dash/target
test ! -e tmx-agent-dash/Cargo.lock
test ! -e tmx-agent-dash/tasks
test -d tasks/tmux-agent-dashboard
test -d /Users/kahgeh/Dev/p/tmx-agent-dash/.git
```

- [x] **Step 2: Verify every dashboard commit remains reachable**

```sh
DASHBOARD_CHECKPOINT=$(git -C /Users/kahgeh/Dev/p/tmx-agent-dash rev-parse HEAD)
for commit in \
  c45dc1a 42bb384 1bbb387 \
  dc7c4919c3d589ef434ececca4b6a9562cfc127c \
  "$DASHBOARD_CHECKPOINT"
do
  git merge-base --is-ancestor "$commit" HEAD
done
```

Expected: every command exits zero.

- [x] **Step 3: Compare the source checkpoint with the imported content**

Create a disposable directory and extract the checkpoint without touching either worktree:

```sh
DASHBOARD_CHECKPOINT=$(git -C /Users/kahgeh/Dev/p/tmx-agent-dash rev-parse HEAD)
MIGRATION_COMPARE_DIR=$(mktemp -d)
mkdir "$MIGRATION_COMPARE_DIR/source"
git -C /Users/kahgeh/Dev/p/tmx-agent-dash archive "$DASHBOARD_CHECKPOINT" \
  | tar -x -C "$MIGRATION_COMPARE_DIR/source"
diff -qr \
  -x Cargo.toml -x Cargo.lock -x README.md -x tasks \
  "$MIGRATION_COMPARE_DIR/source" tmx-agent-dash
diff -qr \
  "$MIGRATION_COMPARE_DIR/source/tasks/tmux-agent-dashboard" \
  tasks/tmux-agent-dashboard
```

Expected: both comparisons report no differences. Inspect the intentional
manifest and approved workspace/API/build/deploy/path README diffs separately:

```sh
diff -u \
  "$MIGRATION_COMPARE_DIR/source/Cargo.toml" \
  tmx-agent-dash/Cargo.toml
diff -u \
  "$MIGRATION_COMPARE_DIR/source/README.md" \
  tmx-agent-dash/README.md
```

Expected: the Cargo manifest diff contains only the `harold-api` path change;
the README diff contains only approved workspace/API/build/deploy/path
integration.

- [x] **Step 4: Run the complete offline Rust gate**

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --offline --locked -- -D warnings
cargo test --workspace --all-targets --all-features --offline --locked
cargo build --workspace --all-targets --all-features --offline --locked
cargo build --workspace --release --all-targets --all-features --offline --locked
cargo doc --workspace --no-deps --all-features --offline --locked
cargo audit --no-fetch --file Cargo.lock
```

Expected: every command exits zero. Record actual test counts and any already-approved audit warning exactly.

- [x] **Step 5: Inspect repository changes**

```sh
git diff --check
git status --short --branch
git log --graph --oneline --decorate -15
git diff --stat 811244a..HEAD
git diff --submodule=log 811244a..HEAD
```

Expected: no uncommitted implementation changes, imported history is visible, and the `events` submodule has no change.

- [x] **Step 6: Record the verification evidence**

Append commands, exact outcomes, test totals, audit result, checkpoint commit, import commit, content comparison, and boundary hashes to `## Review` below. Commit the evidence:

```sh
git add tasks/move-tmx-agent-dash/todo.md
git diff --cached --check
git commit -m "docs: verify dashboard repository migration"
```

### Task 7: Deploy, exercise the installed dashboard, and obtain completion review

**Files:**
- Modify: `tasks/move-tmx-agent-dash/todo.md`
- Runtime install: `~/bin/tmx-agent-dash`
- Optional rollback: `~/bin/tmx-agent-dash.pre-deploy`

**Interfaces:**
- Consumes: verified release binaries and the existing Harold deployment configuration
- Produces: running Harold daemon plus a signed, PATH-resolvable, manually launched dashboard command

- [ ] **Step 1: Inventory the live installation before replacement**

```sh
command -v harold || true
command -v tmx-agent-dash || true
ls -l "$HOME/bin/harold/harold" "$HOME/bin/tmx-agent-dash" 2>/dev/null || true
pgrep -fl "$HOME/bin/harold/harold" || true
lsof -nP -iTCP:50060 -sTCP:LISTEN || true
```

Record the existing Harold PID and whether a dashboard or rollback copy exists.

- [ ] **Step 2: Run the documented deployment**

```sh
make deploy
```

Expected: the workspace release build succeeds before mutation; any prior dashboard is copied to `.pre-deploy`; both binaries are copied and signed; Harold restarts; the dashboard is not launched.

- [ ] **Step 3: Verify installation and daemon health**

```sh
test "$(command -v tmx-agent-dash)" = "$HOME/bin/tmx-agent-dash"
codesign --verify --strict --verbose=2 "$HOME/bin/tmx-agent-dash"
codesign --verify --strict --verbose=2 "$HOME/bin/harold/harold"
pgrep -fl "$HOME/bin/harold/harold"
lsof -nP -iTCP:50060 -sTCP:LISTEN
```

Confirm the new Harold PID differs from the recorded old PID and the listener belongs to the installed Harold process.

- [ ] **Step 4: Start the installed dashboard in an exact disposable tmux session**

Create only the named disposable session:

```sh
! tmux has-session -t harold-dashboard-smoke 2>/dev/null
tmux new-session -d -s harold-dashboard-smoke -x 120 -y 40
tmux send-keys -t harold-dashboard-smoke:0.0 "$HOME/bin/tmx-agent-dash"
tmux send-keys -t harold-dashboard-smoke:0.0 Enter
```

`tmux-send-keys` policy applies: the command and `Enter` must remain separate calls.

- [ ] **Step 5: Verify the live TUI and terminal restoration**

After the dashboard renders, run:

```sh
tmux capture-pane -p -t harold-dashboard-smoke:0.0 -S -80
tmux display-message -p -t harold-dashboard-smoke:0.0 \
  '#{pane_current_command} #{alternate_on} #{pane_dead}'
```

Expected: the capture contains the Harold dashboard masthead, endpoint/revision/health information, and current rows or the valid empty-state message; pane command is `tmx-agent-dash`, alternate screen is `1`, and pane is alive.

Quit and verify restoration:

```sh
tmux send-keys -t harold-dashboard-smoke:0.0 q
tmux display-message -p -t harold-dashboard-smoke:0.0 \
  '#{pane_current_command} #{alternate_on} #{pane_dead}'
```

Expected: the pane returns to its shell, alternate screen is `0`, and the pane remains alive. Then remove only the disposable session created by this task:

```sh
tmux kill-session -t harold-dashboard-smoke
```

- [ ] **Step 6: Record deployment and live evidence**

Append the before/after PIDs, listener, install paths, signature checks, tmux capture summary, and restoration result to `## Review` below.

- [ ] **Step 7: Request the mandatory completion reviewer**

Dispatch a `review_subagent` to inspect all changes from `811244a` through `HEAD`, both repository histories, the dependency audit, root lockfile, Makefile ordering, task/lesson relocation, verification outputs, installed artifacts, Harold restart, and live tmux smoke evidence. Require an explicit thumbs-up.

- [ ] **Step 8: Resolve every review finding and repeat affected gates**

Send findings back to the responsible implementation agent, implement root-cause fixes, rerun every affected focused and full check, update `## Review`, and request completion re-review. Do not report completion until no findings remain and the reviewer returns an explicit thumbs-up.

- [ ] **Step 9: Commit the final evidence**

```sh
git add tasks/move-tmx-agent-dash/todo.md
git diff --cached --check
git commit -m "docs: complete dashboard migration review"
```

## Review

Planning status:

- The repository and dashboard inventories were completed read-only.
- The user approved the non-squashed history-preserving subtree, unchanged `events` submodule, root Cargo workspace integration, root task-record ownership, and installation at `~/bin/tmx-agent-dash` through `make deploy`.
- The specification is committed at `811244a` and was approved by the user.
- Implementation evidence is pending.

### 2026-08-24 — Task 1 dependency gate and baseline revalidation

- Re-read `tasks/move-tmx-agent-dash/spec.md`, Harold `tasks/lessons.md`, and
  the dashboard's `tasks/lessons.md`. The approved boundaries remain intact:
  preserve the four source commits plus a fifth checkpoint, use a non-squashed
  subtree, keep `events` and `.gitmodules` unchanged, introduce no dependency
  before approval, retain the dashboard as a separate binary/library coupled
  only through `harold-api`, and install it on demand at `~/bin/tmx-agent-dash`.
- Harold baseline, from this execution worktree: `git status --short --branch`
  reported only `## feat/move-tmx-agent-dash`; `git rev-parse HEAD:events`
  returned `a23c70c13588beeb9ebd4a248d4b91f5bad8bd46`; and `shasum -a 256
  .gitmodules` returned `8a32225f720343cd59eb8d0d6219e42145da61c29680a49865dbf0e2db1ee60d`.
  `git log -3 --oneline --decorate` showed `d1a3989`, `2452ba1`, and `811244a`.
  The planned deployment-verification wording correction is already committed
  in `2452ba1`; no uncommitted specification correction remained.
- Dashboard baseline: `main` is at
  `dc7c4919c3d589ef434ececca4b6a9562cfc127c`, has no remote, and
  `git diff --check` exited zero. Its status is the expected six modified
  tracked files (`Cargo.toml`, dashboard demo, `app`, `runtime`, `terminal`,
  and `ui`) plus the expected untracked `README.md`, terminal fault-harness
  example/module, and `tasks/` tree. No dashboard state was changed.
- The completed `rust_supply_chain_auditor` report
  `.superpowers/sdd/todo/task-1-supply-audit.md` approves exactly:

  ```toml
  ratatui = { version = "=0.30.2", default-features = false, features = ["crossterm_0_29"] }
  crossterm = { version = "=0.29.0", default-features = false, features = ["events"] }
  tokio = { version = "=1.49.0", default-features = false, features = ["macros", "rt-multi-thread", "signal", "sync", "time"] }
  tonic = { version = "=0.14.5", default-features = false, features = ["channel", "codegen"] }
  ```

  It found an authenticated crates.io-only external source graph, no
  typosquatting/dependency-confusion indicator, no adverse RustSec result for
  the dashboard lockfile, and no unexpected network, credential, or repository
  write behaviour in the active terminal stack. Ratatui and Crossterm have no
  build script; active transitive build scripts are platform/compiler probes
  confined to normal `OUT_DIR`/cfg work. Tokio 1.49.0 and Tonic 0.14.5 are
  already present in Harold; the dashboard feature selections are subsets of
  Harold's existing unified selections and add no capability.
- Approval conditions are binding for subsequent tasks: retain those exact pins
  and feature sets; do not enable Crossterm defaults, `osc52`, `use-dev-tty`,
  or another Ratatui backend without a new audit; keep the dashboard
  unprivileged because Crossterm may use the fixed-argument `tput` fallback via
  `PATH`; after the authorised workspace manifest/lockfile change, run
  `cargo audit --no-fetch --file Cargo.lock` using a current RustSec DB and
  confirm final-lock `lru >= 0.18.2` and `h2 >= 0.4.16`. The auditor reviewed
  the standalone resolution at exactly `lru 0.18.2` and Harold's existing
  `h2 0.4.18`.

### 2026-08-24 — Task 2 standalone dashboard checkpoint

- Recorded the approved source inventory before staging. `git status
  --porcelain=v2` showed exactly six modified tracked files (`Cargo.toml`,
  `examples/dashboard_demo.rs`, `src/app.rs`, `src/runtime.rs`,
  `src/terminal.rs`, and `src/ui.rs`) plus `README.md`,
  `examples/terminal_fault_harness.rs`, `src/terminal/fault_harness.rs`, and
  `tasks/` as untracked. The pruned file inventory contained `.gitignore`,
  `Cargo.lock`, `Cargo.toml`, `README.md`, two examples, eleven `src/` files,
  `tasks/lessons.md`, and the dashboard spec, plan, todo, screen ledger,
  fixture, HTML reference, and four PNG screenshots; `.git/` and `target/`
  were excluded.
- The exact offline gate passed from `/Users/kahgeh/Dev/p/tmx-agent-dash`:
  `cargo fmt --all -- --check`; `cargo test --all-targets --all-features
  --offline` (109 library tests plus 2 `dashboard_demo` example tests passed;
  `src/main.rs` and `terminal_fault_harness` each ran 0 tests); `cargo clippy
  --all-targets --all-features --offline -- -D warnings`; `cargo build
  --release --all-targets --all-features --offline`; and `git diff --check`.
- Staged only `.gitignore Cargo.lock Cargo.toml README.md examples src tasks`.
  `git diff --cached --check` passed and the index held exactly 28 tracked
  paths: the original `.gitignore`, `Cargo.lock`, and 6 unchanged `src/` paths;
  the six modified tracked paths; and the 14 approved new files, including the
  four screenshots. No `.git/`, `target/`, or unrelated path entered the index.
- `DASHBOARD_CHECKPOINT` is
  `e18fd04640bfe35bd0ae63e7d2e2348c5e333b07`
  (`fix: harden dashboard terminal lifecycle`), a 20-file change with 3,026
  insertions and 33 deletions.
- Recovery verification passed: source `git status --short --branch` reports
  only `## main`; the five-commit chain is `e18fd04`, `dc7c491`, `1bbb387`,
  `42bb384`, and `c45dc1a`; and `git show --stat --oneline HEAD` matches the
  reviewed 20-file checkpoint inventory.

### 2026-08-24 — Task 3 non-squashed dashboard history import

- Destination readiness passed before mutation: `tmx-agent-dash/` did not
  exist; `git status --short --branch` reported only
  `## feat/move-tmx-agent-dash`; `git rev-parse HEAD:events` returned
  `a23c70c13588beeb9ebd4a248d4b91f5bad8bd46`; and `shasum -a 256 .gitmodules`
  returned `8a32225f720343cd59eb8d0d6219e42145da61c29680a49865dbf0e2db1ee60d`.
- Added temporary remote `tmx-agent-dash-import` pointing to
  `/Users/kahgeh/Dev/p/tmx-agent-dash`, fetched `main`, and verified its
  remote-tracking ref exactly matched source `HEAD`
  `e18fd04640bfe35bd0ae63e7d2e2348c5e333b07`.
- `git subtree add --prefix=tmx-agent-dash tmx-agent-dash-import main -m
  "chore: import tmx-agent-dash history"` produced merge commit
  `af350775b88031c069960fbd416fba93179d5226`, with first parent
  `4a0335010b8b004ba64f06980c6938c5d7876c9c` and second parent
  `e18fd04640bfe35bd0ae63e7d2e2348c5e333b07`. Its subtree metadata records
  `git-subtree-dir: tmx-agent-dash`, the same mainline parent, and the same
  split checkpoint; no `--squash` was used or reported.
- Removed `tmx-agent-dash-import`. Each required ancestry assertion exited
  zero: `c45dc1a`, `42bb384`, `1bbb387`,
  `dc7c4919c3d589ef434ececca4b6a9562cfc127c`, and
  `e18fd04640bfe35bd0ae63e7d2e2348c5e333b07`. The resulting graph shows the
  five-commit dashboard chain beneath `af35077`'s second parent.
- Exclusion verification passed: `tmx-agent-dash/.git` and
  `tmx-agent-dash/target` are absent, `tmx-agent-dash/Cargo.lock` is present,
  and the worktree was clean immediately after the subtree commit.

### 2026-08-24 — Task 4 blocked during offline workspace resolution

- RED passed before manifest edits: with `set -o pipefail`,
  `cargo metadata --offline --no-deps --format-version 1 | rg
  '"name":"tmx-agent-dash"'` exited 1 and emitted no match.
- Applied exactly the two authorised manifest edits: the root workspace now
  names `tmx-agent-dash` as its fourth member while retaining resolver `"2"`,
  and the dashboard's local dependency is
  `harold-api = { path = "../harold-api" }`. The audited direct pins,
  default-feature settings, and feature lists are byte-for-byte unchanged.
- Removed `tmx-agent-dash/Cargo.lock`, moved all ten dashboard task paths to
  `tasks/tmux-agent-dashboard/`, appended all 31 dashboard lesson bullets
  absent from Harold while preserving every existing Harold lesson, and removed
  the imported `tmx-agent-dash/tasks` tree. A comparison against the imported
  lessons at `HEAD` returned no unmerged bullet.
- The required GREEN command did not complete. `cargo metadata --offline
  --format-version 1` began updating the root lockfile, then exited 101 with
  `failed to download bitflags v1.3.2` and `attempting to make an HTTP
  request, but --offline was specified`. No cached
  `bitflags-1.3.2.crate` exists under the active Cargo registry cache.
- The partially rewritten root lockfile resolves `lru 0.18.2`, satisfying its
  audit floor, but still contains `h2 0.4.13`, below the required
  `h2 >= 0.4.16` floor. The pre-Task-4 root lockfile also contained
  `h2 0.4.13`; cached archives for both `h2 0.4.13` and `h2 0.4.18`
  exist, but no further Cargo resolution was attempted after the hard stop.
- Per the Task 4 brief, no network-enabled Cargo command, workspace check,
  GREEN proof, staging, or commit was attempted. Steps 5–7 remain unchecked.
  The in-progress manifest, lockfile, task relocation, and lesson merge are
  intentionally preserved for coordinator recovery. `git diff --check`
  passes; `events` remains at
  `a23c70c13588beeb9ebd4a248d4b91f5bad8bd46`, and `.gitmodules` retains
  SHA-256
  `8a32225f720343cd59eb8d0d6219e42145da61c29680a49865dbf0e2db1ee60d`.

### 2026-08-24 — Task 4 approved recovery and resolved-feature blocker

- The supply auditor's follow-up ruling conditionally approved restoring only
  the task-generated partial root lockfile, resolving exactly `h2 0.4.18`,
  enforcing a package-coordinate union gate, fetching the accepted locked graph
  once, and returning to offline/no-fetch verification.
- `git diff --check` passed before recovery. `git restore --source=HEAD --
  Cargo.lock` discarded only the partial lockfile; all manifest, task,
  relocation, lesson, and deletion changes were preserved.
- The sole network-enabled resolution command, `cargo update -p h2 --precise
  0.4.18`, added only the audited dashboard closure, replaced `h2 0.4.13`
  with `0.4.18`, and unified `bitflags 2.11.0` to the dashboard-audited
  `2.13.1`. It did not reproduce the earlier unrelated OpenSSL,
  wasm-bindgen, Windows, Antithesis, generator, linkme, or io-uring drift.
- Before any archive fetch, all 471 candidate
  name/version/source/checksum tuples were compared with the 534-coordinate
  union of `HEAD:Cargo.lock`, the standalone dashboard lock, and the approved
  `h2 0.4.18` coordinate. The unapproved set was empty. The candidate
  `h2 0.4.18` coordinate and checksum
  `839c0e8a181239723652be9062bb56ca5bf5f64011f73b623f6f4fc59086a228`
  exactly match the audited standalone lock.
- The ruling prose incorrectly associates
  `0cc23270f6e1808e30a928bdc84dea0b9b4136a8bc82338574f23baf47bbd280`
  with `h2 0.4.18`; that is the adjacent `glob 0.3.3` checksum in both the
  candidate and standalone locks. The authoritative lock-union gate still
  passed without accepting the erroneous tuple.
- The pre-fetch `cargo audit --no-fetch --file Cargo.lock` reported only the
  explicitly exempt pre-existing `paste 1.0.15` unmaintained warning.
  `cargo fetch --locked` was then run exactly once and downloaded the missing
  locked archives, including unchanged `bitflags 1.3.2`, with Cargo checksum
  verification.
- The locked offline GREEN gate passed: `cargo metadata --offline --locked
  --format-version 1` reported exactly `events`, `harold`, `harold-api`,
  and `tmx-agent-dash`; `cargo check --workspace --all-targets
  --all-features --offline --locked` completed successfully; and the final
  `cargo audit --no-fetch --file Cargo.lock` again reported only the allowed
  `paste 1.0.15` warning.
- Membership and relocation assertions passed, and `cargo tree -p
  tmx-agent-dash --depth 1 --offline --locked` showed exactly Crossterm
  `0.29.0`, local `harold-api 0.1.0`, Ratatui `0.30.2`, Tokio `1.49.0`,
  and Tonic `0.14.5`. The root lock now contains `lru 0.18.2` and
  `h2 0.4.18`.
- The final resolved-feature assertion failed the explicit audit condition.
  Despite the exact direct declarations and `default-features = false`,
  Ratatui `0.30.2` depends on `ratatui-crossterm 0.1.2` without disabling
  its defaults; that package likewise depends on Crossterm without disabling
  defaults. The resolved graph therefore enables Crossterm
  `bracketed-paste, default, derive-more, events, windows` and
  `ratatui-crossterm/default` plus `underline-color`. Re-running the tree
  with `--no-default-features` produced the same transitive activation.
- `osc52`, `use-dev-tty`, async `event-stream`, and Ratatui's Termina,
  Termion, and Termwiz backends remain absent, but the required no-Crossterm-
  defaults condition is not met. Step 6 and the commit remain unchecked. No
  staging or commit was attempted; changing the dependency surface requires a
  renewed supply-chain ruling and implementation decision.

### 2026-08-24 — Task 4 corrected supply ruling and completion

- The supply auditor's correction supersedes the earlier rejection of
  Crossterm defaults. It confirms that the exact standalone dashboard already
  resolved `ratatui/crossterm_0_29 -> ratatui-crossterm/default ->
  crossterm/default`; this is approved upstream feature composition, not
  workspace drift. The direct declarations remain immutable and unchanged.
- The correction also confirms the authoritative `h2 0.4.18` checksum as
  `839c0e8a181239723652be9062bb56ca5bf5f64011f73b623f6f4fc59086a228`;
  the earlier `0cc23270...` value was the adjacent `glob 0.3.3` checksum.
  The final lock contains the corrected `h2` identity and remains above the
  RustSec `>= 0.4.16` remediation floor.
- Fresh locked offline metadata asserted the exact approved feature sets:
  Crossterm `bracketed-paste,default,derive-more,events,windows`; Ratatui
  `crossterm,crossterm_0_29,std`; and Ratatui-Crossterm
  `crossterm_0_29,default,underline-color`.
- The same assertion proved absence of Crossterm `osc52`, `use-dev-tty`,
  `event-stream`, and `serde`, plus Ratatui `termina`, `termion`,
  `termwiz`, `calendar`, `all-widgets`, `palette`,
  `portable-atomic`, `scrolling-regions`, and `unstable`.
- Exact locked versions are Ratatui `0.30.2`, Crossterm `0.29.0`, Tokio
  `1.49.0`, Tonic `0.14.5`, `lru 0.18.2`, and `h2 0.4.18`.
- Fresh completion gates passed without network access:
  `cargo metadata --offline --locked --format-version 1` reported exactly
  `events`, `harold`, `harold-api`, and `tmx-agent-dash`;
  the no-deps member assertion matched; all relocation assertions passed;
  `cargo tree -p tmx-agent-dash --depth 1 --offline --locked` showed exactly
  the five intended direct dependencies; and `cargo check --workspace
  --all-targets --all-features --offline --locked` finished successfully.
- `cargo audit --no-fetch --file Cargo.lock` scanned 471 dependencies and
  reported only the explicitly approved pre-existing Turso transitive
  `RUSTSEC-2024-0436` warning for unmaintained `paste 1.0.15`.
- Task 4 is complete. Steps 1–7 are checked; the approved workspace, lock,
  task-record, and lesson changes are ready for the planned
  `build: integrate dashboard workspace crate` commit.

### 2026-08-24 — Task 5 dashboard deployment and documentation

- The literal RED command, `make -n deploy | rg
  'target/release/tmx-agent-dash|bin/tmx-agent-dash'`, exited 1 in this
  worktree because the untracked `.env` prerequisite is absent: Make reported
  `No rule to make target '.env', needed by 'deploy'. Stop.` before it could
  render the deploy recipe. A non-mutating equivalent, `make -n
  --assume-old=.env deploy | rg 'target/release/tmx-agent-dash|bin/tmx-agent-dash'`,
  then exited 1 with no matches, proving the pre-change recipe had no dashboard
  deployment effect.
- Added `INSTALL_DIR`, preserved Harold's effective install path as
  `$(INSTALL_DIR)/harold`, and added the exact dashboard rollback copy, install
  copy, and code-sign effects after `mkdir -p $(DEPLOY_DIR)` and before
  `pkill`. The root workspace `build` prerequisite remains first; Make does not
  launch `tmx-agent-dash`.
- The literal GREEN filter is similarly blocked by the absent `.env` after
  printing setup-codesign output (exit 2). With `.env` assumed old, its exact
  relevant dry-run order was: `cargo build --release`; optional
  `tmx-agent-dash.pre-deploy` copy; copy from
  `target/release/tmx-agent-dash`; dashboard `codesign`; Harold `pkill`; and
  Harold `codesign`. A launch assertion found no dashboard launch command.
- Root README now introduces the dashboard and gives the exact deploy/start
  commands. The package README now identifies the containing workspace and
  `../harold-api`, documents root `make build` and `make deploy`, retains its
  standalone `cargo build --release` and `cargo run` commands, and preserves
  its behavioral sections. `git diff --check` passed before staging.

### 2026-08-24 — Task 5 Fix Round 1 package-relative build path

- Cargo metadata reports the containing workspace target directory as
  `/Users/kahgeh/Dev/p/harold/.worktrees/move-tmx-agent-dash/target`.
  Because the standalone development command is explicitly run from
  `tmx-agent-dash/`, its resulting executable is correctly documented as
  `../target/release/tmx-agent-dash`, not
  `target/release/tmx-agent-dash`.

### 2026-08-24 — Task 6 history, content, workspace, and security verification

- Fresh immutable-boundary assertions all exited zero: `HEAD:events` is
  `a23c70c13588beeb9ebd4a248d4b91f5bad8bd46`; `.gitmodules` SHA-256 is
  `8a32225f720343cd59eb8d0d6219e42145da61c29680a49865dbf0e2db1ee60d`;
  imported `tmx-agent-dash/.git` and `tmx-agent-dash/target` are absent; and
  the recovery source repository remains at
  `/Users/kahgeh/Dev/p/tmx-agent-dash/.git`.
- The source checkpoint is
  `e18fd04640bfe35bd0ae63e7d2e2348c5e333b07`. Fresh
  `git merge-base --is-ancestor` assertions exited zero for all five required
  dashboard commits: `c45dc1a`, `42bb384`, `1bbb387`,
  `dc7c4919c3d589ef434ececca4b6a9562cfc127c`, and the checkpoint.
  `af350775b88031c069960fbd416fba93179d5226` remains the non-squashed subtree
  import merge.
- Extracted the source checkpoint with `git archive` into a disposable
  `/tmp/harold-task6.G5lmLr` directory. `diff -qr` found no differences after
  excluding the approved integrated files (`Cargo.toml`, `Cargo.lock`,
  `README.md`, and `tasks`); relocated
  `tasks/tmux-agent-dashboard` matches byte-for-byte. The only Cargo manifest
  hunk changes `harold-api` from `../harold/harold-api` to `../harold-api`.
  The separately inspected README hunk is limited to containing-workspace
  prerequisites, root `make build`/`make deploy`, and the correct package-run
  output path `../target/release/tmx-agent-dash`.
- Fresh offline, locked workspace gate commands all exited zero:
  `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets
  --all-features --offline --locked -- -D warnings`; `cargo test --workspace
  --all-targets --all-features --offline --locked`; `cargo build --workspace
  --all-targets --all-features --offline --locked`; `cargo build --workspace
  --release --all-targets --all-features --offline --locked`; and `cargo doc
  --workspace --no-deps --all-features --offline --locked`. The test gate
  passed 315 tests across all targets: 52 `events`, 148 `harold`, 4
  `harold-api`, and 111 `tmx-agent-dash`; every other target ran 0 tests.
- `cargo audit --no-fetch --file Cargo.lock` exited zero, loaded 1,099 local
  advisories, and scanned 471 locked dependencies. It reports exactly one
  allowed, pre-existing warning: unmaintained `paste 1.0.15`
  (`RUSTSEC-2024-0436`) through the Turso dependency chain; no advisory failure
  occurred. `cargo audit` has no `--locked` option, so it was pointed directly
  at the approved root `Cargo.lock` without fetching.
- Fresh `git diff --check` was silent and `git status --short --branch` showed
  only `## feat/move-tmx-agent-dash` before this evidence record. The recent
  graph includes the complete imported five-commit chain beneath `af35077`.
  `git diff --stat 811244a..HEAD` reports the approved 34-file migration
  surface, and the full `git diff --submodule=log 811244a..HEAD` has no
  `events` gitlink hunk; a targeted `-- events` assertion is empty. The
  `events` submodule is unchanged.
