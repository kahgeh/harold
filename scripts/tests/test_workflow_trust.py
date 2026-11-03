"""Text checks of the GitHub workflows: what runs where, and where a cache may be used.

The test reads the workflow files as text, with the standard library only.
"""

from pathlib import Path
import re
import unittest

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"
CACHE_PIN = "actions/cache@55cc8345863c7cc4c66a329aec7e433d2d1c52a9"
PUSH_ONLY = ("macos-checks.yml", "linux-tests.yml")
CACHE_PATHS = ("~/.cargo/registry", "~/.cargo/git", "target")


def read(name):
    return (WORKFLOWS / name).read_text(encoding="utf-8")


def trigger_block(text):
    match = re.search(r"^on:\n((?:[ \t]+.*\n|\n)*)", text, re.M)
    assert match, "no on: block"
    return match.group(1)


def cache_steps(text):
    """Return (job, key) for each use of actions/cache."""
    found = []
    job = None
    lines = text.splitlines()
    for index, line in enumerate(lines):
        match = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
        if match:
            job = match.group(1)
        if "actions/cache" in line:
            key = None
            for follow in lines[index + 1:index + 12]:
                if follow.startswith("      - "):
                    break
                m = re.match(r"^\s+key: (.*)$", follow)
                if m:
                    key = m.group(1)
            found.append((job, key))
    return found


class WorkflowTrustTest(unittest.TestCase):
    def test_build_only_builds(self):
        text = read("build.yml")
        self.assertNotIn("cargo test", text)
        self.assertNotIn("check_features.sh", text)
        self.assertNotIn("actions/cache", text)

    def test_push_workflows_trigger_only_on_push_to_main(self):
        for name in PUSH_ONLY:
            block = trigger_block(read(name))
            self.assertEqual(
                [line for line in block.splitlines() if line.strip()],
                ["  push:", "    branches: [main]"],
                name,
            )

    def test_push_workflows_have_no_if_and_no_paths(self):
        for name in PUSH_ONLY:
            text = read(name)
            self.assertIsNone(re.search(r"^\s*if:", text, re.M), name)
            self.assertIsNone(re.search(r"^\s*paths(-ignore)?:", text, re.M), name)

    def test_push_workflows_pin_the_cache(self):
        for name in PUSH_ONLY:
            text = read(name)
            self.assertIn(CACHE_PIN + " # v6.1.0", text, name)
            self.assertEqual(len(re.findall(r"actions/cache@", text)), len(cache_steps(text)), name)
            self.assertNotIn("save-always", text, name)
            for path in CACHE_PATHS:
                self.assertIn(path, text, name)

    def test_cache_key_holds_the_job_name(self):
        for name in PUSH_ONLY:
            steps = cache_steps(read(name))
            self.assertTrue(steps, name)
            for job, key in steps:
                self.assertIsNotNone(key, name)
                self.assertTrue(
                    "${{ github.job }}" in key or f"-{job}-" in key, f"{name} {job} {key}"
                )
                self.assertIn("${{ runner.os }}-1.97.1-", key)
                self.assertIn("hashFiles('Cargo.lock')", key)

    def test_no_untrusted_trigger_uses_a_cache(self):
        for path in sorted(WORKFLOWS.glob("*.yml")):
            text = path.read_text(encoding="utf-8")
            block = trigger_block(text)
            if re.search(r"pull_request|workflow_run", block):
                self.assertNotIn("actions/cache", text, path.name)

    def test_macos_checks_shape(self):
        text = read("macos-checks.yml")
        self.assertIn("permissions:\n  contents: read", text)
        self.assertIn("persist-credentials: false", text)
        self.assertEqual(text.count("runs-on: macos-15"), 2)
        self.assertIn("macos-checks-${{ github.sha }}", text)


if __name__ == "__main__":
    unittest.main()
