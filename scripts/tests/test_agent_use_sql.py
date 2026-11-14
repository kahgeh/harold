"""Check queries/agent-use.sql on fake rows.

Each test replaces the two tables with a SELECT of constants (SELECT only, no write) and sends the
text to the read-only ClickHouse through bin/logq. The tests skip when bin/logq or ClickHouse is not
there, as in CI. The text tests need no ClickHouse.
"""

import subprocess
import unittest

import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import EMPTY_GAUGE, EMPTY_MAP, GAUGE, QUERIES

LOGS = "otel.otel_logs"
EMPTY_LOGS = f"SELECT '' AS ServiceName, '' AS Body, {EMPTY_MAP} AS LogAttributes, now() AS Timestamp WHERE 0"


def cpu(role, value, days_ago=1):
    """Return one fake point of the gauge harold.agent.cpu.utilization."""
    return (f"SELECT 'harold.agent.cpu.utilization' AS MetricName, map('role', '{role}') AS Attributes, "
            f"now() - INTERVAL {int(days_ago * 86400)} SECOND AS TimeUnix, toFloat64({value}) AS Value")


def alert(top_agents, days_ago=1, service="harold", body="machine use past normal"):
    """Return one fake line `machine use past normal`."""
    return (f"SELECT '{service}' AS ServiceName, '{body}' AS Body, map('top_agents', '{top_agents}') AS LogAttributes, "
            f"now() - INTERVAL {int(days_ago * 86400)} SECOND AS Timestamp")


class AgentUseSqlTest(unittest.TestCase):
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)

    def run_query(self, gauge=(), logs=()):
        """Run the query on the fake rows. Return its one row as a dict of text."""
        sql = (QUERIES / "agent-use.sql").read_text(encoding="utf-8")
        for table, rows, empty in ((GAUGE, gauge, EMPTY_GAUGE), (LOGS, logs, EMPTY_LOGS)):
            sql = sql.replace(table, "(" + " UNION ALL ".join(rows or [empty]) + ")")
        done = subprocess.run([self.logq], input=sql.rstrip() + "\nFORMAT TSVWithNames", capture_output=True,
                              text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        lines = [line.split("\t") for line in done.stdout.splitlines()]
        self.assertEqual(len(lines), 2, done.stdout)
        return dict(zip(lines[0], lines[1]))

    def test_the_none_share_is_the_cpu_of_none_over_all_cpu(self):
        row = self.run_query([cpu("none", 0.3), cpu("chief", 0.1), cpu("build_worker", 0.6)])
        self.assertEqual(row["none_share"], "0.3")

    def test_a_point_older_than_7_days_does_not_count(self):
        row = self.run_query([cpu("none", 0.5), cpu("chief", 0.5, days_ago=8)])
        self.assertEqual(row["none_share"], "1")

    def test_no_point_gives_no_share(self):
        self.assertIn(self.run_query()["none_share"], ("\\N", "nan", "NULL"))

    def test_the_alert_count_and_the_alerts_with_no_agent_named(self):
        row = self.run_query(logs=[alert("chief 5% (cargo 5%)"), alert(""), alert(""), alert("x", 9),
                                   alert("x", service="other"), alert("x", body="another line")])
        self.assertEqual((row["alerts"], row["alerts_empty"]), ("3", "2"))

    def test_no_line_gives_zero_alerts(self):
        row = self.run_query()
        self.assertEqual((row["alerts"], row["alerts_empty"]), ("0", "0"))


class AgentUseFileTextTest(unittest.TestCase):
    """These tests read the file as text. They need no ClickHouse."""

    def test_the_file_is_a_select_of_the_two_tables_with_a_note_header(self):
        text = (QUERIES / "agent-use.sql").read_text(encoding="utf-8")
        for word in ("-- Note:", "-- Shows:", "-- Columns:", GAUGE, "otel.otel_logs", "harold.agent.cpu.utilization",
                     "machine use past normal", "none_share", "alerts_empty"):
            self.assertIn(word, text)
        for word in ("INSERT", "ALTER", "DROP", "CREATE", "DELETE"):
            self.assertNotIn(word, text.upper().replace("--", ""))

    def test_the_readme_lists_the_file(self):
        readme = (QUERIES / "README.md").read_text(encoding="utf-8")
        self.assertIn("`agent-use.sql`", readme)


if __name__ == "__main__":
    unittest.main()
