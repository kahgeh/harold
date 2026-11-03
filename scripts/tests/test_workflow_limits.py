"""Pin the per-step timeout-minutes limits of the GitHub workflows.

The test reads the workflow files as text, with the standard library only.
"""

from pathlib import Path
import re
import unittest

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"


def _job_lines(text, job):
    lines = text.splitlines()
    start = None
    for index, line in enumerate(lines):
        if line == f"  {job}:":
            start = index + 1
            break
    if start is None:
        raise LookupError(f"no job named {job}")
    end = len(lines)
    for index in range(start, len(lines)):
        if re.match(r"^  [A-Za-z0-9_-]+:\s*$", lines[index]):
            end = index
            break
    return lines[start:end]


def step_limit(text, job, step):
    """Return the timeout-minutes of a step, or None when it has none.

    Raise LookupError when the job or the step does not exist.
    """
    lines = _job_lines(text, job)
    start = None
    for index, line in enumerate(lines):
        if line == f"      - name: {step}":
            start = index + 1
            break
    if start is None:
        raise LookupError(f"no step named {step} in job {job}")
    for line in lines[start:]:
        if line.startswith("      - "):
            break
        match = re.match(r"^        timeout-minutes: (\d+)\s*$", line)
        if match:
            return int(match.group(1))
    return None


def job_limit(text, job):
    for line in _job_lines(text, job):
        match = re.match(r"^    timeout-minutes: (\d+)\s*$", line)
        if match:
            return int(match.group(1))
    return None


def read(name):
    return (WORKFLOWS / name).read_text(encoding="utf-8")


class WorkflowLimitsTest(unittest.TestCase):
    def test_build_job_steps(self):
        text = read("build.yml")
        self.assertEqual(step_limit(text, "build", "Build release binaries"), 20)
        with self.assertRaises(LookupError):
            step_limit(text, "build", "Test")
        with self.assertRaises(LookupError):
            step_limit(text, "build", "Check the feature mixes")

    def test_macos_checks_steps(self):
        text = read("macos-checks.yml")
        self.assertEqual(step_limit(text, "test", "Test"), 40)
        self.assertEqual(step_limit(text, "features", "Check the feature mixes"), 30)

    def test_build_linux_job_has_no_test_step(self):
        with self.assertRaises(LookupError):
            step_limit(read("build.yml"), "linux", "Test")

    def test_linux_tests_workflow_test_step(self):
        self.assertEqual(step_limit(read("linux-tests.yml"), "linux", "Test"), 25)

    def test_job_limits(self):
        build = read("build.yml")
        self.assertEqual(job_limit(build, "build"), 30)
        self.assertEqual(job_limit(build, "linux"), 30)
        self.assertEqual(job_limit(read("linux-tests.yml"), "linux"), 60)
        checks = read("macos-checks.yml")
        self.assertEqual(job_limit(checks, "test"), 45)
        self.assertEqual(job_limit(checks, "features"), 45)

    def test_step_limit_reads_the_right_step(self):
        text = (
            "jobs:\n"
            "  a:\n"
            "    timeout-minutes: 60\n"
            "    steps:\n"
            "      - name: Test\n"
            "        run: x\n"
            "      - name: Other\n"
            "        timeout-minutes: 7\n"
            "  b:\n"
            "    steps:\n"
            "      - name: Test\n"
            "        timeout-minutes: 9\n"
        )
        self.assertIsNone(step_limit(text, "a", "Test"))
        self.assertEqual(step_limit(text, "a", "Other"), 7)
        self.assertEqual(step_limit(text, "b", "Test"), 9)
        with self.assertRaises(LookupError):
            step_limit(text, "a", "Missing")
        with self.assertRaises(LookupError):
            step_limit(text, "c", "Test")


if __name__ == "__main__":
    unittest.main()
