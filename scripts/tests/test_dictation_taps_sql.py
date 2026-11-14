"""Check queries/dictation-taps.sql of the spec dictation-every-attempt.

The text tests need no ClickHouse, no port and no server. The fixture test replaces the table
`otel.otel_logs` with a SELECT of constants (SELECT only, no write) and sends the text to the
read-only ClickHouse through bin/logq. It skips when bin/logq or ClickHouse is not there, as in CI.
"""

import re
import subprocess
import unittest

from test_flow_spec_review_sql import QUERIES, find_logq

TAPS = (QUERIES / "dictation-taps.sql").read_text()
LOGS = "otel.otel_logs"


def header(text):
    return "\n".join(line for line in text.splitlines() if line.startswith("--"))


def body(text):
    return "\n".join(line for line in text.splitlines() if not line.startswith("--"))


def line(result, tap, hours_ago=1, body_text="app dictation check"):
    """Return one fake log line of the dictation check."""
    return (f"SELECT '{body_text}' AS Body, now() - INTERVAL {hours_ago} HOUR AS Timestamp, "
            f"map('result', '{result}', 'tap', '{tap}', 'started', 'true') AS LogAttributes")


class DictationTapsText(unittest.TestCase):
    def test_the_columns_are_named_in_the_header_and_the_select(self):
        self.assertIn("-- Columns: series, taps, ok_taps, ok_share_pct.", TAPS)
        for name in ("series", "taps", "ok_taps", "ok_share_pct"):
            self.assertRegex(body(TAPS), rf"\bAS {name}\b", name)

    def test_the_window_is_7_days_and_the_rows_are_limited(self):
        self.assertIn("INTERVAL 7 DAY", body(TAPS))
        self.assertRegex(body(TAPS), r"LIMIT \d+")

    def test_the_failed_rows_leave_out_ok_no_speech_and_page_ended(self):
        self.assertIn("result NOT IN ('ok', 'no_speech', 'page_ended')", body(TAPS))

    def test_old_pages_and_missing_are_left_out_of_the_share(self):
        self.assertIn("!= 'missing'", body(TAPS))
        self.assertIn("tap = 1", body(TAPS))
        self.assertIn("tap >= 2", body(TAPS))

    def test_the_header_says_what_confirms_the_fix(self):
        text = header(TAPS)
        for part in ("start_failed", "aborted", "page_ended", "No alert"):
            self.assertIn(part, text, part)

    def test_the_query_reads_and_never_writes(self):
        self.assertNotRegex(body(TAPS).upper(), r"\b(INSERT|ALTER|DROP|CREATE|TRUNCATE|DELETE)\b")


class DictationTapsFixture(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.logq = find_logq()
        if cls.logq is None:
            raise unittest.SkipTest("bin/logq is not here (set LOGQ): the fixture test needs the read-only ClickHouse")
        probe = subprocess.run([cls.logq], input="SELECT 1", capture_output=True, text=True, timeout=30)
        if probe.returncode != 0 or probe.stdout.strip() != "1":
            raise unittest.SkipTest("ClickHouse cannot be reached through bin/logq: the fixture test is skipped")

    def run_query(self, lines):
        sql = TAPS.replace(LOGS, "(" + " UNION ALL ".join(lines) + ")")
        done = subprocess.run([self.logq], input=sql.rstrip() + "\nFORMAT TSVWithNames", capture_output=True,
                              text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        rows = [row.split("\t") for row in done.stdout.splitlines()]
        return {r[0]: dict(zip(rows[0], r)) for r in rows[1:]}

    def test_it_gives_the_share_by_tap_and_lists_a_failed_tap_2(self):
        rows = self.run_query([
            line("ok", 1), line("ok", 1), line("network", 1), line("ok", 1),
            line("ok", 2), line("aborted", 2), line("page_ended", 2), line("no_speech", 3),
            line("start_failed", 3),
            # Left out: an old page (tap 0), a missing page, an old line and another line.
            line("aborted", 0), line("missing", 0), line("aborted", 2, hours_ago=24 * 8),
            line("aborted", 2, body_text="app request"),
        ])
        self.assertEqual((rows["tap 1"]["taps"], rows["tap 1"]["ok_taps"], rows["tap 1"]["ok_share_pct"]),
                         ("4", "3", "75"))
        self.assertEqual((rows["tap 2 or more"]["taps"], rows["tap 2 or more"]["ok_taps"],
                          rows["tap 2 or more"]["ok_share_pct"]), ("5", "1", "20"))
        self.assertEqual(rows["failed, tap 2 or more: aborted"]["taps"], "1")
        self.assertEqual(rows["failed, tap 2 or more: start_failed"]["taps"], "1")
        self.assertEqual(sorted(rows), ["failed, tap 2 or more: aborted", "failed, tap 2 or more: start_failed",
                                        "tap 1", "tap 2 or more"])

    def test_with_no_line_both_shares_are_null(self):
        rows = self.run_query([line("ok", 1, hours_ago=24 * 9)])
        self.assertEqual(rows["tap 1"]["taps"], "0")
        self.assertEqual(rows["tap 1"]["ok_share_pct"], "\\N")
        self.assertEqual(sorted(rows), ["tap 1", "tap 2 or more"])


if __name__ == "__main__":
    unittest.main()
