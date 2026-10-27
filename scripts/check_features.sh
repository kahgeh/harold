#!/usr/bin/env bash
# Checks the Cargo feature mixes that `cargo test --workspace --locked` (mac-defaults) does not
# reach: linux-defaults, no feature, and each feature alone. Prints each command, stops at the
# first failure and names it. It also fails when a build prints a compiler warning for the
# harold crate. It reads the output and sets no RUSTFLAGS, so the builds keep their cache.
# After a failure it prints a summary of the failing tests, panics and compiler errors as its last lines.
# Run it before every merge and every release.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)"

features=(imessage calls)

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

# A warning line is `warning: ...` or, with a code, `warning[E....]: ...`. The linker message
# and the per-crate count line are not compiler warnings of the code, so they do not count.
warnings_in_log() {
    grep -E '^warning(\[E[0-9]+\])?: ' "$log" | grep -vE '^warning: (linker stderr|`[^`]+` \(.*\) generated [0-9]+ warnings?)'
}

# The last thing a failed run prints: the failing tests, their panics, the counts and the compiler
# errors. Cargo prints these far above the end, so a caller that shows only the tail would miss them.
failure_summary() {
    echo "check_features: summary:"
    local lines
    lines="$(
        {
            grep -E '^test .* \.\.\. FAILED$' "$log"
            grep -E -A1 'panicked at' "$log"
            grep -E '^test result: ' "$log"
            grep -E -A1 '^error(\[E[0-9]+\])?: ' "$log"
        } | grep -v '^--$' | head -n 40
    )"
    if [ -z "$lines" ]; then
        echo "check_features: summary: no test or error line found"
        return
    fi
    echo "$lines"
}

run() {
    echo "check_features: $*"
    # No colour codes, so the warning test sees the line start even when CARGO_TERM_COLOR=always.
    CARGO_TERM_COLOR=never "$@" 2>&1 | tee "$log"
    if [ "${PIPESTATUS[0]}" -ne 0 ]; then
        echo "check_features: failed: $*" >&2
        failure_summary >&2
        exit 1
    fi
    if warnings_in_log >/dev/null; then
        echo "check_features: warning in: $*" >&2
        warnings_in_log >&2
        exit 1
    fi
}

run cargo check --locked --workspace --no-default-features --features harold/linux-defaults
run cargo check --locked -p harold --no-default-features
for feature in "${features[@]}"; do
    run cargo check --locked -p harold --no-default-features --features "$feature"
done
# The "built without" settings tests, which neither feature set reaches.
run cargo test --locked -p harold --no-default-features

echo "check_features: ok"
