#!/bin/sh
set -eu
if ! command -v python3 >/dev/null 2>&1; then
    echo 'Python 3.9+ is required. Install Python, then rerun this command.' >&2
    exit 1
fi
exec python3 "$(dirname "$0")/install.py" "$@"
