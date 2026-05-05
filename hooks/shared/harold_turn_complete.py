from __future__ import annotations

import json
import os
import re
import socket
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path


HAROLD_ADDR = os.getenv("HAROLD_ADDR", "localhost:50060")
HAROLD_PROTO = Path.home() / "bin/harold/harold.proto"
HAROLD_BINARY = Path.home() / "bin/harold/harold"


@dataclass(frozen=True)
class TurnComplete:
    cwd: str
    last_user_prompt: str
    assistant_message: str


def clean_text(value: str, limit: int) -> str:
    value = re.sub(r"[^\x09\x0a\x0d\x20-\x7e]", "", value)
    value = re.sub(r"\s+", " ", value).strip()
    return value[:limit]


def get_pane_info() -> tuple[str, str]:
    pane_id = os.environ.get("TMUX_PANE", "")
    if not pane_id:
        return "", "unknown"

    try:
        result = subprocess.run(
            [
                "tmux",
                "display-message",
                "-t",
                pane_id,
                "-p",
                "#{session_name}:#{window_index}.#{pane_index}",
            ],
            capture_output=True,
            text=True,
            timeout=3,
        )
        if result.returncode == 0 and result.stdout.strip():
            return pane_id, clean_text(result.stdout.strip(), 120)
    except Exception:
        pass

    return pane_id, "unknown"


def get_main_context(cwd: str) -> str:
    git_env = os.environ.copy()
    git_env["GIT_OPTIONAL_LOCKS"] = "0"

    try:
        branch = subprocess.run(
            ["git", "rev-parse", "--abbrev-ref", "HEAD"],
            cwd=cwd,
            env=git_env,
            capture_output=True,
            text=True,
            timeout=2,
        ).stdout.strip()
        if branch and branch != "main":
            return clean_text(branch, 120)

        url = subprocess.run(
            ["git", "config", "--get", "remote.origin.url"],
            cwd=cwd,
            env=git_env,
            capture_output=True,
            text=True,
            timeout=2,
        ).stdout.strip()
        if url:
            name = url.rstrip("/").removesuffix(".git").rsplit("/", 1)[-1]
            if name:
                return clean_text(name, 120)
    except Exception:
        pass

    return clean_text(os.path.basename(cwd) or "unknown", 120)


def ensure_harold_running() -> None:
    try:
        host, port = HAROLD_ADDR.rsplit(":", 1)
        with socket.create_connection((host, int(port)), timeout=1):
            return
    except Exception:
        pass

    subprocess.Popen(
        [str(HAROLD_BINARY)],
        cwd=str(HAROLD_BINARY.parent),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    time.sleep(1)


def call_harold(
    pane_id: str,
    pane_label: str,
    last_user_prompt: str,
    assistant_message: str,
    main_context: str,
) -> None:
    payload = json.dumps(
        {
            "pane_id": pane_id,
            "pane_label": pane_label,
            "last_user_prompt": last_user_prompt,
            "assistant_message": assistant_message,
            "main_context": main_context,
        }
    )
    subprocess.run(
        [
            "grpcurl",
            "-plaintext",
            "-import-path",
            str(HAROLD_PROTO.parent),
            "-proto",
            HAROLD_PROTO.name,
            "-d",
            payload,
            HAROLD_ADDR,
            "harold.Harold/TurnComplete",
        ],
        capture_output=True,
        timeout=10,
    )


def notify_harold(turn: TurnComplete) -> None:
    pane_id, pane_label = get_pane_info()
    ensure_harold_running()
    call_harold(
        pane_id=pane_id,
        pane_label=pane_label,
        last_user_prompt=clean_text(turn.last_user_prompt, 500),
        assistant_message=clean_text(turn.assistant_message, 2000),
        main_context=get_main_context(turn.cwd),
    )
