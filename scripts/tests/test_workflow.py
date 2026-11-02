"""Plain-text checks on .github/workflows/build.yml (no yaml module)."""
import unittest
from pathlib import Path

WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "build.yml"


def workflow_text():
    return WORKFLOW.read_text(encoding="utf-8")


class ReleaseGateTest(unittest.TestCase):
    def test_linux_job_has_no_pending_digest(self):
        """Release gate: fails until the user approves the archlinux:base digest."""
        self.assertNotIn("DIGEST_PENDING_APPROVAL", workflow_text())


class LinuxJobTest(unittest.TestCase):
    def test_linux_steps_are_present(self):
        text = workflow_text()
        for needle in (
            "linux-defaults",
            "ldd target/x86_64-unknown-linux-gnu/release/harold",
            "libssl|libcrypto",
            "harold-x86_64-unknown-linux-gnu.tar.gz",
            "sha256sum",
            "sha256sum -c",
            "--build-features",
            "archlinux:base@sha256:",
            "gh release upload",
            "--clobber",
        ):
            self.assertIn(needle, text, needle)

    def test_mac_job_is_unchanged_in_name(self):
        self.assertIn("harold-aarch64-apple-darwin", workflow_text())

    def test_linux_packaging_runs_only_on_release(self):
        linux = workflow_text().split("\n  linux:\n", 1)[1]
        tail = linux.split("name: Build release binaries", 1)[1]
        steps = tail.split("      - name:")
        self.assertGreaterEqual(len(steps), 5)
        for step in steps:
            self.assertIn("if: github.event_name == 'release'", step)


if __name__ == "__main__":
    unittest.main()
