import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "check_log_parity.sh"

# The fake logq reads the SQL on stdin. It returns 1 row for the message "was resent", else 0 rows.
FAKE_LOGQ = """#!/usr/bin/env bash
sql=$(cat)
case "$sql" in
*"was resent"*) echo 1 ;;
*) echo 0 ;;
esac
"""

LOG = "\n".join([
    # allowed: a health line
    "2026-11-07T09:30:00.000000Z  WARN harold::telemetry_health: telemetry export failing (collector unreachable at http://h:4317)",
    # allowed: a telemetry crate line, with ANSI codes
    "\x1b[2m2026-11-07T09:31:00.000000Z\x1b[0m \x1b[33m WARN\x1b[0m opentelemetry_sdk: BatchLogProcessor.ExportError secret=alpha",
    # allowed: no timestamp at the start
    "thread 'main' panicked at src/main.rs:1:1: secret=beta",
    # unsent, resent
    '2026-11-07T09:32:00.500000Z  INFO harold::tools::watch: was resent pane_id="%7" secret=gamma unsent=true',
    # unsent, not resent
    "2026-11-07T09:33:00.000000Z  INFO harold::tools::watch: was lost secret=delta unsent=true",
    # gap
    "2026-11-07T09:34:00.000000Z  INFO harold::tools::watch: gap message secret=epsilon",
    # gap too old: outside the window
    "2026-11-06T01:00:00.000000Z  INFO harold::tools::watch: old gap secret=zeta",
    "",
])


def run(log_text, extra_env=None):
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        log = tmp / "harold.log"
        log.write_text(log_text)
        logq = tmp / "logq"
        logq.write_text(FAKE_LOGQ)
        logq.chmod(0o755)
        env = dict(os.environ, HAROLD_LOG=str(log), LOGQ=str(logq),
                   NOW="2026-11-07T10:00:00Z")
        env.update(extra_env or {})
        return subprocess.run(["bash", str(SCRIPT), "2"], capture_output=True,
                              text=True, env=env)


class CheckLogParityTest(unittest.TestCase):
    def test_counts_and_lines(self):
        result = run(LOG)
        out = result.stdout
        self.assertIn("allowed: 3", out)
        self.assertIn("unsent resent: 1", out)
        self.assertIn("unsent not resent: 1", out)
        self.assertIn("gap: 1", out)
        self.assertIn("not resent: 2026-11-07T09:33:00.000000Z was lost", out)
        self.assertIn("gap: 2026-11-07T09:34:00.000000Z gap message", out)
        self.assertNotIn("old gap", out)

    def test_exit_one_on_gap(self):
        self.assertEqual(run(LOG).returncode, 1)

    def test_exit_zero_without_gap(self):
        no_gap = "\n".join(l for l in LOG.split("\n") if "gap message" not in l)
        result = run(no_gap)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("gap: 0", result.stdout)

    def test_no_field_value_printed(self):
        result = run(LOG)
        text = result.stdout + result.stderr
        for value in ["alpha", "beta", "gamma", "delta", "epsilon", "%7", "secret="]:
            self.assertNotIn(value, text)

    def test_quote_in_message_does_not_break_the_query(self):
        line = "2026-11-07T09:33:00.000000Z  INFO harold::x: it's a \\ test unsent=true\n"
        result = run(line)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("unsent not resent: 1", result.stdout)


if __name__ == "__main__":
    unittest.main()
