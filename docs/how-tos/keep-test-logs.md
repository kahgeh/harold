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
