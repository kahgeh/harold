"""Check the files of the spec decision-tool: queries/decisions.sql (the card Decisions) and
queries/evidence/decision-tool-1.sql.

The tests with fake rows replace the tables with a SELECT of constants (SELECT only, no write) and send the
text to the read-only ClickHouse through bin/logq. They skip when bin/logq or ClickHouse is not there, as in CI.
The text tests need no ClickHouse and no port.
"""

import json
import subprocess
import unittest

import test_big_picture as bp
import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import QUERIES

EMPTY_MAP = base.EMPTY_MAP
EMPTY_SUM = (f"SELECT '' AS MetricName, {EMPTY_MAP} AS ResourceAttributes, {EMPTY_MAP} AS Attributes, "
             "now() AS TimeUnix, toFloat64(0) AS Value WHERE 0")
EMPTY_GAUGE = EMPTY_SUM
EMPTY_LOGS = f"SELECT '' AS ServiceName, '' AS Body, {EMPTY_MAP} AS LogAttributes, now() AS Timestamp WHERE 0"

GREEN, RED, WHITE = "\U0001f7e2", "\U0001f534", "⚪"
UP, DOWN = "▲", "▼"


def point(name, attrs, days_ago, value):
    """Return one fake point of a metric, `days_ago` days old."""
    pairs = ", ".join(f"'{key}', '{val}'" for key, val in {"organisation": "org", **attrs}.items())
    return (f"SELECT '{name}' AS MetricName, {EMPTY_MAP} AS ResourceAttributes, map({pairs}) AS Attributes, "
            f"now() - INTERVAL {int(days_ago * 86400)} SECOND AS TimeUnix, toFloat64({value}) AS Value")


def added(days_ago, value):
    return point("harold.decisions.added", {"role": "chief", "kind": "model", "accountable": "user"}, days_ago, value)


def evaluated(process, outcome, days_ago, value):
    return point("harold.decisions.evaluated", {"role": "chief", "kind": "model", "accountable": "user",
                                                "process_grade": process, "outcome_grade": outcome}, days_ago, value)


def gauge(status, days_ago, value):
    return point("harold.decisions.open", {"status": status}, days_ago, value)


def log(body, hours_ago, service="harold"):
    return (f"SELECT '{service}' AS ServiceName, '{body}' AS Body, {EMPTY_MAP} AS LogAttributes, "
            f"now() - INTERVAL {int(hours_ago * 3600)} SECOND AS Timestamp")


class LogqTest(unittest.TestCase):
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)

    def rows(self, sql):
        done = subprocess.run([self.logq], input=sql.rstrip() + "\nFORMAT TSVWithNames", capture_output=True,
                              text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        lines = [line.split("\t") for line in done.stdout.splitlines()]
        return [dict(zip(lines[0], line)) for line in lines[1:]] if lines else []


class DecisionsQueryTest(LogqTest):
    def card(self, sums=(), gauges=()):
        sql = (QUERIES / "decisions.sql").read_text(encoding="utf-8")
        sql = sql.replace("otel.otel_metrics_sum", "(" + " UNION ALL ".join(sums or [EMPTY_SUM]) + ")")
        sql = sql.replace("otel.otel_metrics_gauge", "(" + " UNION ALL ".join(gauges or [EMPTY_GAUGE]) + ")")
        return self.rows(sql)

    def week_of_counters(self):
        """12 made this week and 10 the week before; 5 evaluated (4 good process, 3 good outcome) this week
        and 4 the week before (3 good process, 2 good outcome)."""
        return [
            added(20, 0), added(10, 10), added(3, 22),
            evaluated("good", "good", 20, 0), evaluated("good", "good", 10, 2), evaluated("good", "good", 3, 5),
            evaluated("good", "poor", 20, 0), evaluated("good", "poor", 10, 1), evaluated("good", "poor", 3, 2),
            evaluated("poor", "poor", 20, 0), evaluated("poor", "poor", 10, 1), evaluated("poor", "poor", 3, 2),
        ]

    def test_each_figure_and_its_change(self):
        rows = self.card(self.week_of_counters(), [gauge("due", 10, 2), gauge("due", 1, 3), gauge("open", 1, 9)])
        self.assertEqual(len(rows), 1)
        row = rows[0]
        self.assertEqual((row["made"], row["made_change_pct"]), ("12", "20"))
        self.assertEqual((row["evaluated"], row["evaluated_change_pct"]), ("5", "25"))
        self.assertEqual((row["due"], row["due_change_pct"]), ("3", "50"))

    def test_the_shares_and_their_change_in_points(self):
        rows = self.card(self.week_of_counters(), [gauge("due", 1, 3)])
        row = rows[0]
        # This week 5 evaluated: process good 4 (80%), outcome good 3 (60%).
        # The week before 4 evaluated: process good 3 (75%), outcome good 2 (50%).
        self.assertEqual((row["good_process_pct"], row["good_outcome_pct"]), ("80", "60"))
        self.assertEqual(float(row["good_process_change_pts"]), 5.0)
        self.assertEqual(float(row["good_outcome_change_pts"]), 10.0)

    def test_a_restart_counts_the_whole_value_of_the_first_point_after_it(self):
        rows = self.card([added(20, 0), added(6, 10), added(2, 3)])
        self.assertEqual(rows[0]["made"], "13", "10, then a drop to 3 that rises by 3")

    def test_no_data_in_the_week_before_gives_an_empty_change(self):
        rows = self.card([added(20, 0), added(3, 4)])
        row = rows[0]
        self.assertEqual(row["made"], "4")
        self.assertEqual(row["made_change_pct"], "\\N")
        self.assertEqual(row["good_process_pct"], "\\N")
        self.assertEqual(row["due"], "0")

    def test_seeded_counters_show_0_made_and_no_counter_shows_no_row(self):
        zero = self.card([added(2, 0), added(1, 0)])
        self.assertEqual((zero[0]["made"], zero[0]["evaluated"]), ("0", "0"))
        self.assertEqual(self.card([]), [])

    def test_a_week_with_no_evaluated_record_has_no_share(self):
        rows = self.card([added(3, 2), evaluated("good", "good", 3, 0)])
        self.assertEqual((rows[0]["good_process_pct"], rows[0]["good_process_change_pts"]), ("\\N", "\\N"))


class DecisionsCardLineTest(LogqTest):
    """The wrap of the card on fake rows: three lines, the dots, the arrows and the points."""

    def lines(self, **columns):
        sql = bp.wrap("decisions", bp.fake_source(**columns))
        done = subprocess.run([self.logq], input=sql, capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        return done.stdout.rstrip("\n").split("\n")

    FIGURES = dict(made=12, made_change_pct=20, evaluated=5, evaluated_change_pct=67, good_process_pct=80,
                   good_process_change_pts=5.0, good_outcome_pct=60, good_outcome_change_pts=-10.0,
                   due=3, due_change_pct=50)

    def test_the_card_of_the_spec(self):
        self.assertEqual(self.lines(**self.FIGURES), [
            f"12 made {GREEN} {UP} 20% · 5 evaluated {GREEN} {UP} 67%",
            f"good process 80% {GREEN} {UP} 5.0 pts · good outcome 60% {RED} {DOWN} 10.0 pts",
            f"3 due {RED} {UP} 50% on last week",
        ])

    def test_a_fall_in_due_is_green_and_a_fall_in_made_is_red(self):
        figures = dict(self.FIGURES, made_change_pct=-20, due_change_pct=-50)
        lines = self.lines(**figures)
        self.assertEqual(lines[0].split(" · ")[0], f"12 made {RED} {DOWN} 20%")
        self.assertEqual(lines[2], f"3 due {GREEN} {DOWN} 50% on last week")

    def test_no_change_and_no_figure(self):
        figures = dict(self.FIGURES, made_change_pct=0, good_process_change_pts=0.0, due_change_pct=None,
                       good_outcome_change_pts=None, evaluated_change_pct=None)
        lines = self.lines(**figures)
        self.assertEqual(lines[0], f"12 made {WHITE} 0% · 5 evaluated no change figure")
        self.assertEqual(lines[1], f"good process 80% {WHITE} 0.0 pts · good outcome 60% no change figure")
        self.assertEqual(lines[2], "3 due no change figure")

    def test_a_share_with_no_evaluated_record_says_no_data_yet(self):
        figures = dict(self.FIGURES, evaluated=0, good_process_pct=None, good_outcome_pct=None,
                       good_process_change_pts=None, good_outcome_change_pts=None)
        self.assertEqual(self.lines(**figures)[1],
                         "good process no data yet no change figure · good outcome no data yet no change figure")

    def test_a_null_made_gives_no_row(self):
        self.assertEqual(self.lines(**dict(self.FIGURES, made=None)), [""])


class DecisionEvidenceQueryTest(LogqTest):
    def run_since(self, logs=()):
        sql = (QUERIES / "evidence" / "decision-tool-1.sql").read_text(encoding="utf-8")
        sql = sql.replace("{since:DateTime}", "(now() - INTERVAL 172800 SECOND)")
        sql = sql.replace("otel.otel_logs", "(" + " UNION ALL ".join(logs or [EMPTY_LOGS]) + ")")
        rows = self.rows(sql)
        self.assertEqual(len(rows), 1, rows)
        return rows[0]

    def test_an_evaluated_line_is_evaluated(self):
        row = self.run_since([log("decision added", 30), log("decision evaluated", 5)])
        self.assertEqual((row["present"], row["reason"], row["added"], row["evaluated"]), ("1", "evaluated", "1", "1"))

    def test_only_an_added_line_is_only_added(self):
        row = self.run_since([log("decision added", 30)])
        self.assertEqual((row["present"], row["reason"]), ("0", "only_added"))

    def test_no_line_is_none_added(self):
        self.assertEqual(self.run_since([])["reason"], "none_added")

    def test_a_line_before_since_or_of_another_service_does_not_count(self):
        row = self.run_since([log("decision evaluated", 24 * 3), log("decision evaluated", 5, "other")])
        self.assertEqual((row["present"], row["reason"]), ("0", "none_added"))


class DecisionFilesTextTest(unittest.TestCase):
    """These tests read the files as text. They need no ClickHouse."""

    def test_the_evidence_file_follows_the_form_of_the_other_evidence_files(self):
        text = (QUERIES / "evidence" / "decision-tool-1.sql").read_text(encoding="utf-8")
        for word in ("{since:DateTime}", "INTERVAL 30 DAY", "LIMIT 1", "AS present", "AS reason", "otel.otel_logs",
                     "'evaluated'", "'only_added'", "'none_added'"):
            self.assertIn(word, text)
        for word in ("INSERT", "ALTER", "DROP", "CREATE", "DELETE"):
            self.assertNotIn(word, text.upper().replace("--", ""))
        self.assertLess(len(text.encode("utf-8")), 64 * 1024)

    def test_the_card_has_a_seventh_tile_that_wraps_its_file(self):
        tiles = {tile["id"]: tile for tile in json.loads(bp.DASHBOARD.read_text(encoding="utf-8"))["tiles"]}
        self.assertEqual(list(tiles)[-1], "decisions")
        sql = (QUERIES / "decisions.sql").read_text(encoding="utf-8")
        self.assertEqual(tiles["decisions"]["config"]["sqlTemplate"], bp.wrap("decisions", sql))

    def test_the_text_tile_lists_the_card_on_both_sides(self):
        markdown = json.loads(bp.DASHBOARD.read_text(encoding="utf-8"))["tiles"][0]["config"]["markdown"]
        higher, lower = markdown.split("Higher is better: ")[1].split(" Lower is better: ")
        self.assertIn("Decisions (made, evaluated, good process and good outcome)", higher)
        self.assertIn("Decisions (due)", lower)

    def test_the_card_file_is_read_only_and_names_its_signals(self):
        text = (QUERIES / "decisions.sql").read_text(encoding="utf-8")
        for word in ("harold.decisions.added", "harold.decisions.evaluated", "harold.decisions.open", "LIMIT 1"):
            self.assertIn(word, text)
        for word in ("INSERT", "ALTER", "DROP", "CREATE", "DELETE"):
            self.assertNotIn(word, text.upper().replace("--", ""))


if __name__ == "__main__":
    unittest.main()
