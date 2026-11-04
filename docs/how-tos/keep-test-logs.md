# Keep test logs

A test run keeps one log with its commit, its command and its result. Use `scripts/test-run` for each test run. The format is in [ADR 29](../adr/0029-a-test-run-keeps-one-log-in-one-format.md).

## Run a command

Run this command from the repository root or from a worktree:

```sh
scripts/test-run <spec id> <name> -- <command...>
```

The spec id and the name are kebab-case. The runner prints the output and writes it to the log. It exits with the exit code of the command.

The log is in the test log folder of the base checkout:

```
.superpowers/pod/test-logs/<spec id>/<UTC time>-<name>.log
```

The base checkout is the worktree of `main`. When a log of the same name exists, the runner adds `-2`, `-3` and so on. The runner never writes over a log.

## Run the Harold tests

Run the full test suite with the timing tests under the lock. The lock lets only one full suite run at a time on this Mac. `lockf` is a macOS and BSD tool.

Run this command:

```sh
lockf -k /tmp/pods-full-test.lock scripts/test-run <spec id> cargo-test -- \
  cargo test --workspace --locked --features harold/timing-tests,control-tower/timing-tests
```

## Run the scripts

Run the call test and the OpenTelemetry test through the runner:

```sh
scripts/test-run <spec id> call-test -- python3 scripts/call_test.py
scripts/test-run <spec id> otel-test -- python3 scripts/otel_test.py
```

The runner sets `TEST_RUN_DIR` and `TEST_RUN_STEM` for the script. The script then copies its files next to the log, after a pass and after a failure:

| File | Content |
| --- | --- |
| `<stem>-harold.log` | The log of the scratch Harold |
| `<stem>-browser.log` | The browser output (`otel_test.py` only) |
| `<stem>-collector.log` | The collector output (`otel_test.py` only) |
| `<stem>-<part>.withheld` | A note that a file held a secret, so the script did not copy it |

Each script scans a file for secrets before it copies the file. The script does not copy a file with a secret. The `-<part>.withheld` file names the pattern and never the match. Without the two variables, the scripts copy nothing.

## Tests that open a port or start a second Harold

Some tests open a port or start a second Harold. On this Mac they could reach the installed Harold. These tests run in a container test set, in a podman container with no route to the Mac.

A container test set is a file `scripts/container-tests/<set>.txt`. It lists one test name on each line. The runner skips blank lines and lines that start with `#`. The runner refuses an empty set and an unknown set name.

Run a set:

```sh
scripts/test-run <spec id> <name> --container <set> -- cargo test
```

The words after `--` show that this is a test run. The set decides which tests run. The command in the container is always:

```sh
cargo test --locked --offline -p harold --no-default-features --features harold/linux-defaults -- --ignored --exact <names>
```

The runner does these steps in order:

1. It refuses to run when the podman machine is stopped. It never starts the machine. Run `podman machine start` yourself.
2. It makes the podman network `harold-tests` (`--internal`) when the network is missing.
3. It runs `cargo fetch --locked --target aarch64-unknown-linux-gnu` on the Mac. The log holds the output and one line `# fetched: <crate-version list>` with the crates that are new in `~/.cargo/registry/cache`.
4. It runs the container. The container gets the worktree and the crate files as read-only mounts, and no Mac environment value.
5. It moves each result file of the test from `/out` to the test log folder as `<stem>-<part>`. A file with a secret goes through the same scan as in `scripts/call_test.py`. The runner withholds it and writes a `.withheld` note.
6. It counts the lines `test <name> ... ok`. The count uses the exact name. The test run fails and names each test that did not run.

The header of the test log has two extra lines:

- `container: <image id> from <base digest>`
- `dpkg: <package>=<version> ...`, the packages installed in the image, on one line

The `command` line shows the cargo command. It holds no environment value.

### Build the image

Build the image only after the audit row exists:

```sh
podman build -t harold-tests scripts/container-tests
```

Audit row 19 in `docs/references/dependency-audits.md` covers the base image and the three apt packages. Ask for a new audit when one of these happens:

- The Containerfile changes.
- The toolchain leaves 1.97.1.
- The date is after 2026-12-05.

## Read a log

List the runs of one spec id:

```sh
scripts/test-run show <spec id>
```

The command prints one row for each run: time, name, commit, seconds, result and `features`. The `features` column shows `timing` when the command holds `timing-tests`. The command skips logs with no header and the part files.

Read one log:

```sh
less .superpowers/pod/test-logs/<spec id>/<file>.log
```

Find the failures:

```sh
grep -n 'FAILED\|panicked' .superpowers/pod/test-logs/<spec id>/<file>.log
```

The header names the commit that the run used. The footer holds the result.

## Stop a run

The runner sends a SIGTERM on to the command. The runner does not send any other signal on to the command. A signal that stops the run gives `result: stopped`.

## Keep and delete

The logs stay on the local machine. Git ignores the folder. Nothing goes to git or S3. The logs stay until the pod closes. Delete them by hand.
