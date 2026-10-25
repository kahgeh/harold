#!/usr/bin/env bash
# Checks the Cargo feature mixes that `cargo test --workspace --locked` (mac-defaults) does not
# reach: linux-defaults, no feature, and each feature alone. Prints each command, stops at the
# first failure and names it. It also fails when a build prints a compiler warning for the
# harold crate. It reads the output and sets no RUSTFLAGS, so the builds keep their cache.
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

run() {
    echo "check_features: $*"
    # No colour codes, so the warning test sees the line start even when CARGO_TERM_COLOR=always.
    CARGO_TERM_COLOR=never "$@" 2>&1 | tee "$log"
    if [ "${PIPESTATUS[0]}" -ne 0 ]; then
        echo "check_features: failed: $*" >&2
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
