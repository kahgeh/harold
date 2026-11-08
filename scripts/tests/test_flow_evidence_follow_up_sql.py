"""Check the installed row of factory-flow.sql (the column evidence_missing: the count of specs installed without evidence) on fake rows.

Each test replaces the two metric tables with a SELECT of constants (SELECT only, no write) and sends
the text to the read-only ClickHouse through bin/logq. The tests skip when bin/logq or ClickHouse is
not there, as in CI. The text tests at the end need no ClickHouse.
"""

import unittest

import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import QUERIES


def missing(spec, minutes_ago, value, product="p"):
    """Return one fake point of the gauge pod.evidence.missing."""
    return (f"SELECT 'pod.evidence.missing' AS MetricName, map('pod.product', '{product}', 'spec.id', '{spec}') AS Attributes, "
            f"now() - INTERVAL {minutes_ago} MINUTE AS TimeUnix, toFloat64({value}) AS Value")


class FlowEvidenceFollowUpSqlTest(unittest.TestCase):
    # Reuse the ClickHouse set-up and the fake-row runner of the spec review test file.
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)
    run_query = base.FlowSqlTest.run_query

    def installed_row(self, points):
        rows = self.run_query("factory-flow.sql", gauge=points)
        return {row["stage"]: row for row in rows}["installed"]

    def test_two_specs_at_1_and_one_at_0_give_2(self):
        row = self.installed_row([missing("a", 1, 1), missing("b", 2, 1), missing("c", 1, 0)])
        self.assertEqual((row["evidence_missing"], row["median_wait_s"], row["has_data"]), ("2", "\\N", "1"))

    def test_a_point_older_than_10_minutes_does_not_count(self):
        row = self.installed_row([missing("a", 1, 1), missing("b", 11, 1)])
        self.assertEqual(row["evidence_missing"], "1")

    def test_the_newest_point_of_a_spec_decides(self):
        row = self.installed_row([missing("a", 8, 1), missing("a", 2, 0), missing("b", 8, 0), missing("b", 2, 1)])
        self.assertEqual(row["evidence_missing"], "1")

    def test_the_same_spec_id_in_two_products_counts_twice(self):
        row = self.installed_row([missing("a", 1, 1, "p"), missing("a", 1, 1, "q")])
        self.assertEqual(row["evidence_missing"], "2")

    def test_with_no_point_the_count_is_0(self):
        self.assertEqual(self.installed_row([])["evidence_missing"], "0")


class FlowEvidenceFollowUpSqlTextTest(unittest.TestCase):
    """These tests read the files as text. They need no ClickHouse."""

    def test_factory_flow_reads_the_evidence_gauge_of_the_last_10_minutes(self):
        text = (QUERIES / "factory-flow.sql").read_text(encoding="utf-8")
        for word in ("'pod.evidence.missing'", "INTERVAL 10 MINUTE"):
            self.assertIn(word, text)

    def test_the_evidence_query_names_its_counter_and_reasons(self):
        one = (QUERIES / "evidence" / "evidence-follow-up-1.sql").read_text(encoding="utf-8")
        for word in ("harold.evidence.lines", "'typed'", "pod.evidence.missing", "line_typed", "none_missing",
                     "line_not_typed", "no_gauge"):
            self.assertIn(word, one)


if __name__ == "__main__":
    unittest.main()
