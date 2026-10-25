#!/usr/bin/env bash
# Checks the Cargo feature mixes that `cargo test --workspace --locked` (mac-defaults) does not
# reach: linux-defaults, no feature, and each feature alone. Prints each command, stops at the
# first failure and names it. Run it before every merge and every release.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)"

features=(imessage calls)

run() {
    echo "check_features: $*"
    if ! "$@"; then
        echo "check_features: failed: $*" >&2
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
