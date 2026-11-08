"""Pin the release time that the build workflow gives to the build.

The test reads build.yml as text, with the standard library only.
"""

from pathlib import Path
import re
import unittest

BUILD = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "build.yml"
TIME_LINE = (
    'export HAROLD_RELEASE_TIME="$(git for-each-ref '
    "--format='%(creatordate:iso-strict)' \"refs/tags/$HAROLD_RELEASE_VERSION\")\""
)


def step_lines(text, job, step):
    lines = text.splitlines()
    start = lines.index(f"  {job}:") + 1
    end = len(lines)
    for index in range(start, len(lines)):
        if re.match(r"^  [A-Za-z0-9_-]+:\s*$", lines[index]):
            end = index
            break
    job_lines = lines[start:end]
    begin = job_lines.index(f"      - name: {step}") + 1
    out = []
    for line in job_lines[begin:]:
        if line.startswith("      - "):
            break
        out.append(line.strip())
    return out


class ReleaseTimeTest(unittest.TestCase):
    def check(self, job):
        text = BUILD.read_text(encoding="utf-8")
        step = step_lines(text, job, "Build release binaries")
        self.assertIn("HAROLD_RELEASE_VERSION: ${{ github.event.release.tag_name }}", step)
        joined = "\n".join(step)
        self.assertIn(TIME_LINE, joined)
        # The time is read from the tag's own time, before the build starts.
        self.assertLess(joined.index("HAROLD_RELEASE_TIME"), joined.index("cargo build"))

    def test_build_job_passes_the_tag_time(self):
        self.check("build")

    def test_linux_job_passes_the_tag_time(self):
        self.check("linux")


if __name__ == "__main__":
    unittest.main()
