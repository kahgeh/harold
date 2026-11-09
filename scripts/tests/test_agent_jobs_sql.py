"""Check evidence/agent-jobs-1.sql and evidence/agent-jobs-2.sql on fake rows.

Each test replaces the table otel.otel_metrics_sum with a SELECT of constants (SELECT only, no write)
and sends the text to the read-only ClickHouse through bin/logq. The tests skip when bin/logq or
ClickHouse is not there, as in CI. The text tests at the end need no ClickHouse.
"""

import subprocess
import unittest

import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import QUERIES

EMPTY_MAP = base.EMPTY_MAP
EMPTY_SUM = (f"SELECT '' AS MetricName, {EMPTY_MAP} AS ResourceAttributes, {EMPTY_MAP} AS Attributes, "
             "now() AS TimeUnix, toFloat64(0) AS Value WHERE 0")


def point(name, attrs, days_ago, value):
    """Return one fake point of a counter, `days_ago` days old."""
    pairs = ", ".join(f"'{key}', '{val}'" for key, val in attrs.items())
    return (f"SELECT '{name}' AS MetricName, {EMPTY_MAP} AS ResourceAttributes, map({pairs}) AS Attributes, "
            f"now() - INTERVAL {int(days_ago * 86400)} SECOND AS TimeUnix, toFloat64({value}) AS Value")


def used(days_ago, value):
    return point("harold.feature.used", {"spec.id": "agent-jobs"}, days_ago, value)


def ends(days_ago, value, end_line="typed", how="ended"):
    return point("harold.job.ends", {"how": how, "end_line": end_line}, days_ago, value)


class AgentJobsSqlTest(unittest.TestCase):
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)

    def run_query(self, name, rows):
        """Run the query with `since` 1 day ago. Return its rows as dicts."""
        sql = (QUERIES / "evidence" / name).read_text(encoding="utf-8")
        sql = sql.replace("{since:DateTime}", "(now() - INTERVAL 86400 SECOND)")
        sql = sql.replace("otel.otel_metrics_sum", "(" + " UNION ALL ".join(rows or [EMPTY_SUM]) + ")")
        done = subprocess.run([self.logq], input=sql.rstrip() + "\nFORMAT TSVWithNames", capture_output=True,
                              text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        lines = [line.split("\t") for line in done.stdout.splitlines()]
        self.assertEqual(len(lines), 2, done.stdout)
        return dict(zip(lines[0], lines[1]))

    def test_a_use_after_since_is_job_added(self):
        row = self.run_query("agent-jobs-1.sql", [used(3, 0), used(0.5, 1)])
        self.assertEqual((row["present"], row["reason"]), ("1", "job_added"))

    def test_a_series_that_stays_at_zero_is_no_job_yet(self):
        row = self.run_query("agent-jobs-1.sql", [used(3, 0), used(0.5, 0)])
        self.assertEqual((row["present"], row["reason"]), ("0", "no_job_yet"))

    def test_a_use_before_since_is_no_job_yet(self):
        row = self.run_query("agent-jobs-1.sql", [used(3, 1), used(2, 1), used(0.5, 1)])
        self.assertEqual((row["present"], row["reason"]), ("0", "no_job_yet"))

    def test_no_point_at_all_is_no_job_yet(self):
        self.assertEqual(self.run_query("agent-jobs-1.sql", [])["reason"], "no_job_yet")

    def test_an_end_with_a_typed_line_is_end_told(self):
        row = self.run_query("agent-jobs-2.sql", [ends(3, 0), ends(0.5, 1)])
        self.assertEqual((row["present"], row["reason"]), ("1", "end_told"))

    def test_an_end_whose_line_failed_is_end_not_told(self):
        row = self.run_query("agent-jobs-2.sql",
                             [ends(3, 0), ends(0.5, 1), ends(3, 0, "failed"), ends(0.5, 1, "failed")])
        self.assertEqual((row["present"], row["reason"]), ("0", "end_not_told"))

    def test_a_removed_job_is_not_an_end(self):
        row = self.run_query("agent-jobs-2.sql", [ends(3, 0, "none", "removed"), ends(0.5, 1, "none", "removed")])
        self.assertEqual((row["present"], row["reason"]), ("0", "no_end_yet"))

    def test_seeded_zero_points_are_no_end_yet(self):
        row = self.run_query("agent-jobs-2.sql", [ends(3, 0), ends(0.5, 0)])
        self.assertEqual((row["present"], row["reason"]), ("0", "no_end_yet"))


class AgentJobsSqlTextTest(unittest.TestCase):
    """These tests read the files as text. They need no ClickHouse."""

    def read(self, name):
        return (QUERIES / "evidence" / name).read_text(encoding="utf-8")

    def test_both_files_follow_the_form_of_the_other_evidence_files(self):
        for name in ("agent-jobs-1.sql", "agent-jobs-2.sql"):
            text = self.read(name)
            for word in ("{since:DateTime}", "INTERVAL 30 DAY", "LIMIT 1", "AS present", "AS reason", "SELECT"):
                self.assertIn(word, text, name)
            for word in ("INSERT", "ALTER", "DROP", "CREATE", "DELETE"):
                self.assertNotIn(word, text.upper().replace("--", ""), name)
            self.assertLess(len(text.encode("utf-8")), 64 * 1024)

    def test_the_files_name_their_signals_and_reasons(self):
        one = self.read("agent-jobs-1.sql")
        for word in ("feature.used", "'agent-jobs'", "job_added", "no_job_yet"):
            self.assertIn(word, one)
        two = self.read("agent-jobs-2.sql")
        for word in ("harold.job.ends", "'ended'", "'failed'", "end_told", "no_end_yet", "end_not_told"):
            self.assertIn(word, two)


if __name__ == "__main__":
    unittest.main()
