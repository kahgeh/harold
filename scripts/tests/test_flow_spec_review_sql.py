"""Check the spec rows of factory-flow.sql and the phase filter of delivery-lead-time.sql on fake rows.

Each test replaces the two metric tables with a SELECT of constants (SELECT only, no write) and sends
the text to the read-only ClickHouse through bin/logq. The tests skip when bin/logq or ClickHouse is
not there, as in CI. The text tests at the end need no ClickHouse.
"""

from pathlib import Path
import os
import shutil
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[2]
QUERIES = ROOT / "docs" / "references" / "telemetry" / "queries"
HISTOGRAM = "otel.otel_metrics_histogram"
GAUGE = "otel.otel_metrics_gauge"
EMPTY_MAP = "CAST(map(), 'Map(String, String)')"
FOUR_PHASES = ["open_to_approved", "approved_to_started", "started_to_completed", "merged_to_released"]
NEW_PHASES = ["spec_writing", "architect_review", "approval_wait"]


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


def lead(spec, phase, seconds):
    """Return one fake point of pod.spec.lead_time: one lead time, recorded an hour ago."""
    return (f"SELECT 'pod.spec.lead_time' AS MetricName, {EMPTY_MAP} AS ResourceAttributes, "
            f"map('phase', '{phase}', 'spec.id', '{spec}', 'pod.product', 'p') AS Attributes, "
            f"now() - INTERVAL 1 HOUR AS TimeUnix, toUInt64(1) AS Count, toFloat64({seconds}) AS Sum")


def inventory(name, items):
    """Return one fake point of the gauge pod.inventory.items."""
    return (f"SELECT 'pod.inventory.items' AS MetricName, map('list', '{name}') AS Attributes, "
            f"now() - INTERVAL 1 HOUR AS TimeUnix, toFloat64({items}) AS Value")


EMPTY_HISTOGRAM = (f"SELECT '' AS MetricName, {EMPTY_MAP} AS ResourceAttributes, {EMPTY_MAP} AS Attributes, "
                   "now() AS TimeUnix, toUInt64(0) AS Count, toFloat64(0) AS Sum WHERE 0")
EMPTY_GAUGE = f"SELECT '' AS MetricName, {EMPTY_MAP} AS Attributes, now() AS TimeUnix, toFloat64(0) AS Value WHERE 0"


class FlowSqlTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.logq = find_logq()
        if cls.logq is None:
            raise unittest.SkipTest("bin/logq is not here (set LOGQ): the fixture tests need the read-only ClickHouse")
        probe = subprocess.run([cls.logq], input="SELECT 1", capture_output=True, text=True, timeout=30)
        if probe.returncode != 0 or probe.stdout.strip() != "1":
            raise unittest.SkipTest("ClickHouse cannot be reached through bin/logq: the fixture tests are skipped")

    def run_query(self, name, histogram=(), gauge=()):
        """Run the query file on the fake rows. Return the rows as dicts of text."""
        sql = (QUERIES / name).read_text(encoding="utf-8").replace("$__filters", "1")
        for table, rows, empty in ((HISTOGRAM, histogram, EMPTY_HISTOGRAM), (GAUGE, gauge, EMPTY_GAUGE)):
            sql = sql.replace(table, "(" + " UNION ALL ".join(rows or [empty]) + ")")
        done = subprocess.run([self.logq], input=sql.rstrip() + "\nFORMAT TSVWithNames", capture_output=True,
                              text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        lines = [line.split("\t") for line in done.stdout.splitlines()]
        return [dict(zip(lines[0], line)) for line in lines[1:]] if lines else []

    def test_delivery_lead_time_gives_only_the_four_phases_as_spec_rows(self):
        points = [lead("a", phase, 3600) for phase in FOUR_PHASES + NEW_PHASES + ["idea_to_spec"]]
        rows = self.run_query("delivery-lead-time.sql", histogram=points)
        self.assertEqual([row["series"] for row in rows], sorted(f"spec: {phase}" for phase in FOUR_PHASES))

    def flow_rows(self):
        specs = [
            # Three specs with a reviews file: writing, review and approval waits.
            ("a", 7200, (2400, 2400, 2400)),
            ("c", 2400, (1800, 0, 600)),
            ("d", 1800, (600, 900, 300)),
        ]
        points = []
        for spec, total, (writing, review, waiting) in specs:
            points += [lead(spec, "open_to_approved", total), lead(spec, "spec_writing", writing),
                       lead(spec, "architect_review", review), lead(spec, "approval_wait", waiting)]
        # Two specs from before the reviews file: only open_to_approved.
        points += [lead("b", "open_to_approved", 3600), lead("e", "open_to_approved", 1000)]
        points += [lead("a", "approved_to_started", 100), lead("a", "started_to_completed", 200)]
        gauge = [inventory("awaiting_approval", 5), inventory("in_spec_writing", 1),
                 inventory("in_architect_review", 2)]
        return {row["stage"]: row for row in self.run_query("factory-flow.sql", histogram=points, gauge=gauge)}

    def test_the_spec_and_architect_review_rows_have_items_and_medians(self):
        rows = self.flow_rows()
        self.assertEqual((rows["spec"]["items"], rows["spec"]["median_wait_s"], rows["spec"]["has_data"]),
                         ("1", "1800", "1"))
        self.assertEqual((rows["architect review"]["items"], rows["architect review"]["median_wait_s"],
                          rows["architect review"]["has_data"]), ("2", "900", "1"))

    def test_the_approval_row_takes_the_waiting_specs_less_the_two_lists(self):
        rows = self.flow_rows()
        self.assertEqual(rows["approval"]["items"], "2")
        self.assertEqual(rows["approval"]["has_data"], "1")

    def test_the_approval_median_takes_approval_wait_else_open_to_approved(self):
        # a 2400, b 3600 (old), c 600, d 300, e 1000 (old): the median is 1000.
        self.assertEqual(self.flow_rows()["approval"]["median_wait_s"], "1000")

    def test_the_build_row_is_unchanged(self):
        self.assertEqual(self.flow_rows()["build"]["median_wait_s"], "300")

    def test_with_no_review_lists_the_review_rows_have_no_items_and_no_median(self):
        rows = {row["stage"]: row for row in self.run_query(
            "factory-flow.sql", histogram=[lead("b", "open_to_approved", 3600)],
            gauge=[inventory("awaiting_approval", 1)])}
        self.assertEqual((rows["spec"]["items"], rows["spec"]["median_wait_s"]), ("0", "\\N"))
        self.assertEqual(rows["approval"]["items"], "1")
        self.assertEqual(rows["approval"]["median_wait_s"], "3600")


class FlowSqlTextTest(unittest.TestCase):
    """These tests read the files as text. They need no ClickHouse."""

    def test_delivery_lead_time_filters_the_phase_of_the_spec_rows(self):
        text = (QUERIES / "delivery-lead-time.sql").read_text(encoding="utf-8")
        self.assertIn("AND Attributes['phase'] IN ('open_to_approved', 'approved_to_started', "
                      "'started_to_completed', 'merged_to_released')", text)

    def test_factory_flow_reads_the_two_new_lists_and_phases(self):
        text = (QUERIES / "factory-flow.sql").read_text(encoding="utf-8")
        for word in ("in_spec_writing", "in_architect_review", "spec_writing", "architect_review", "approval_wait"):
            self.assertIn(f"'{word}'", text)


if __name__ == "__main__":
    unittest.main()
