"""Check the build and merge rows of factory-flow.sql, and the ready phases in delivery-lead-time.sql, on fake rows.

Each test replaces the two metric tables with a SELECT of constants (SELECT only, no write) and sends
the text to the read-only ClickHouse through bin/logq. The tests skip when bin/logq or ClickHouse is
not there, as in CI. The text tests at the end need no ClickHouse.
"""

from pathlib import Path
import unittest

import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import FOUR_PHASES, QUERIES, inventory, lead

READY_PHASES = ["started_to_ready", "ready_to_completed"]


class FlowMergeSqlTest(unittest.TestCase):
    # Reuse the ClickHouse set-up and the fake-row runner of the spec review test file.
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)
    run_query = base.FlowSqlTest.run_query

    def flow_rows(self):
        points = []
        # Specs with a ready file: build = approved_to_started + started_to_ready (250, 30, 2).
        for spec, (start, ready, rest, whole) in {"a": (100, 150, 50, 200), "d": (10, 20, 70, 90),
                                                  "e": (1, 1, 60, 61)}.items():
            points += [lead(spec, "approved_to_started", start), lead(spec, "started_to_ready", ready),
                       lead(spec, "ready_to_completed", rest), lead(spec, "started_to_completed", whole)]
        # Specs with no ready file: the old rule, approved_to_started + started_to_completed (500, 600).
        points += [lead("b", "approved_to_started", 100), lead("b", "started_to_completed", 400),
                   lead("f", "approved_to_started", 100), lead("f", "started_to_completed", 500)]
        # A spec with only one build phase gives no build wait.
        points += [lead("g", "started_to_completed", 9999)]
        gauge = [inventory("awaiting_approval", 5), inventory("in_spec_writing", 1),
                 inventory("in_architect_review", 2), inventory("approved_not_started", 3),
                 inventory("in_progress", 4), inventory("ready_not_merged", 1)]
        return {row["stage"]: row for row in self.run_query("factory-flow.sql", histogram=points, gauge=gauge)}

    def test_delivery_lead_time_gives_only_the_four_spec_rows_with_the_ready_phases_present(self):
        points = [lead("a", phase, 3600) for phase in FOUR_PHASES + READY_PHASES]
        rows = self.run_query("delivery-lead-time.sql", histogram=points)
        self.assertEqual([row["series"] for row in rows], sorted(f"spec: {phase}" for phase in FOUR_PHASES))

    def test_the_build_median_ends_at_the_ready_time_for_a_spec_with_a_ready_file(self):
        # 2, 30, 250 (ready), 500 and 600 (old rule): the median is 250.
        row = self.flow_rows()["build"]
        self.assertEqual((row["median_wait_s"], row["has_data"]), ("250", "1"))

    def test_the_merge_row_has_the_ready_not_merged_items_and_the_merge_median(self):
        # ready_to_completed 50, 70 and 60: the median is 60.
        row = self.flow_rows()["merge"]
        self.assertEqual((row["items"], row["median_wait_s"], row["has_data"]), ("1", "60", "1"))

    def test_the_item_counts_add_up_to_the_three_waiting_lists(self):
        rows = self.flow_rows()
        stages = ["spec", "architect review", "approval", "build", "merge"]
        self.assertEqual([rows[stage]["items"] for stage in stages], ["1", "2", "2", "6", "1"])
        self.assertEqual(sum(int(rows[stage]["items"]) for stage in stages), 5 + 3 + 4)

    def test_with_no_ready_list_the_merge_row_is_empty_with_data(self):
        rows = {row["stage"]: row for row in self.run_query(
            "factory-flow.sql", histogram=[lead("b", "approved_to_started", 100), lead("b", "started_to_completed", 400)],
            gauge=[inventory("in_progress", 2)])}
        self.assertEqual((rows["merge"]["items"], rows["merge"]["median_wait_s"], rows["merge"]["has_data"]),
                         ("0", "\\N", "1"))
        self.assertEqual((rows["build"]["items"], rows["build"]["median_wait_s"]), ("2", "500"))


class FlowMergeSqlTextTest(unittest.TestCase):
    """These tests read the files as text. They need no ClickHouse."""

    def test_factory_flow_reads_the_ready_list_and_phases(self):
        text = (QUERIES / "factory-flow.sql").read_text(encoding="utf-8")
        for word in ("ready_not_merged", "started_to_ready", "ready_to_completed"):
            self.assertIn(f"'{word}'", text)


if __name__ == "__main__":
    unittest.main()
