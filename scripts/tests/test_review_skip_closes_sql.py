"""Check evidence/review-skip-closes-1.sql on fake rows.

Each test replaces the table otel.otel_logs with a SELECT of constants (SELECT only, no write) and sends the
text to the read-only ClickHouse through bin/logq. The tests skip when bin/logq or ClickHouse is not there, as
in CI. The text test at the end needs no ClickHouse.
"""

import subprocess
import unittest

import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import QUERIES

QUERY = "evidence/review-skip-closes-1.sql"
EMPTY_MAP = base.EMPTY_MAP
EMPTY_LOGS = (f"SELECT '' AS ServiceName, '' AS Body, {EMPTY_MAP} AS LogAttributes, now() AS Timestamp WHERE 0")


def line(hours_ago, body="poll finished", file=None, service="harold"):
    """Return one fake log line, `hours_ago` hours old."""
    attributes = f"map('file', '{file}')" if file else EMPTY_MAP
    return (f"SELECT '{service}' AS ServiceName, '{body}' AS Body, {attributes} AS LogAttributes, "
            f"now() - INTERVAL {int(hours_ago * 3600)} SECOND AS Timestamp")


def unreadable(hours_ago, file):
    return line(hours_ago, "pod file unreadable", file)


class ReviewSkipClosesSqlTest(unittest.TestCase):
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)

    def run_since(self, logs=()):
        """Run the query with `since` 2 days ago. Return its one row."""
        sql = (QUERIES / QUERY).read_text(encoding="utf-8")
        sql = sql.replace("{since:DateTime}", "(now() - INTERVAL 172800 SECOND)")
        sql = sql.replace("otel.otel_logs", "(" + " UNION ALL ".join(logs or [EMPTY_LOGS]) + ")")
        done = subprocess.run([self.logq], input=sql.rstrip() + "\nFORMAT TSVWithNames", capture_output=True,
                              text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        lines = [row.split("\t") for row in done.stdout.splitlines()]
        self.assertEqual(len(lines), 2, done.stdout)
        return dict(zip(lines[0], lines[1]))

    def test_harold_lines_and_no_unreadable_line_is_files_read(self):
        row = self.run_since([line(10)])
        self.assertEqual((row["present"], row["reason"], row["unreadable"]), ("1", "files_read", "0"))

    def test_an_unreadable_line_for_one_of_the_three_files_is_still_unreadable(self):
        row = self.run_since([line(10), unreadable(10, "reviews/model-stall.md")])
        self.assertEqual((row["present"], row["reason"], row["unreadable"]), ("0", "still_unreadable", "1"))

    def test_an_unreadable_line_for_another_file_does_not_count(self):
        row = self.run_since([line(10), unreadable(10, "reviews/propose-hints.md")])
        self.assertEqual((row["present"], row["reason"], row["unreadable"]), ("1", "files_read", "0"))

    def test_an_unreadable_line_before_since_does_not_count(self):
        row = self.run_since([line(10), unreadable(100, "reviews/agent-jobs.md")])
        self.assertEqual(row["reason"], "files_read")

    def test_no_harold_line_is_no_harold_lines(self):
        row = self.run_since([line(10, service="other")])
        self.assertEqual((row["present"], row["reason"]), ("0", "no_harold_lines"))


class ReviewSkipClosesSqlTextTest(unittest.TestCase):
    """This test reads the file as text. It needs no ClickHouse."""

    def test_the_query_names_its_files_and_reasons(self):
        text = (QUERIES / QUERY).read_text(encoding="utf-8")
        for word in ("pod file unreadable", "reviews/agent-jobs.md", "reviews/model-stall.md",
                     "reviews/harder-held-out.md", "files_read", "still_unreadable", "no_harold_lines",
                     "{since:DateTime}", "INTERVAL 7 DAY", "LIMIT 1"):
            self.assertIn(word, text)
        self.assertNotIn("INSERT", text.upper())


if __name__ == "__main__":
    unittest.main()
