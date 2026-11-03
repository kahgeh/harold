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


def string_or_empty(value: Any) -> str:
    return value if isinstance(value, str) else ""


def extract_last_user_prompt_and_time(transcript_path: str | None) -> tuple[str, str]:
    if not isinstance(transcript_path, str) or not transcript_path or not os.path.isfile(transcript_path):
        return "", ""

    prompts: list[tuple[str, str]] = []
    with open(transcript_path, encoding="utf-8") as f:
        for line in f:
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue

            if not isinstance(data, dict):
                continue

            if data.get("type") != "user":
                continue

            message = data.get("message")
            if not isinstance(message, dict):
                continue

            prompt = text_from_content(message.get("content")).strip()
            if prompt and not prompt.startswith("<") and "tool_use_id" not in prompt:
                prompts.append((prompt, string_or_empty(data.get("timestamp"))))

    return prompts[-1] if prompts else ("", "")


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

            if data.get("type") != "assistant":
                continue

            message = data.get("message")
            if not isinstance(message, dict):
                continue

            text = text_from_content(message.get("content")).strip()
            if text:
                last_text = text

    return last_text


USAGE_FIELDS = (
    ("input_tokens", "input_tokens"),
    ("output_tokens", "output_tokens"),
    ("cache_read_input_tokens", "cache_read_tokens"),
    ("cache_creation_input_tokens", "cache_write_tokens"),
)


def is_tool_result_line(message: dict[str, Any]) -> bool:
    content = message.get("content")
    return isinstance(content, list) and any(
        isinstance(block, dict) and block.get("type") == "tool_result" for block in content
    )


def extract_turn_usage(transcript_path: str | None) -> dict[str, Any]:
    if not isinstance(transcript_path, str) or not transcript_path or not os.path.isfile(transcript_path):
        return {}

    # One assistant message can span several lines; each line repeats its usage.
    by_message: dict[str, dict[str, int]] = {}
    model = ""
    with open(transcript_path, encoding="utf-8") as f:
        for line in f:
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(data, dict):
                continue
            message = data.get("message")
            if not isinstance(message, dict):
                continue

            if data.get("type") == "user":
                if not is_tool_result_line(message):
                    by_message = {}
                    model = ""
                continue
            if data.get("type") != "assistant":
                continue

            if isinstance(message.get("model"), str) and message["model"]:
                model = message["model"]
            usage = message.get("usage")
            message_id = message.get("id")
            if not isinstance(usage, dict) or not isinstance(message_id, str) or not message_id:
                continue
            seen = by_message.setdefault(message_id, {})
            for source, target in USAGE_FIELDS:
                value = usage.get(source)
                if isinstance(value, int) and not isinstance(value, bool):
                    seen[target] = max(seen.get(target, 0), value)

    if not by_message:
        return {}
    totals = {target: sum(seen.get(target, 0) for seen in by_message.values()) for _, target in USAGE_FIELDS}
    totals["model"] = model
    return totals


def main() -> int:
    try:
        input_data = read_json_stdin()
        if not input_data:
            return 0
        if input_data.get("hook_event_name") == "SubagentStop":
            return 0

        transcript_path = input_data.get("transcript_path")
        assistant_message = input_data.get("last_assistant_message")
        assistant_message = assistant_message.strip() if isinstance(assistant_message, str) else ""
        if not assistant_message:
            assistant_message = extract_last_assistant_message(transcript_path)

        last_user_prompt, turn_started_at = extract_last_user_prompt_and_time(transcript_path)
        usage = extract_turn_usage(transcript_path)
        cwd = input_data.get("cwd")
        notify_harold(
            TurnComplete(
                cwd=cwd if isinstance(cwd, str) else os.getcwd(),
                last_user_prompt=last_user_prompt,
                assistant_message=assistant_message,
                session_id=string_or_empty(input_data.get("session_id")),
                turn_started_at=turn_started_at,
                transcript_path=string_or_empty(transcript_path),
                **usage,
            )
        )
    except Exception:
        return 0

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
