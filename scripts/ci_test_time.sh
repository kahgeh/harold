#!/usr/bin/env bash
# Reads the output of `cargo test` on its input and prints each line unchanged.
# It warns when the main harold test binary runs for more than 900 seconds.
#
# The main binary starts at the cargo line `Running unittests src/main.rs (.../deps/harold-<hash>)`.
# The first `test result: ... finished in <n>s` line after it holds its time. The lines of the
# other binaries are not read. The exit status is always 0, so a pipe from `cargo test` keeps
# the exit status of `cargo test` (the step runs with `pipefail`).

limit=900

awk -v limit="$limit" '
  { print; fflush() }
  /Running +unittests .*src\/main\.rs \(.*deps\/harold-[0-9a-f]+\)/ { in_main = 1; next }
  in_main && /test result:.* finished in [0-9.]+s/ {
    in_main = 0
    line = $0
    sub(/.* finished in /, "", line)
    sub(/s.*/, "", line)
    if (line + 0 > limit) {
      printf "::warning::The main harold test binary took %s s. The warning limit is %d s.\n", line, limit
      fflush()
    }
  }
'
exit 0
