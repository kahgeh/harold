"""Check queries/reply-owner.sql: the wrong owned replies and the `pane label unknown` lines.

The text tests need no ClickHouse. The fixture tests replace the log table with a SELECT of
constants (SELECT only, no write) and send the text to the read-only ClickHouse through
bin/logq. They skip when bin/logq or ClickHouse is not there, as in CI.
"""

import subprocess
import unittest

from test_flow_spec_review_sql import QUERIES, find_logq

QUERY = (QUERIES / "reply-owner.sql").read_text(encoding="utf-8")
LOG_TABLE = "otel.otel_logs"


def decided(machine, owned, owner_from, latest, version, hours=1):
    """Return one fake `reply owner decided` line, logged `hours` ago by Harold `machine`."""
    return (f"SELECT now() - INTERVAL {hours} HOUR AS Timestamp, 'reply owner decided' AS Body, "
            f"map('harold.machine', '{machine}') AS ResourceAttributes, "
            f"map('owned', '{owned}', 'owner_from', '{owner_from}', 'latest_notifier', '{latest}', "
            f"'harold.version', '{version}') AS LogAttributes")


def unknown_pane(block, hours=1):
    """Return one fake `pane label unknown` line."""
    return (f"SELECT now() - INTERVAL {hours} HOUR AS Timestamp, 'pane label unknown' AS Body, "
            f"map('harold.machine', 'studio-mac') AS ResourceAttributes, "
            f"map('pane_id', '{block}') AS LogAttributes")


EMPTY = ("SELECT now() AS Timestamp, '' AS Body, CAST(map(), 'Map(String, String)') AS ResourceAttributes, "
         "CAST(map(), 'Map(String, String)') AS LogAttributes WHERE 0")


class ReplyOwnerText(unittest.TestCase):
    def test_the_query_reads_and_never_writes(self):
        body = "\n".join(line for line in QUERY.splitlines() if not line.startswith("--")).upper()
        for word in ("INSERT", "ALTER", "DROP", "CREATE", "TRUNCATE", "DELETE"):
            self.assertNotRegex(body, rf"\b{word}\b", word)

    def test_the_header_names_the_columns_the_window_and_the_version_rule(self):
        header = "\n".join(line for line in QUERY.splitlines() if line.startswith("--")).lower()
        for part in ("-- columns:", "7 days", "harold.version", "expected to be 0"):
            self.assertIn(part, header, part)


class ReplyOwnerFixture(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.logq = find_logq()
        if cls.logq is None:
            raise unittest.SkipTest("bin/logq is not here (set LOGQ): the fixture tests are skipped")
        probe = subprocess.run([cls.logq], input="SELECT 1", capture_output=True, text=True, timeout=30)
        if probe.returncode != 0 or probe.stdout.strip() != "1":
            raise unittest.SkipTest("ClickHouse cannot be reached through bin/logq: the fixture tests are skipped")

    def run_query(self, lines):
        sql = QUERY.replace(LOG_TABLE, "(" + " UNION ALL ".join(lines or [EMPTY]) + ")")
        done = subprocess.run([self.logq], input=sql.rstrip().rstrip(";") + "\nFORMAT TSVWithNames",
                              capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        rows = [line.split("\t") for line in done.stdout.splitlines()]
        return [dict(zip(rows[0], row)) for row in rows[1:]] if rows else []

    def test_a_wrong_owned_reply_is_listed(self):
        rows = self.run_query([
            decided("laptop-mac", "true", "latest_marker", "studio-mac", "9.9.9"),
            decided("laptop-mac", "true", "none", "studio-mac", "9.9.9"),
        ])
        wrong = [row for row in rows if row["kind"] == "wrong owned reply"]
        self.assertEqual(len(wrong), 2, rows)
        self.assertEqual({row["latest_notifier"] for row in wrong}, {"studio-mac"})

    def test_right_owned_replies_and_dropped_replies_are_not_listed(self):
        rows = self.run_query([
            decided("laptop-mac", "true", "latest_marker", "Laptop-Mac", "9.9.9"),
            decided("laptop-mac", "true", "none", "", "9.9.9"),
            decided("laptop-mac", "true", "tag", "studio-mac", "9.9.9"),
            decided("laptop-mac", "false", "latest_marker", "studio-mac", "9.9.9"),
        ])
        self.assertEqual([row for row in rows if row["kind"] == "wrong owned reply"], [])

    def test_lines_of_an_older_version_and_older_than_7_days_are_left_out(self):
        rows = self.run_query([
            decided("laptop-mac", "true", "latest_marker", "studio-mac", "0.0.1"),
            decided("laptop-mac", "true", "latest_marker", "studio-mac", "9.9.9", hours=24 * 8),
        ])
        self.assertEqual([row for row in rows if row["kind"] == "wrong owned reply"], [])

    def test_pane_label_unknown_lines_are_counted_in_7_days(self):
        rows = self.run_query([unknown_pane("block:a"), unknown_pane("block:b"), unknown_pane("block:c", hours=24 * 8)])
        counted = [row for row in rows if row["kind"] == "pane label unknown"]
        self.assertEqual([row["lines"] for row in counted], ["2"])

    def test_no_lines_give_a_count_of_0_and_no_wrong_reply(self):
        rows = self.run_query([])
        self.assertEqual([(row["kind"], row["lines"]) for row in rows], [("pane label unknown", "0")])


if __name__ == "__main__":
    unittest.main()
