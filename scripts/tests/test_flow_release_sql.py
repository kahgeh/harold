"""Check the release row of factory-flow.sql on fake rows.

Each test replaces the two metric tables with a SELECT of constants (SELECT only, no write) and sends
the text to the read-only ClickHouse through bin/logq. The tests skip when bin/logq or ClickHouse is
not there, as in CI. The text tests at the end need no ClickHouse.
"""

import unittest

import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import EMPTY_MAP, QUERIES, inventory, lead


def point(spec, hours_ago, count, total):
    """Return one fake point of merged_to_released with its own Count, Sum and age."""
    return (f"SELECT 'pod.spec.lead_time' AS MetricName, {EMPTY_MAP} AS ResourceAttributes, "
            f"map('phase', 'merged_to_released', 'spec.id', '{spec}', 'pod.product', 'p') AS Attributes, "
            f"now() - INTERVAL {hours_ago} HOUR AS TimeUnix, toUInt64({count}) AS Count, toFloat64({total}) AS Sum")


class FlowReleaseSqlTest(unittest.TestCase):
    # Reuse the ClickHouse set-up and the fake-row runner of the spec review test file.
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)
    run_query = base.FlowSqlTest.run_query

    def release_row(self, points):
        rows = self.run_query("factory-flow.sql", histogram=points, gauge=[inventory("merged_not_released", 4)])
        return {row["stage"]: row for row in rows}["release"]

    def test_three_specs_with_merged_to_released_give_their_median(self):
        row = self.release_row([lead("a", "merged_to_released", 7200), lead("b", "merged_to_released", 1800),
                                lead("c", "merged_to_released", 3600)])
        self.assertEqual((row["items"], row["median_wait_s"], row["has_data"]), ("4", "3600", "1"))

    def test_with_no_merged_to_released_point_the_median_is_empty_and_the_count_stays(self):
        row = self.release_row([lead("a", "ready_to_completed", 100)])
        self.assertEqual((row["items"], row["median_wait_s"], row["has_data"]), ("4", "\\N", "1"))

    def test_a_restart_that_sends_the_series_again_does_not_add_a_wait(self):
        # Spec a: Count 2 then a counter reset to Count 1 (waits 1000 and 1000). Its wait is 1000, not 2000.
        # With specs b 1500 and c 1200 the median is 1200. A summed wait for a would give 1500.
        row = self.release_row([point("a", 3, 2, 2000), point("a", 2, 1, 1000),
                                lead("b", "merged_to_released", 1500), lead("c", "merged_to_released", 1200)])
        self.assertEqual(row["median_wait_s"], "1200")

    def test_the_same_point_sent_twice_does_not_add_a_wait(self):
        row = self.release_row([point("a", 3, 1, 1000), point("a", 2, 1, 1000),
                                lead("b", "merged_to_released", 1500), lead("c", "merged_to_released", 1200)])
        self.assertEqual(row["median_wait_s"], "1200")

    def test_a_point_older_than_30_days_gives_no_wait(self):
        row = self.release_row([point("a", 24 * 31, 1, 9999)])
        self.assertEqual(row["median_wait_s"], "\\N")


class FlowReleaseSqlTextTest(unittest.TestCase):
    """These tests read the files as text. They need no ClickHouse."""

    def test_factory_flow_reads_the_merged_to_released_phase(self):
        text = (QUERIES / "factory-flow.sql").read_text(encoding="utf-8")
        self.assertIn("'merged_to_released'", text)

    def test_the_evidence_query_names_its_phase_and_reasons(self):
        one = (QUERIES / "evidence" / "flow-release-1.sql").read_text(encoding="utf-8")
        for word in ("'merged_to_released'", "phase_seen", "no_release_yet"):
            self.assertIn(word, one)


if __name__ == "__main__":
    unittest.main()
