"""Text checks of the GitHub workflows: what runs where, and where a cache may be used.

The test reads the workflow files as text, with the standard library only.
"""

from pathlib import Path
import re
import unittest

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"
PIN_SHA = "55cc8345863c7cc4c66a329aec7e433d2d1c52a9"
RESTORE_PIN = "actions/cache/restore@" + PIN_SHA + " # v6.1.0"
SAVE_PIN = "actions/cache/save@" + PIN_SHA + " # v6.1.0"
PUSH_ONLY = ("macos-checks.yml", "linux-tests.yml")
CACHE_PATHS = ("~/.cargo/registry", "~/.cargo/git", "target")


def read(name):
    return (WORKFLOWS / name).read_text(encoding="utf-8")


def trigger_block(text):
    match = re.search(r"^on:\n((?:[ \t]+.*\n|\n)*)", text, re.M)
    assert match, "no on: block"
    return match.group(1)


def cache_steps(text):
    """Return (job, kind, key, has_always, line) for each use of actions/cache/restore or save."""
    found = []
    job = None
    lines = text.splitlines()
    step_start = 0
    for index, line in enumerate(lines):
        match = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
        if match:
            job = match.group(1)
        if line.startswith("      - "):
            step_start = index
        kind = re.search(r"uses: actions/cache/(restore|save)@", line)
        if not kind:
            continue
        step_end = index + 1
        while step_end < len(lines) and not lines[step_end].startswith("      - ") and not re.match(r"^  [A-Za-z0-9_-]+:\s*$", lines[step_end]):
            step_end += 1
        key = None
        has_always = False
        for follow in lines[step_start:step_end]:
            m = re.match(r"^\s+key: (.*)$", follow)
            if m:
                key = m.group(1)
            if re.match(r"^\s+if: always\(\)\s*$", follow):
                has_always = True
        found.append((job, kind.group(1), key, has_always, index))
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
            ifs = re.findall(r"^\s*if:.*$", text, re.M)
            self.assertEqual([i.strip() for i in ifs], ["if: always()"] * len(ifs), name)
            self.assertIsNone(re.search(r"^\s*paths(-ignore)?:", text, re.M), name)

    def test_push_workflows_pin_the_cache(self):
        for name in PUSH_ONLY:
            text = read(name)
            self.assertIn(RESTORE_PIN, text, name)
            self.assertIn(SAVE_PIN, text, name)
            self.assertNotIn("actions/cache@", text, name)
            self.assertNotIn("save-always", text, name)
            uses = re.findall(r"uses: (actions/cache/[a-z]+@\S+ # \S+)", text)
            self.assertEqual(sorted(set(uses)), sorted([RESTORE_PIN, SAVE_PIN]), name)
            for path in CACHE_PATHS:
                self.assertIn(path, text, name)

    def test_each_job_restores_then_saves_with_always(self):
        for name in PUSH_ONLY:
            text = read(name)
            steps = cache_steps(text)
            jobs = sorted({job for job, *_ in steps})
            self.assertTrue(jobs, name)
            for job in jobs:
                mine = [s for s in steps if s[0] == job]
                self.assertEqual([s[1] for s in mine], ["restore", "save"], f"{name} {job}")
                self.assertTrue(mine[1][3], f"{name} {job}: the save needs if: always()")
                self.assertFalse(mine[0][3], f"{name} {job}: the restore must not use if: always()")
                self.assertEqual(mine[0][2], mine[1][2], f"{name} {job}: the keys differ")

    def test_save_comes_after_the_work_steps(self):
        for name in PUSH_ONLY:
            text = read(name)
            for job, kind, _key, _always, line in cache_steps(text):
                if kind != "save":
                    continue
                job_text = "\n".join(text.splitlines()[line:])
                job_text = re.split(r"(?m)^  [A-Za-z0-9_-]+:\s*$", job_text)[0]
                self.assertNotIn("run:", job_text, f"{name} {job}: a work step follows the save")

    def test_cache_key_holds_the_job_name(self):
        for name in PUSH_ONLY:
            steps = cache_steps(read(name))
            self.assertTrue(steps, name)
            for job, _kind, key, _always, _line in steps:
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
