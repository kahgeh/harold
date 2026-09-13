#!/usr/bin/env python3

from __future__ import annotations

import json
import os
import sys
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

from harold_turn_complete import TurnComplete, notify_harold  # noqa: E402


def read_json_stdin() -> dict[str, Any] | None:
    try:
        data = json.load(sys.stdin)
        return data if isinstance(data, dict) else None
    except json.JSONDecodeError:
        return None


def text_from_content(content: Any) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""

    parts: list[str] = []
    for block in content:
        if isinstance(block, str):
            parts.append(block)
        elif isinstance(block, dict):
            text = block.get("text")
            if isinstance(text, str):
                parts.append(text)
    return "\n".join(parts)


def extract_last_user_prompt(transcript_path: str | None) -> str:
    if not isinstance(transcript_path, str) or not transcript_path or not os.path.isfile(transcript_path):
        return ""

    prompts: list[str] = []
    with open(transcript_path, encoding="utf-8") as f:
        for line in f:
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue

            if not isinstance(data, dict):
                continue

            payload = data.get("payload")
            if not isinstance(payload, dict):
                continue
            prompt = ""
            if data.get("type") == "event_msg" and payload.get("type") == "user_message":
                prompt = text_from_content(payload.get("message"))
            elif data.get("type") == "response_item" and payload.get("type") == "message":
                if payload.get("role") == "user":
                    prompt = text_from_content(payload.get("content"))

            prompt = prompt.strip()
            if prompt and not prompt.startswith("<") and "tool_use_id" not in prompt:
                prompts.append(prompt)

    return prompts[-1] if prompts else ""


def extract_last_assistant_message(transcript_path: str | None) -> str:
    if not isinstance(transcript_path, str) or not transcript_path or not os.path.isfile(transcript_path):
        return ""

    last_text = ""
    with open(transcript_path, encoding="utf-8") as f:
        for line in f:
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue

            if not isinstance(data, dict):
                continue

            payload = data.get("payload")
            if not isinstance(payload, dict):
                continue
            text = ""
            if data.get("type") == "event_msg" and payload.get("type") == "agent_message":
                text = text_from_content(payload.get("message"))
            elif data.get("type") == "response_item" and payload.get("type") == "message":
                if payload.get("role") == "assistant":
                    text = text_from_content(payload.get("content"))

            if text.strip():
                last_text = text.strip()

    return last_text


def main() -> int:
    try:
        input_data = read_json_stdin()
        if not input_data:
            return 0
        transcript_path = input_data.get("transcript_path")
        assistant_message = input_data.get("last_assistant_message")
        assistant_message = assistant_message.strip() if isinstance(assistant_message, str) else ""
        if not assistant_message:
            assistant_message = extract_last_assistant_message(transcript_path)

        cwd = input_data.get("cwd")
        notify_harold(
            TurnComplete(
                cwd=cwd if isinstance(cwd, str) else os.getcwd(),
                last_user_prompt=extract_last_user_prompt(transcript_path),
                assistant_message=assistant_message,
            )
        )
    except Exception:
        return 0

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
