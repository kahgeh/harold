import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from . import claude_turn_complete, codex_turn_complete


class ProviderHooksTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="Harold's provider test ")
        self.addCleanup(self.temporary.cleanup)
        self.transcript = Path(self.temporary.name) / "transcript.jsonl"

    def write_records(self, records):
        self.transcript.write_text(
            "broken JSON\n" + "\n".join(json.dumps(record) for record in records),
            encoding="utf-8",
        )

    def invoke(self, module, payload):
        with patch.object(sys, "stdin", io.StringIO(json.dumps(payload))):
            with patch.object(module, "notify_harold") as notify:
                self.assertEqual(module.main(), 0)
        return notify

    def test_claude_records_skip_malformed_and_internal_messages(self):
        self.write_records([
            None, [], 7,
            {"type": "user", "message": None},
            {"type": "assistant", "message": []},
            {"type": "user", "message": {"content": "old prompt"}},
            {"type": "user", "message": {"content": [{"type": "text", "text": "new prompt"}]}},
            {"type": "user", "message": {"content": "<system reminder>"}},
            {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "id", "content": "tool output"}]}},
            {"type": "assistant", "message": {"content": [{"type": "text", "text": "first"}, {"type": "text", "text": "second"}]}},
        ])
        notify = self.invoke(claude_turn_complete, {"cwd": "/project", "transcript_path": str(self.transcript)})
        turn = notify.call_args.args[0]
        self.assertEqual((turn.cwd, turn.last_user_prompt, turn.assistant_message), ("/project", "new prompt", "first\nsecond"))

    def test_codex_event_message_records(self):
        self.write_records([
            None, [], 7,
            {"type": "event_msg", "payload": None},
            {"type": "response_item", "payload": []},
            {"type": "event_msg", "payload": {"type": "user_message", "message": "new prompt"}},
            {"type": "event_msg", "payload": {"type": "agent_message", "message": "finished"}},
            {"type": "event_msg", "payload": {"type": "agent_message", "message": {"unexpected": "object"}}},
        ])
        notify = self.invoke(codex_turn_complete, {"cwd": "/project", "transcript_path": str(self.transcript)})
        turn = notify.call_args.args[0]
        self.assertEqual((turn.cwd, turn.last_user_prompt, turn.assistant_message), ("/project", "new prompt", "finished"))

    def test_codex_response_item_records(self):
        self.write_records([
            {"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "new prompt"}]}},
            {"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "finished"}]}},
        ])
        notify = self.invoke(codex_turn_complete, {"transcript_path": str(self.transcript)})
        turn = notify.call_args.args[0]
        self.assertEqual((turn.last_user_prompt, turn.assistant_message), ("new prompt", "finished"))

    def test_direct_reply_takes_precedence_and_invalid_cwd_uses_process_directory(self):
        for module in (claude_turn_complete, codex_turn_complete):
            with self.subTest(module=module.__name__):
                with patch.object(module, "extract_last_assistant_message", side_effect=AssertionError("must use direct reply")):
                    notify = self.invoke(module, {"cwd": [], "last_assistant_message": " direct reply "})
                turn = notify.call_args.args[0]
                self.assertEqual(turn.cwd, os.getcwd())
                self.assertEqual(turn.assistant_message, "direct reply")

    def test_malformed_top_level_input_never_notifies(self):
        for module in (claude_turn_complete, codex_turn_complete):
            for raw in ("broken JSON", "[]", "null", "7", '"text"', "{}"):
                with self.subTest(module=module.__name__, raw=raw):
                    with patch.object(sys, "stdin", io.StringIO(raw)):
                        with patch.object(module, "notify_harold") as notify:
                            self.assertEqual(module.main(), 0)
                    notify.assert_not_called()

    def test_claude_subagent_stop_never_notifies(self):
        self.invoke(claude_turn_complete, {"hook_event_name": "SubagentStop", "last_assistant_message": "subagent done"}).assert_not_called()

    def test_missing_or_malformed_transcript_path_keeps_direct_reply(self):
        for module in (claude_turn_complete, codex_turn_complete):
            for path in (None, [], 17, str(self.transcript)):
                with self.subTest(module=module.__name__, path=path):
                    notify = self.invoke(module, {"transcript_path": path, "last_assistant_message": "finished"})
                    turn = notify.call_args.args[0]
                    self.assertEqual((turn.last_user_prompt, turn.assistant_message), ("", "finished"))

    def test_notification_failure_is_silent_and_does_not_block_provider(self):
        for module in (claude_turn_complete, codex_turn_complete):
            with self.subTest(module=module.__name__):
                with patch.object(sys, "stdin", io.StringIO('{"last_assistant_message": "private content"}')):
                    with patch.object(module, "notify_harold", side_effect=RuntimeError("private content")):
                        with patch.object(sys, "stderr", io.StringIO()) as stderr:
                            self.assertEqual(module.main(), 0)
                            self.assertEqual(stderr.getvalue(), "")

    def test_installed_scripts_find_shared_notifier_in_custom_prefix(self):
        hooks = Path(self.temporary.name) / "custom prefix" / "harold" / "hooks"
        hooks.mkdir(parents=True)
        shared = Path(__file__).resolve().parent.parent / "shared" / "harold_turn_complete.py"
        shutil.copyfile(shared, hooks / shared.name)
        for module in (claude_turn_complete, codex_turn_complete):
            source = Path(module.__file__)
            deployed = hooks / source.name
            shutil.copyfile(source, deployed)
            # Execute the actual module and check where its imported notifier lives.
            code = "import runpy, sys; runpy.run_path(sys.argv[1]); print(sys.modules['harold_turn_complete'].HAROLD_BUNDLE)"
            result = subprocess.run([sys.executable, "-I", "-c", code, str(deployed)], cwd="/", capture_output=True, text=True, check=True)
            self.assertEqual(result.stdout.strip(), str(hooks.parent.resolve()))
            self.assertEqual(result.stderr, "")


if __name__ == "__main__":
    unittest.main()
