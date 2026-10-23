#!/usr/bin/env bash
# Fails when a tracked file shows the author's machine: a home folder, the folder layout, or a
# Claude scratch folder. Each pattern is built from parts so this file does not match itself.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

home="/Users/""kahgeh"
layout="Dev""/p/"
scratch="claude""-502"

if git grep -n -F -e "$home" -e "$layout" -e "$scratch" -- .; then
    echo "check_local_paths: tracked files above show this machine's paths; use /Users/you, ~/code or a placeholder" >&2
    exit 1
fi
echo "check_local_paths: ok"
