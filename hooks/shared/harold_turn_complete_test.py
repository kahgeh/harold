import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from threading import Barrier
import unittest
from unittest.mock import patch

from . import harold_turn_complete


class HaroldTurnCompleteTest(unittest.TestCase):
    def test_clean_text_removes_controls_collapses_whitespace_and_bounds_output(self) -> None:
        self.assertEqual(
            harold_turn_complete.clean_text("  refresh\x1b[31m\n\tevents  ", 14),
            "refresh events",
        )
        self.assertEqual(
            harold_turn_complete.clean_text("  Résumé\u2003task  ", 160),
            "Résumé task",
        )
        self.assertEqual(
            harold_turn_complete.clean_text(
                "before \x1b]0;private title\x07 after", 160
            ),
            "before after",
        )
        self.assertEqual(
            harold_turn_complete.clean_text("before \u009b31mred after", 160),
            "before red after",
        )
        self.assertEqual(
            harold_turn_complete.clean_text("before\u009dprivate\u009c after", 160),
            "before after",
        )
        self.assertEqual(harold_turn_complete.clean_text("🦀🦀🦀", 2), "🦀🦀")

    @patch.object(harold_turn_complete.subprocess, "run")
    def test_call_harold_preserves_the_wire_compatible_payload(self, run) -> None:
        harold_turn_complete.call_harold(
            pane_id="%8",
            pane_label="harold:2.3",
            last_user_prompt="refresh events",
            assistant_message="events refreshed",
            main_context="harold",
            grpc_addr="127.0.0.1:55060",
        )

        command = run.call_args.args[0]
        payload = json.loads(command[command.index("-d") + 1])
        self.assertEqual(
            payload,
            {
                "pane_id": "%8",
                "pane_label": "harold:2.3",
                "last_user_prompt": "refresh events",
                "assistant_message": "events refreshed",
                "main_context": "harold",
            },
        )
        self.assertNotIn("work_summary", payload)


class ManagedHookTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="Harold's test ")
        self.addCleanup(self.temporary.cleanup)
        self.prefix = Path(self.temporary.name).resolve()
        self.bundle = self.prefix / "harold"
        hook_path = self.bundle / "hooks" / "harold_turn_complete.py"
        hook_path.parent.mkdir(parents=True)
        shutil.copyfile(harold_turn_complete.__file__, hook_path)
        (self.bundle / "service.json").write_text(
            json.dumps({"grpc_addr": "127.0.0.1:55060"}), encoding="utf-8"
        )
        spec = importlib.util.spec_from_file_location("installed_harold_hook", hook_path)
        self.hook = importlib.util.module_from_spec(spec)
        with patch.dict(sys.modules, {spec.name: self.hook}):
            spec.loader.exec_module(self.hook)
        self.turn = self.hook.TurnComplete("/project", "refresh", "finished")
        self.addCleanup(patch.stopall)
        patch.object(self.hook, "get_pane_info", return_value=("%8", "work:0.0")).start()
        patch.object(self.hook, "get_main_context", return_value="project").start()
        patch.object(socket, "create_connection", return_value=unittest.mock.MagicMock()).start()
        patch.object(subprocess, "Popen", side_effect=AssertionError("unmanaged launch")).start()

    def test_installed_hook_uses_managed_endpoint_and_paths_with_spaces(self) -> None:
        with patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
            with patch.dict(os.environ, {"HAROLD_ADDR": "127.0.0.1:59999"}):
                self.hook.notify_harold(self.turn)

        start, delivery = run.call_args_list
        self.assertEqual(start.args[0], [str(self.prefix / "haroldctl"), "start"])
        self.assertTrue(start.kwargs["check"])
        self.assertLessEqual(start.kwargs["timeout"], 8)
        command = delivery.args[0]
        self.assertEqual(command[command.index("-import-path") + 1], str(self.bundle))
        self.assertEqual(command[-2], "127.0.0.1:55060")
        self.assertEqual(command[-1], "harold.Harold/TurnComplete")

    def test_failed_managed_start_never_delivers_to_an_unrelated_listener(self) -> None:
        error = subprocess.CalledProcessError(1, ["haroldctl", "start"])
        with patch.object(subprocess, "run", side_effect=error) as run:
            with self.assertRaises(subprocess.CalledProcessError):
                self.hook.notify_harold(self.turn)
        self.assertEqual(run.call_args.args[0], [str(self.prefix / "haroldctl"), "start"])
        self.assertEqual(run.call_count, 1)

    def test_start_timeout_prevents_delivery(self) -> None:
        error = subprocess.TimeoutExpired(["haroldctl", "start"], 8)
        with patch.object(subprocess, "run", side_effect=error) as run:
            with self.assertRaises(subprocess.TimeoutExpired):
                self.hook.notify_harold(self.turn)
        self.assertEqual(run.call_args.args[0], [str(self.prefix / "haroldctl"), "start"])
        self.assertEqual(run.call_count, 1)

    def test_missing_or_invalid_managed_endpoint_prevents_delivery(self) -> None:
        for metadata in [{}, {"grpc_addr": ""}, {"grpc_addr": 42}]:
            with self.subTest(metadata=metadata):
                (self.bundle / "service.json").write_text(json.dumps(metadata), encoding="utf-8")
                commands = []
                def run(command, **kwargs):
                    commands.append(command)
                    return subprocess.CompletedProcess(command, 0)
                with patch.object(subprocess, "run", side_effect=run):
                    with self.assertRaises((ValueError, KeyError)):
                        self.hook.notify_harold(self.turn)
                self.assertFalse(any(command[0] == "grpcurl" for command in commands))

    def test_simultaneous_hooks_wait_for_managed_readiness_before_delivery(self) -> None:
        starting = Barrier(2)
        commands = []
        def run(command, **kwargs):
            commands.append(command)
            if command[0] == str(self.prefix / "haroldctl"):
                starting.wait(timeout=2)
            else:
                self.assertEqual(sum(command[-1] == "start" for command in commands), 2)
            return subprocess.CompletedProcess(command, 0)

        with patch.object(subprocess, "run", side_effect=run):
            with ThreadPoolExecutor(max_workers=2) as executor:
                futures = [executor.submit(self.hook.notify_harold, self.turn) for _ in range(2)]
                for future in futures:
                    future.result(timeout=3)
        self.assertEqual(sum(command[-1] == "start" for command in commands), 2)
        self.assertEqual(sum(command[0] == "grpcurl" for command in commands), 2)


if __name__ == "__main__":
    unittest.main()
