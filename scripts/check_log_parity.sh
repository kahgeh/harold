#!/usr/bin/env bash
# Shows whether ClickHouse holds each line that harold.log holds.
#
# Usage: scripts/check_log_parity.sh <hours>
# Env:   HAROLD_LOG  the log file (default $HOME/bin/harold/harold.log)
#        LOGQ        the command that reads SQL on stdin and prints the result. It defaults to logq, found on PATH
#                    (the chief-of-pods bin/logq). Its first output line must be a plain number.
#        NOW         an ISO time, for tests that fix the clock (default: the real time)
# Each line of the last <hours> is one of these kinds:
#   allowed  a health line, a line of a telemetry crate, or a line with no timestamp at the start
#   unsent   a line that ends with unsent=true
#   gap      any other line
# Each unsent line is looked up in otel.otel_logs (same message, time within 1 second): resent or not resent.
# The script prints the count of each kind, then the time and message of each gap line and each not-resent line.
# It never prints a field value. A message is cut at 200 characters.
# A span prefix with fields (span{k=v}: ) before the target is accepted. A span prefix with no fields is not.
# Exit code: 0 when there is no gap line, 1 when there is a gap line (the watch uses this), 2 on a usage, logq or other internal error.
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
LINE = re.compile(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z)\s+([A-Z]+)\s+(?:[A-Za-z0-9_]+\{[^}]*\}:(?:[A-Za-z0-9_]+\{[^}]*\}:)*\s?)?([^\s:]+(?:::[^\s:]+)*):\s?(.*)$")
FIELD = re.compile(r" [A-Za-z_][A-Za-z0-9_.]*=")
HEALTH = ("telemetry export failing", "telemetry export recovered")
EXACT = ("opentelemetry", "tonic", "hyper", "hyper_util", "h2", "tower", "tower_http", "http")
PREFIXES = ("opentelemetry_", "opentelemetry-", "tonic_")
UNSENT = " unsent=true"


def parse_time(text):
    head, _, fraction = text[:-1].partition(".")
    base = datetime.strptime(head, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)
    return base + timedelta(microseconds=int((fraction + "000000")[:6]))


def is_telemetry(target):
    crate = target.split("::")[0]
    return crate in EXACT or crate.startswith(PREFIXES)


def message_of(rest):
    if rest.endswith(UNSENT):
        rest = rest[: -len(UNSENT)]
    match = FIELD.search(rest)
    return rest[: match.start()] if match else rest


def literal(text):
    return "'" + text.replace("\\", "\\\\").replace("'", "\\'") + "'"


def fail(text):
    print("check_log_parity: " + text, file=sys.stderr)
    sys.exit(2)


def resent(time_text, message):
    stamp = "parseDateTime64BestEffort(" + literal(time_text) + ", 6)"
    sql = (
        "SELECT count() FROM otel.otel_logs WHERE ServiceName = 'harold' AND Body = " + literal(message)
        + " AND Timestamp >= " + stamp + " - INTERVAL 2 SECOND AND Timestamp <= " + stamp + " + INTERVAL 2 SECOND"
        + " AND abs(toUnixTimestamp64Micro(Timestamp) - toUnixTimestamp64Micro(" + stamp + ")) <= 1000000"
    )
    try:
        done = subprocess.run(os.environ["LOGQ"], input=sql, capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.SubprocessError):
        fail("logq did not run")
    if done.returncode != 0:
        fail("logq failed")
    lines = done.stdout.strip().split("\n")
    first = lines[0].strip()
    if not re.fullmatch(r"[0-9]+", first):
        fail("logq printed no number")
    return int(first) > 0


def main():
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
                    not_resent.append((time_text, message[:200]))
            else:
                gaps.append((time_text, message[:200]))

    print(f"allowed: {allowed}")
    print(f"unsent resent: {unsent_resent}")
    print(f"unsent not resent: {len(not_resent)}")
    print(f"gap: {len(gaps)}")
    for time_text, message in not_resent:
        print(f"not resent: {time_text} {message}")
    for time_text, message in gaps:
        print(f"gap: {time_text} {message}")
    return 1 if gaps else 0


try:
    code = main()
except SystemExit:
    raise
except Exception as error:
    fail("internal error: " + type(error).__name__)
sys.exit(code)
PY
