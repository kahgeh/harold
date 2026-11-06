#!/usr/bin/env bash
# Shows whether ClickHouse holds each line that harold.log holds.
#
# Usage: scripts/check_log_parity.sh <hours>
# Env:   HAROLD_LOG  the log file (default $HOME/bin/harold/harold.log)
#        LOGQ        the command that reads SQL on stdin and prints the result (default logq, from PATH)
#        NOW         an ISO time, for tests that fix the clock (default: the real time)
# Each line of the last <hours> is one of these kinds:
#   allowed  a health line, a line of a telemetry crate, or a line with no timestamp at the start
#   unsent   a line that ends with unsent=true
#   gap      any other line
# Each unsent line is looked up in otel.otel_logs (same message, time within 1 second): resent or not resent.
# The script prints the count of each kind, then the time and message of each gap line and each not-resent line.
# It never prints a field value.
# Exit code: 0 when there is no gap line, 1 when there is a gap line (the watch uses this), 2 on a usage or logq error.
set -euo pipefail

if [ "$#" -ne 1 ] || ! [[ "$1" =~ ^[0-9]+$ ]]; then
    echo "usage: check_log_parity.sh <hours>" >&2
    exit 2
fi

export HOURS="$1"
export HAROLD_LOG="${HAROLD_LOG:-$HOME/bin/harold/harold.log}"
export LOGQ="${LOGQ:-logq}"

if [ ! -r "$HAROLD_LOG" ]; then
    echo "check_log_parity: cannot read the log file" >&2
    exit 2
fi

exec python3 -I - <<'PY'
import os
import re
import subprocess
import sys
from datetime import datetime, timedelta, timezone

ANSI = re.compile(r"\x1b\[[0-9;]*[@-~]")
LINE = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)\s+([A-Z]+)\s+([^\s:]+(?:::[^\s:]+)*):\s?(.*)$")
FIELD = re.compile(r" [A-Za-z_][A-Za-z0-9_.]*=")
HEALTH = ("telemetry export failing", "telemetry export recovered")
TELEMETRY_CRATES = ("opentelemetry", "tonic", "hyper", "hyper_util", "h2", "tower", "tower_http", "http")
UNSENT = " unsent=true"


def parse_time(text):
    head, _, fraction = text[:-1].partition(".")
    base = datetime.strptime(head, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)
    return base + timedelta(microseconds=int((fraction + "000000")[:6]))


def is_telemetry(target):
    crate = target.split("::")[0]
    return any(crate == name or (name == "opentelemetry" and crate.startswith(name)) for name in TELEMETRY_CRATES)


def message_of(rest):
    if rest.endswith(UNSENT):
        rest = rest[: -len(UNSENT)]
    match = FIELD.search(rest)
    return rest[: match.start()] if match else rest


def literal(text):
    return "'" + text.replace("\\", "\\\\").replace("'", "\\'") + "'"


def resent(time_text, message):
    sql = (
        "SELECT count() FROM otel.otel_logs WHERE Body = " + literal(message)
        + " AND abs(toUnixTimestamp64Micro(Timestamp) - toUnixTimestamp64Micro(parseDateTime64BestEffort("
        + literal(time_text) + ", 6))) <= 1000000"
    )
    done = subprocess.run(os.environ["LOGQ"], input=sql, capture_output=True, text=True, shell=False)
    if done.returncode != 0:
        print("check_log_parity: logq failed", file=sys.stderr)
        sys.exit(2)
    first = done.stdout.strip().split("\n")[0].strip() if done.stdout.strip() else ""
    return first.isdigit() and int(first) > 0


now_text = os.environ.get("NOW")
now = parse_time(now_text.rstrip("Z") + "Z") if now_text else datetime.now(timezone.utc)
cutoff = now - timedelta(hours=int(os.environ["HOURS"]))

allowed = 0
gaps = []
unsent_resent = 0
not_resent = []
with open(os.environ["HAROLD_LOG"], encoding="utf-8", errors="replace") as handle:
    for raw in handle:
        line = ANSI.sub("", raw.rstrip("\n"))
        match = LINE.match(line)
        if not match:
            if line.strip():
                allowed += 1
            continue
        time_text, _level, target, rest = match.groups()
        if parse_time(time_text) < cutoff:
            continue
        message = message_of(rest)
        if message.startswith(HEALTH) or is_telemetry(target):
            allowed += 1
        elif rest.endswith(UNSENT):
            if resent(time_text, message):
                unsent_resent += 1
            else:
                not_resent.append((time_text, message))
        else:
            gaps.append((time_text, message))

print(f"allowed: {allowed}")
print(f"unsent resent: {unsent_resent}")
print(f"unsent not resent: {len(not_resent)}")
print(f"gap: {len(gaps)}")
for time_text, message in not_resent:
    print(f"not resent: {time_text} {message}")
for time_text, message in gaps:
    print(f"gap: {time_text} {message}")
sys.exit(1 if gaps else 0)
PY
