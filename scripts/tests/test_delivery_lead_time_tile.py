"""Check the query of the card Delivery lead time on fake rows.

The query file docs/references/telemetry/queries/delivery-lead-time-tile.sql reads the table
otel.otel_metrics_histogram. The test replaces that table name with a SELECT of constants
(SELECT only, no table, no write) and sends the text to the read-only ClickHouse through bin/logq.
The tests skip when bin/logq or ClickHouse is not there, as in CI.

A fake point has the columns that the query reads: MetricName, Attributes, TimeUnix and Sum.
"""

from pathlib import Path
import os
import re
import shutil
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
QUERY = ROOT / "docs" / "references" / "telemetry" / "queries" / "delivery-lead-time-tile.sql"
TABLE = "otel.otel_metrics_histogram"
DAY = 86400
PHASES = ["open_to_approved", "approved_to_started", "started_to_completed", "merged_to_released"]


def find_logq():
    """Return the path of bin/logq, or None. LOGQ names it. The default is the chief's tool."""
    named = os.environ.get("LOGQ")
    if named:
        return named if Path(named).is_file() else None
    found = shutil.which("logq")
    if found:
        return found
    default = Path.home() / "Dev" / "p" / "chief-of-pods" / "bin" / "logq"
    return str(default) if default.is_file() else None


def point(spec, phase, days, hours_ago, metric="pod.spec.lead_time"):
    """Return one fake point: a phase of a spec that took the given days, recorded hours_ago hours ago."""
    return (f"SELECT '{metric}' AS MetricName, map('spec.id', '{spec}', 'phase', '{phase}') AS Attributes, "
            f"now() - INTERVAL {hours_ago} HOUR AS TimeUnix, toFloat64({days * DAY}) AS Sum")


def spec_points(spec, phase_days, released_hours_ago):
    """Return the four points of a spec. The release is recorded released_hours_ago hours ago."""
    rows = []
    for index, phase in enumerate(PHASES):
        if phase in phase_days:
            rows.append(point(spec, phase, phase_days[phase], released_hours_ago + (len(PHASES) - index) * 24
                              if phase != "merged_to_released" else released_hours_ago))
    return rows


def four(first, second, third, release):
    return dict(zip(PHASES, [first, second, third, release]))


class DeliveryLeadTimeTileTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.logq = find_logq()
        if cls.logq is None:
            raise unittest.SkipTest("bin/logq is not here (set LOGQ): the fixture tests need the read-only ClickHouse")
        probe = subprocess.run([cls.logq], input="SELECT 1", capture_output=True, text=True, timeout=30)
        if probe.returncode != 0 or probe.stdout.strip() != "1":
            raise unittest.SkipTest("ClickHouse cannot be reached through bin/logq: the fixture tests are skipped")
        cls.text = QUERY.read_text(encoding="utf-8")

    def run_on(self, *points):
        """Run the query on the fake points. Return the lines of the answer, in TSV."""
        source = " UNION ALL ".join(points) if points else \
            "SELECT '' AS MetricName, CAST(map(), 'Map(String, String)') AS Attributes, now() AS TimeUnix, toFloat64(0) AS Sum WHERE 0"
        self.assertIn(TABLE, self.text)
        sql = self.text.replace(TABLE, f"({source})").rstrip() + "\nFORMAT TSV"
        done = subprocess.run([self.logq], input=sql, capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        return [line.split("\t") for line in done.stdout.splitlines()]

    def test_two_released_specs_give_their_median_sum_in_days(self):
        # Sums: 3 days and 5 days. The median is 4 days. Nothing before: no change figure.
        rows = self.run_on(*spec_points("a", four(1, 1, 0.5, 0.5), 30), *spec_points("b", four(2, 1, 1, 1), 60))
        self.assertEqual(rows, [["4", "\\N"]])

    def test_the_change_compares_with_the_7_days_before(self):
        # This week 3 and 5 days (median 4). The week before: one spec of 5 days. Change: -20 percent.
        rows = self.run_on(*spec_points("a", four(1, 1, 0.5, 0.5), 30), *spec_points("b", four(2, 1, 1, 1), 60),
                           *spec_points("c", four(2, 1, 1, 1), 24 * 10))
        self.assertEqual(rows, [["4", "-20"]])

    def test_no_merged_to_released_point_gives_no_row(self):
        partial = {phase: 1 for phase in PHASES[:3]}
        self.assertEqual(self.run_on(*spec_points("a", partial, 30)), [])
        self.assertEqual(self.run_on(), [])

    def test_a_release_outside_the_7_days_gives_no_row(self):
        self.assertEqual(self.run_on(*spec_points("a", four(1, 1, 1, 1), 24 * 10)), [])

    def test_a_spec_with_a_missing_phase_does_not_count(self):
        missing = {phase: 1 for phase in PHASES if phase != "approved_to_started"}
        rows = self.run_on(*spec_points("a", four(1, 1, 0.5, 0.5), 30), *spec_points("b", four(2, 1, 1, 1), 60),
                           *spec_points("x", missing, 30))
        self.assertEqual(rows, [["4", "\\N"]])

    def test_the_first_point_of_a_phase_counts(self):
        # A second, later point of one phase of the spec must not change its sum.
        extra = point("a", "open_to_approved", 50, 20)
        rows = self.run_on(*spec_points("a", four(1, 1, 1, 1), 30), extra)
        self.assertEqual(rows, [["4", "\\N"]])

    def test_a_phase_recorded_before_the_7_days_still_counts(self):
        # The first phases of a spec are often recorded weeks before its release.
        rows = self.run_on(*spec_points("a", four(1, 1, 1, 1), 30))
        self.assertEqual(rows, [["4", "\\N"]])

    def test_other_metrics_are_ignored(self):
        other = [point("z", phase, 9, 30, metric="pod.other") for phase in PHASES]
        self.assertEqual(self.run_on(*other), [])


class DeliveryLeadTimeTileTextTest(unittest.TestCase):
    """These tests read the file as text. They need no ClickHouse."""

    def setUp(self):
        self.text = QUERY.read_text(encoding="utf-8")

    def test_the_header_does_not_say_the_card_waits(self):
        header = self.text.split("SELECT", 1)[0] if not self.text.lstrip().startswith("WITH") else \
            self.text.split("WITH", 1)[0]
        self.assertNotRegex(header, r"(?i)\buntil\b.*\brecords\b|\bstub\b|idea to the user's check")
        self.assertRegex(self.text, r"(?m)^-- Waits for: nothing$")

    def test_the_columns_and_the_guards(self):
        code = re.sub(r"(?m)--.*$", "", self.text)
        self.assertRegex(code, r"(?i)\bAS\s+days\b")
        self.assertRegex(code, r"(?i)\bAS\s+days_change_pct\b")
        self.assertRegex(code, r"(?i)\bLIMIT\s+1\b")
        self.assertIn("pod.spec.lead_time", code)

    def test_no_provider_name(self):
        self.assertIsNone(re.search(r"(?i)claude|anthropic|codex|openai|gemini", self.text))


if __name__ == "__main__":
    unittest.main()
