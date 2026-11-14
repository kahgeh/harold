import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from . import claude_session_start as hook

ROLE_ENV = {"POD_ROLE": "pnem", "POD_PRODUCT": "harold"}
PAYLOAD = {
    "session_id": "s-1",
    "transcript_path": "/t/s-1.jsonl",
    "cwd": "/pod",
    "source": "startup",
}


class SessionStartHookTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="Harold's session start test ")
        self.addCleanup(self.temporary.cleanup)
        self.folder = Path(self.temporary.name)

    def test_sends_every_field(self):
        request = hook.build_request(ROLE_ENV, PAYLOAD, 4242, "%7")
        self.assertEqual(
            request,
            {
                "pod_role": "pnem",
                "pod_product": "harold",
                "pane_id": "%7",
                "session_id": "s-1",
                "transcript_path": "/t/s-1.jsonl",
                "cwd": "/pod",
                "agent_pid": 4242,
                "start_file": "",
                "source": "startup",
                "pod_spec": "",
            },
        )

    def test_a_build_worker_sends_its_spec_id_from_pod_spec(self):
        env = {"POD_ROLE": "build_worker", "POD_PRODUCT": "harold", "POD_SPEC": "agent-restart"}
        request = hook.build_request(env, PAYLOAD, 4242, "%7")
        self.assertEqual(request["pod_spec"], "agent-restart")

    def test_without_pod_role_sends_nothing(self):
        self.assertIsNone(hook.build_request({}, PAYLOAD, 4242, "%7"))
        self.assertIsNone(hook.build_request({"POD_ROLE": ""}, PAYLOAD, 4242, "%7"))

    def test_start_file_is_sent_when_it_exists_in_the_pod_folder(self):
        start = self.folder / ".superpowers/pod/start/pnem.json"
        start.parent.mkdir(parents=True)
        start.write_text("{}", encoding="utf-8")
        request = hook.build_request(ROLE_ENV, {**PAYLOAD, "cwd": str(self.folder)}, 1, "%7")
        self.assertEqual(request["start_file"], str(start))

    def test_pane_from_tmux_then_rex_then_empty_and_no_command_runs(self):
        with patch.dict(os.environ, {"TMUX_PANE": "%3", "REX_BLOCK": "block:9"}, clear=True):
            with patch.object(subprocess, "run") as run:
                self.assertEqual(hook.pane_id(), "%3")
            run.assert_not_called()
        with patch.dict(os.environ, {"REX_BLOCK": "block:9"}, clear=True):
            self.assertEqual(hook.pane_id(), "block:9")
        with patch.dict(os.environ, {}, clear=True):
            self.assertEqual(hook.pane_id(), "")

    def test_the_agent_is_the_first_claude_process_up_the_parent_chain(self):
        table = {
            50: (40, "/bin/sh"),
            40: (30, "/usr/local/bin/claude"),
            30: (1, "zsh"),
        }
        self.assertEqual(hook.find_agent_pid(50, table.get), 40)
        self.assertEqual(hook.find_agent_pid(40, table.get), 40)

    def test_the_walk_stops_after_5_levels_and_falls_back_to_the_parent(self):
        table = {pid: (pid - 1, "sh") for pid in range(100, 90, -1)}
        table[95] = (94, "claude")
        self.assertEqual(hook.find_agent_pid(100, table.get), 100)
        table[96] = (95, "claude")
        self.assertEqual(hook.find_agent_pid(100, table.get), 96)

    def test_no_claude_process_or_a_missing_row_gives_the_parent(self):
        self.assertEqual(hook.find_agent_pid(7, {7: (1, "sh")}.get), 7)
        self.assertEqual(hook.find_agent_pid(7, lambda pid: None), 7)

    def test_main_sends_the_pid_the_walk_found(self):
        with patch.object(hook, "find_agent_pid", return_value=4242):
            code, run = self.run_main(ROLE_ENV, PAYLOAD)
        command = run.call_args.args[0]
        self.assertEqual(json.loads(command[command.index("-d") + 1])["agent_pid"], 4242)

    def run_main(self, env, payload, grpc_addr="127.0.0.1:1"):
        (self.folder / "service.json").write_text(json.dumps({"grpc_addr": grpc_addr}), encoding="utf-8")
        with patch.dict(os.environ, env, clear=True), \
                patch.object(sys, "stdin", io.StringIO(json.dumps(payload))), \
                patch.object(hook, "HAROLD_BUNDLE", self.folder), \
                patch.object(hook, "pane_id", return_value="%7"), \
                patch.object(subprocess, "run") as run:
            code = hook.main()
        return code, run

    def test_main_calls_the_rpc_with_the_request(self):
        code, run = self.run_main(ROLE_ENV, PAYLOAD)
        self.assertEqual(code, 0)
        command = run.call_args.args[0]
        self.assertEqual(command[0], "grpcurl")
        self.assertEqual(command[-1], "harold.Harold/PodAgentStarted")
        sent = json.loads(command[command.index("-d") + 1])
        self.assertEqual((sent["pod_role"], sent["pane_id"], sent["session_id"]), ("pnem", "%7", "s-1"))

    def test_main_without_pod_role_runs_nothing(self):
        code, run = self.run_main({}, PAYLOAD)
        self.assertEqual(code, 0)
        run.assert_not_called()

    def test_a_harold_that_does_not_answer_ends_the_hook_within_3_seconds(self):
        code, run = self.run_main(ROLE_ENV, PAYLOAD)
        command = run.call_args.args[0]
        self.assertLessEqual(float(command[command.index("-max-time") + 1]), 2.5)
        self.assertLessEqual(run.call_args.kwargs["timeout"], 3)
        with patch.object(subprocess, "run", side_effect=subprocess.TimeoutExpired("grpcurl", 3)), \
                patch.dict(os.environ, ROLE_ENV, clear=True), \
                patch.object(sys, "stdin", io.StringIO(json.dumps(PAYLOAD))), \
                patch.object(hook, "HAROLD_BUNDLE", self.folder), \
                patch.object(hook, "pane_id", return_value="%7"):
            self.assertEqual(hook.main(), 0)

    def test_no_service_file_exits_0_without_a_call(self):
        with patch.dict(os.environ, ROLE_ENV, clear=True), \
                patch.object(sys, "stdin", io.StringIO(json.dumps(PAYLOAD))), \
                patch.object(hook, "HAROLD_BUNDLE", self.folder), \
                patch.object(hook, "pane_id", return_value="%7"), \
                patch.object(subprocess, "run") as run:
            self.assertEqual(hook.main(), 0)
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
