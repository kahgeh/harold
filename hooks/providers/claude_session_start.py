#!/usr/bin/env python3
"""Claude Code SessionStart hook: tell Harold which agent holds a pod role.

Without POD_ROLE the hook does nothing. It never waits on Harold for more than 3 seconds
and it always exits 0, so an agent never waits on it.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

# Releases install the provider scripts beside the shared notifier.
HOOK_DIRECTORY = Path(__file__).resolve().parent
SHARED_DIRECTORY = (
    HOOK_DIRECTORY
    if (HOOK_DIRECTORY / "harold_turn_complete.py").is_file()
    else HOOK_DIRECTORY.parent / "shared"
)
sys.path.insert(0, str(SHARED_DIRECTORY))

from harold_turn_complete import HAROLD_BUNDLE, HAROLD_PROTO  # noqa: E402

CALL_SECONDS = 2
# The whole hook ends within 3 seconds: the walk up the parents takes at most 0.6 seconds
# (a 0.4 second budget and one command of at most 0.2) and the call at most 2.2.
WALK_LEVELS = 5
WALK_SECONDS = 0.4
AGENT_COMMAND = "claude"


def string_or_empty(value: Any) -> str:
    return value if isinstance(value, str) else ""


def pane_id() -> str:
    """The pane id of the agent: TMUX_PANE, then REX_BLOCK, else empty. It runs no command."""
    return os.environ.get("TMUX_PANE") or os.environ.get("REX_BLOCK") or ""


def process_row(pid: int) -> tuple[int, str] | None:
    """The parent pid and the command of process `pid`, or None."""
    try:
        done = subprocess.run(
            ["ps", "-o", "ppid=,comm=", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=0.2,
        )
        parent, _, command = done.stdout.strip().partition(" ")
        return int(parent), command.strip()
    except Exception:
        return None


def find_agent_pid(parent_pid: int, row: Any = process_row) -> int:
    """The first process from `parent_pid` up, at most 5 levels, whose command name is
    `claude`. The hook can run under a wrapper shell, so its parent may not be the agent.
    Falls back to `parent_pid` when none is found."""
    deadline = time.monotonic() + WALK_SECONDS
    pid = parent_pid
    for _ in range(WALK_LEVELS):
        if pid <= 1 or time.monotonic() > deadline:
            break
        found = row(pid)
        if found is None:
            break
        parent, command = found
        if os.path.basename(command) == AGENT_COMMAND:
            return pid
        pid = parent
    return parent_pid


def start_file(role: str, cwd: str) -> str:
    """The start file of the role in the pod folder, or empty when there is none."""
    if not cwd:
        return ""
    path = Path(cwd) / ".superpowers" / "pod" / "start" / f"{role}.json"
    return str(path) if path.is_file() else ""


def build_request(
    env: Any, payload: dict[str, Any], parent_pid: int, pane: str
) -> dict[str, Any] | None:
    """The PodAgentStarted request, or None when POD_ROLE is not set."""
    role = env.get("POD_ROLE", "")
    if not role:
        return None
    cwd = string_or_empty(payload.get("cwd"))
    return {
        "pod_role": role,
        "pod_product": env.get("POD_PRODUCT", ""),
        "pane_id": pane,
        "session_id": string_or_empty(payload.get("session_id")),
        "transcript_path": string_or_empty(payload.get("transcript_path")),
        "cwd": cwd,
        "agent_pid": parent_pid,
        "start_file": start_file(role, cwd),
        "source": string_or_empty(payload.get("source")),
    }


def grpc_address() -> str | None:
    """Harold's gRPC address from service.json, without starting Harold."""
    try:
        metadata = json.loads((HAROLD_BUNDLE / "service.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    address = metadata.get("grpc_addr") if isinstance(metadata, dict) else None
    return address if isinstance(address, str) and address.strip() else None


def call_harold(request: dict[str, Any], address: str) -> None:
    subprocess.run(
        [
            "grpcurl",
            "-plaintext",
            "-max-time",
            str(CALL_SECONDS),
            "-import-path",
            str(HAROLD_PROTO.parent),
            "-proto",
            HAROLD_PROTO.name,
            "-d",
            json.dumps(request),
            address,
            "harold.Harold/PodAgentStarted",
        ],
        capture_output=True,
        timeout=CALL_SECONDS + 0.2,
    )


def main() -> int:
    try:
        payload = json.load(sys.stdin)
        if not isinstance(payload, dict):
            return 0
        if not os.environ.get("POD_ROLE"):
            return 0
        address = grpc_address()
        if address is None:
            return 0
        request = build_request(os.environ, payload, find_agent_pid(os.getppid()), pane_id())
        if request is None:
            return 0
        call_harold(request, address)
    except Exception:
        return 0
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
