import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "check_features.sh"

FAKE_CARGO = """#!/usr/bin/env bash
case "$FAKE_MODE:$1" in
test_fail:test)
    for i in $(seq 1 300); do echo "test a::b::case_$i ... ok"; done
    echo
    echo "failures:"
    echo
    echo "---- a::b::flaky stdout ----"
    echo "thread 'a::b::flaky' panicked at src/a.rs:10:5:"
    echo "assertion failed: left == right"
    echo
    echo "failures:"
    echo "    a::b::flaky"
    echo
    echo "test a::b::flaky ... FAILED"
    echo "test result: FAILED. 299 passed; 1 failed; 0 ignored"
    exit 101
    ;;
check_fail:check)
    echo "error[E0425]: cannot find value \\`x\\` in this scope"
    echo " --> src/main.rs:3:5"
    echo "error: could not compile \\`harold\\`"
    exit 101
    ;;
silent_fail:check)
    echo "Checking harold"
    exit 1
    ;;
esac
echo "fine"
exit 0
"""


class CheckFeaturesTest(unittest.TestCase):
    def run_script(self, mode):
        with tempfile.TemporaryDirectory() as tmp:
            cargo = Path(tmp) / "cargo"
            cargo.write_text(FAKE_CARGO)
            cargo.chmod(0o755)
            env = dict(os.environ, FAKE_MODE=mode, PATH=f"{tmp}:{os.environ['PATH']}")
            result = subprocess.run(
                ["bash", str(SCRIPT)],
                cwd=REPO,
                env=env,
                capture_output=True,
                text=True,
            )
        # stdout and stderr share one terminal when the script runs for real. Here they are two
        # pipes, so the tail check reads stderr, where the failure and the summary go.
        return result

    def test_failed_test_is_named_in_the_last_lines(self):
        result = self.run_script("test_fail")
        self.assertEqual(result.returncode, 1)
        tail = result.stderr.splitlines()[-15:]
        self.assertIn("test a::b::flaky ... FAILED", tail)
        self.assertIn("thread 'a::b::flaky' panicked at src/a.rs:10:5:", tail)
        self.assertIn("assertion failed: left == right", tail)
        self.assertIn("test result: FAILED. 299 passed; 1 failed; 0 ignored", tail)

    def test_summary_is_after_the_failed_line(self):
        lines = self.run_script("test_fail").stderr.splitlines()
        failed = [i for i, line in enumerate(lines) if line.startswith("check_features: failed: ")]
        self.assertEqual(len(failed), 1)
        self.assertEqual(lines[failed[0] + 1], "check_features: summary:")

    def test_compiler_error_is_named_in_the_last_lines(self):
        result = self.run_script("check_fail")
        self.assertEqual(result.returncode, 1)
        tail = result.stderr.splitlines()[-10:]
        self.assertIn("error[E0425]: cannot find value `x` in this scope", tail)
        self.assertIn(" --> src/main.rs:3:5", tail)

    def test_failure_without_a_matching_line(self):
        result = self.run_script("silent_fail")
        self.assertEqual(result.returncode, 1)
        self.assertTrue(
            result.stderr.splitlines()[-1].endswith("no test or error line found"),
            result.stderr,
        )

    def test_pass_ends_with_ok_and_has_no_summary(self):
        result = self.run_script("pass")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.splitlines()[-1], "check_features: ok")
        self.assertNotIn("summary", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
