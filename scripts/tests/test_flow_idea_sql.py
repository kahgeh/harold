"""Check the idea row of factory-flow.sql and the phase filter of delivery-lead-time.sql on fake rows.

Each test replaces the two metric tables with a SELECT of constants (SELECT only, no write) and sends
the text to the read-only ClickHouse through bin/logq. The tests skip when bin/logq or ClickHouse is
not there, as in CI. The text tests at the end need no ClickHouse.
"""

from pathlib import Path
import unittest

import test_flow_spec_review_sql as base
from test_flow_spec_review_sql import FOUR_PHASES, QUERIES, inventory, lead


class FlowIdeaSqlTest(unittest.TestCase):
    # Reuse the ClickHouse set-up and the fake-row runner of the spec review test file.
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)
    run_query = base.FlowSqlTest.run_query

    def flow_rows(self, points, gauge):
        return {row["stage"]: row for row in self.run_query("factory-flow.sql", histogram=points, gauge=gauge)}

    def test_the_idea_row_has_the_idea_items_and_the_median_of_idea_to_open(self):
        # idea_to_open 100, 300 and 200: the median is 200.
        points = [lead("a", "idea_to_open", 100), lead("b", "idea_to_open", 300), lead("c", "idea_to_open", 200)]
        row = self.flow_rows(points, [inventory("idea", 4), inventory("in_progress", 9)])["idea"]
        self.assertEqual((row["items"], row["median_wait_s"], row["has_data"]), ("4", "200", "1"))

    def test_with_no_idea_list_and_no_phase_the_idea_row_has_zero_items_and_no_median(self):
        row = self.flow_rows([lead("a", "approved_to_started", 5)], [inventory("in_progress", 1)])["idea"]
        self.assertEqual((row["items"], row["median_wait_s"], row["has_data"]), ("0", "\\N", "1"))

    def test_the_idea_phase_does_not_change_the_other_rows(self):
        points = [lead("a", "idea_to_open", 99999), lead("a", "approved_to_started", 100),
                  lead("a", "started_to_completed", 400)]
        row = self.flow_rows(points, [inventory("in_progress", 2)])["build"]
        self.assertEqual((row["items"], row["median_wait_s"]), ("2", "500"))

    def test_delivery_lead_time_gives_only_the_four_spec_rows_with_idea_to_open_present(self):
        points = [lead("a", phase, 3600) for phase in FOUR_PHASES + ["idea_to_open"]]
        rows = self.run_query("delivery-lead-time.sql", histogram=points)
        self.assertEqual([row["series"] for row in rows], sorted(f"spec: {phase}" for phase in FOUR_PHASES))


class FlowIdeaSqlTextTest(unittest.TestCase):
    """These tests read the files as text. They need no ClickHouse."""

    def test_factory_flow_reads_the_idea_list_and_phase(self):
        text = (QUERIES / "factory-flow.sql").read_text(encoding="utf-8")
        for word in ("idea", "idea_to_open"):
            self.assertIn(f"'{word}'", text)

    def test_the_two_evidence_queries_name_their_phase_list_and_reasons(self):
        evidence = QUERIES / "evidence"
        one = (evidence / "flow-idea-1.sql").read_text(encoding="utf-8")
        two = (evidence / "flow-idea-2.sql").read_text(encoding="utf-8")
        for word in ("'idea'", "list_seen", "no_list", "{since:DateTime}"):
            self.assertIn(word, one)
        for word in ("'idea_to_open'", "phase_seen", "no_phase_yet", "{since:DateTime}"):
            self.assertIn(word, two)


if __name__ == "__main__":
    unittest.main()
