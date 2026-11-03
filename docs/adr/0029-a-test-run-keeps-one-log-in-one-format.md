# 29. A test run keeps one log in one format

Date: 2026-11-03
Status: Accepted (test-run-logs).

## Context

Test logs had no commit, no command and no result. Each product kept them in its own way. One `cargo test` log had 3,270 lines. To see if the run passed, a reader searched for each `test result:` line. A passing script run deleted the scratch Harold's log.

## Decision

1. A test run keeps one log at `<base checkout>/.superpowers/pod/test-logs/<spec id>/<UTC time>-<name>.log`. `<UTC time>` has the form `YYYYMMDDTHHMMSSZ`. The spec id and the name are kebab-case. The base checkout is the worktree of `main`. When no such worktree exists, the runner writes nothing.
2. The log starts with the header `# test-run: 1`. Then it has one `# key: value` line each for `id`, `name`, `command`, `cwd`, `commit` and `started`. The `command` line holds the arguments as typed and never an environment value. The `commit` line adds ` dirty` when the tree has changes.
3. The log ends with a footer: `finished`, `duration_s`, `exit` and `result`. `result` is `passed`, `failed` or `stopped`. When the output has `test result:` lines, `result` adds their sums.
4. The runner sets `TEST_RUN_DIR` and `TEST_RUN_STEM` for the command. A file that a run adds sits next to the log as `<UTC time>-<name>-<part>.<ext>`.
5. The runner never writes over a log that exists already. It adds `-2`, `-3` and so on to the name.
6. `test-run show <spec id>` lists only files whose first line is `# test-run: 1`. It skips headerless logs and part files.
7. A copied file holds no secret. The scripts do not copy a file that matches a secret pattern. A `.withheld` file names the pattern and never the match.

## Consequences

A report quotes one line per run. Every product's runner writes this format. The logs stay local until the pod closes, and a person deletes them by hand. The logs never go to git or S3. The other products copy the script or call a shared one.
