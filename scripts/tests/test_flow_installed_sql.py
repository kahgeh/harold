"""Check the installed row of factory-flow.sql on fake rows.

Each test replaces the two metric tables with a SELECT of constants (SELECT only, no write) and sends
the text to the read-only ClickHouse through bin/logq. The tests skip when bin/logq or ClickHouse is
not there, as in CI. The text tests at the end need no ClickHouse.
"""

import unittest

import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import QUERIES, inventory


def install(machine, version, hours_ago, count, total):
    """Return one fake point of harold.release.install_lag with its own Count, Sum and age."""
    return (f"SELECT 'harold.release.install_lag' AS MetricName, "
            f"map('harold.machine', '{machine}') AS ResourceAttributes, "
            f"map('version', '{version}') AS Attributes, "
            f"now() - INTERVAL {hours_ago} HOUR AS TimeUnix, toUInt64({count}) AS Count, toFloat64({total}) AS Sum")


def missing(spec, value):
    """Return one fake point of the gauge pod.evidence.missing, one minute old."""
    return (f"SELECT 'pod.evidence.missing' AS MetricName, "
            f"map('pod.product', 'p', 'spec.id', '{spec}') AS Attributes, "
            f"now() - INTERVAL 1 MINUTE AS TimeUnix, toFloat64({value}) AS Value")


class FlowInstalledSqlTest(unittest.TestCase):
    # Reuse the ClickHouse set-up and the fake-row runner of the spec review test file.
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)
    run_query = base.FlowSqlTest.run_query

    def installed_row(self, points=(), gauge=()):
        rows = self.run_query("factory-flow.sql", histogram=list(points), gauge=list(gauge))
        return {row["stage"]: row for row in rows}["installed"]

    def test_two_installs_on_one_machine_and_one_on_another_give_three_waits(self):
        # The waits are 1000, 3000 and 2000 seconds: the median is 2000. Two waits would give 1000 or 3000.
        row = self.installed_row([install("m1", "v1", 5, 1, 1000), install("m1", "v2", 4, 1, 3000),
                                  install("m2", "v1", 3, 1, 2000)],
                                 [inventory("released_not_installed", 3)])
        self.assertEqual((row["items"], row["median_wait_s"], row["has_data"]), ("3", "2000", "1"))

    def test_a_restart_that_sends_the_same_series_again_does_not_add_a_wait(self):
        # m1 v1 sends the same point twice (Count 1, Sum 1000). Counted twice, the median would be 1000.
        row = self.installed_row([install("m1", "v1", 5, 1, 1000), install("m1", "v1", 4, 1, 1000),
                                  install("m1", "v2", 3, 1, 3000), install("m2", "v1", 2, 1, 2000)])
        self.assertEqual(row["median_wait_s"], "2000")

    def test_a_series_that_restarts_its_count_gives_one_wait(self):
        # m1 v1: Count 2 (Sum 2000), then a reset to Count 1 (Sum 1000). Its wait is 1000.
        row = self.installed_row([install("m1", "v1", 5, 2, 2000), install("m1", "v1", 4, 1, 1000),
                                  install("m1", "v2", 3, 1, 3000), install("m2", "v1", 2, 1, 5000)])
        self.assertEqual(row["median_wait_s"], "3000")

    def test_an_install_older_than_30_days_gives_no_wait(self):
        row = self.installed_row([install("m1", "v1", 24 * 31, 1, 9999)])
        self.assertEqual((row["median_wait_s"], row["has_data"]), ("\\N", "1"))

    def test_with_no_list_and_no_install_the_row_has_no_items_and_no_median(self):
        row = self.installed_row()
        self.assertEqual((row["items"], row["median_wait_s"], row["has_data"]), ("0", "\\N", "1"))

    def test_the_evidence_count_stays_in_its_own_column(self):
        row = self.installed_row([install("m1", "v1", 5, 1, 1000)],
                                 [inventory("released_not_installed", 4), missing("a", 1), missing("b", 1),
                                  missing("c", 0)])
        self.assertEqual((row["items"], row["evidence_missing"]), ("4", "2"))

    def test_the_other_rows_have_a_zero_evidence_count(self):
        rows = self.run_query("factory-flow.sql", gauge=[missing("a", 1)])
        self.assertEqual([row["evidence_missing"] for row in rows if row["stage"] != "installed"],
                         ["0"] * 7)


class FlowInstalledSqlTextTest(unittest.TestCase):
    """These tests read the files as text. They need no ClickHouse."""

    def test_factory_flow_reads_the_install_lag_and_the_list(self):
        text = (QUERIES / "factory-flow.sql").read_text(encoding="utf-8")
        for word in ("'harold.release.install_lag'", "'released_not_installed'", "evidence_missing"):
            self.assertIn(word, text)

    def test_the_columns_line_names_evidence_missing(self):
        text = (QUERIES / "factory-flow.sql").read_text(encoding="utf-8")
        self.assertIn("-- Columns: stage, items, median_wait_s, has_data, evidence_missing.", text)


if __name__ == "__main__":
    unittest.main()
