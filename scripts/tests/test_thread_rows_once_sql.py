"""Check evidence/thread-rows-once-1.sql on fake rows.

Each test replaces the two tables otel.otel_traces and otel.otel_logs with a SELECT of constants
(SELECT only, no write) and sends the text to the read-only ClickHouse through bin/logq. The tests skip
when bin/logq or ClickHouse is not there, as in CI. The text test at the end needs no ClickHouse.
"""

import subprocess
import unittest

import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import QUERIES

QUERY = "evidence/thread-rows-once-1.sql"
EMPTY_MAP = base.EMPTY_MAP
EMPTY_TRACES = "SELECT '' AS SpanName, now() AS Timestamp WHERE 0"
EMPTY_LOGS = f"SELECT '' AS Body, {EMPTY_MAP} AS LogAttributes, now() AS Timestamp WHERE 0"


def span(days_ago, name="thread.duplicate_row"):
    """Return one fake browser span, `days_ago` days old (a fraction is allowed)."""
    return f"SELECT '{name}' AS SpanName, now() - INTERVAL {int(days_ago * 86400)} SECOND AS Timestamp"


def connected(days_ago, streams):
    """Return one fake `app page connected` line with `streams_for_page`."""
    return (f"SELECT 'app page connected' AS Body, map('streams_for_page', '{streams}', 'page_id', 'p') AS LogAttributes, "
            f"now() - INTERVAL {int(days_ago * 86400)} SECOND AS Timestamp")


class ThreadRowsOnceSqlTest(unittest.TestCase):
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)

    def run_since(self, days_since, traces=(), logs=()):
        """Run the query with `since` set `days_since` days ago. Return its one row."""
        sql = (QUERIES / QUERY).read_text(encoding="utf-8")
        sql = sql.replace("{since:DateTime}", f"(now() - INTERVAL {int(days_since * 86400)} SECOND)")
        for table, rows, empty in (("otel.otel_traces", traces, EMPTY_TRACES), ("otel.otel_logs", logs, EMPTY_LOGS)):
            sql = sql.replace(table, "(" + " UNION ALL ".join(rows or [empty]) + ")")
        done = subprocess.run([self.logq], input=sql.rstrip() + "\nFORMAT TSVWithNames", capture_output=True,
                              text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        lines = [line.split("\t") for line in done.stdout.splitlines()]
        self.assertEqual(len(lines), 2, done.stdout)
        return dict(zip(lines[0], lines[1]))

    def test_a_removed_copy_after_since_is_evidence(self):
        row = self.run_since(1, traces=[span(0.5)])
        self.assertEqual((row["present"], row["reason"]), ("1", "duplicate_removed"))

    def test_a_removed_copy_before_since_is_not_evidence(self):
        row = self.run_since(1, traces=[span(2)])
        self.assertEqual((row["present"], row["reason"]), ("0", "too_early"))

    def test_another_span_is_not_evidence(self):
        row = self.run_since(1, traces=[span(0.5, "browser.message_shown")])
        self.assertEqual(row["reason"], "too_early")

    def test_8_days_with_pages_and_one_stream_each_is_evidence(self):
        row = self.run_since(8, logs=[connected(3, 1), connected(2, 1)])
        self.assertEqual((row["present"], row["reason"]), ("1", "no_second_stream"))

    def test_8_days_with_a_second_stream_is_not_evidence(self):
        row = self.run_since(8, logs=[connected(3, 1), connected(2, 2)])
        self.assertEqual((row["present"], row["reason"]), ("0", "too_early"))

    def test_8_days_with_no_page_at_all_is_not_evidence(self):
        self.assertEqual(self.run_since(8)["reason"], "too_early")

    def test_3_days_with_one_stream_each_is_too_early(self):
        row = self.run_since(3, logs=[connected(2, 1)])
        self.assertEqual((row["present"], row["reason"]), ("0", "too_early"))


class ThreadRowsOnceSqlTextTest(unittest.TestCase):
    """This test reads the file as text. It needs no ClickHouse."""

    def test_the_query_names_its_span_log_field_and_reasons(self):
        text = (QUERIES / QUERY).read_text(encoding="utf-8")
        for word in ("thread.duplicate_row", "app page connected", "streams_for_page", "duplicate_removed",
                     "no_second_stream", "too_early", "{since:DateTime}"):
            self.assertIn(word, text)
        self.assertNotIn("INSERT", text.upper())


if __name__ == "__main__":
    unittest.main()
