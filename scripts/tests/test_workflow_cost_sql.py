"""Check queries/workflow-cost.sql and queries/evidence/workflow-cost-1.sql of the spec workflow-cost.

The fixture tests replace the table otel.otel_metrics_sum with a SELECT of constants (SELECT only, no write)
and send the text to the read-only ClickHouse through bin/logq. They skip when bin/logq or ClickHouse is not
there, as in CI. The text tests need no ClickHouse.
"""

import re
import subprocess
import unittest

import test_flow_spec_review_sql as base
from test_agent_jobs_sql import EMPTY_SUM, point
from test_flow_spec_review_sql import QUERIES

COST = (QUERIES / "workflow-cost.sql").read_text(encoding="utf-8")
EVIDENCE = (QUERIES / "evidence" / "workflow-cost-1.sql").read_text(encoding="utf-8")
README = (QUERIES / "README.md").read_text(encoding="utf-8")
COLUMNS = ["workflow", "workflow.step", "spec.id", "tokens", "cost_aud", "prior_cost_aud", "change_pct", "flag"]


def body(text):
    return "\n".join(line for line in text.splitlines() if not line.startswith("--"))


def cost(workflow, days_ago, value, step="run", spec="none", model="claude-x"):
    attrs = {"workflow": workflow, "workflow.step": step, "spec.id": spec, "model": model, "pod.product": "harold"}
    return point("harold.workflow.cost", attrs, days_ago, value)


def tokens(workflow, days_ago, value, kind="input", step="run", spec="none"):
    attrs = {"workflow": workflow, "workflow.step": step, "spec.id": spec, "model": "claude-x",
             "pod.product": "harold", "kind": kind}
    return point("harold.workflow.tokens", attrs, days_ago, value)


class QueryRunner(unittest.TestCase):
    setUpClass = classmethod(base.FlowSqlTest.setUpClass.__func__)

    def run_sql(self, sql, rows):
        sql = sql.replace("{since:DateTime}", "(now() - INTERVAL 86400 SECOND)")
        sql = sql.replace("otel.otel_metrics_sum", "(" + " UNION ALL ".join(rows or [EMPTY_SUM]) + ")")
        done = subprocess.run([self.logq], input=sql.rstrip() + "\nFORMAT TSVWithNames", capture_output=True,
                              text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        lines = [line.split("\t") for line in done.stdout.splitlines()]
        return [dict(zip(lines[0], line)) for line in lines[1:]]


class WorkflowCostFixture(QueryRunner):
    def rows(self):
        return [
            # doubled: 1 prior, 3 now
            cost("doubled", 20, 0), cost("doubled", 10, 1), cost("doubled", 3, 4),
            tokens("doubled", 20, 0), tokens("doubled", 3, 100), tokens("doubled", 3, 50, "output"),
            # new: no prior cost
            cost("fresh", 20, 0), cost("fresh", 3, 2),
            # steady: 1 and 1
            cost("steady", 20, 0), cost("steady", 10, 1), cost("steady", 3, 2),
            # falling: 4 prior, 1 now
            cost("falling", 20, 0), cost("falling", 10, 4), cost("falling", 3, 5),
            # exactly twice is not doubled
            cost("twice", 20, 0), cost("twice", 10, 1), cost("twice", 3, 3),
        ]

    def test_the_flags_and_the_order(self):
        got = self.run_sql(COST, self.rows())
        self.assertEqual([row["workflow"] for row in got], ["doubled", "fresh", "twice", "steady", "falling"])
        self.assertEqual([row["flag"] for row in got], ["doubled", "new", "", "", ""])
        self.assertEqual(list(got[0]), COLUMNS)

    def test_a_row_at_three_times_the_prior_cost_is_doubled(self):
        row = self.run_sql(COST, self.rows())[0]
        self.assertEqual((float(row["cost_aud"]), float(row["prior_cost_aud"]), row["change_pct"]), (3.0, 1.0, "200"))
        self.assertEqual(row["tokens"], "150")

    def test_a_row_with_no_prior_cost_is_new_with_no_change(self):
        row = self.run_sql(COST, self.rows())[1]
        self.assertEqual((float(row["cost_aud"]), float(row["prior_cost_aud"]), row["change_pct"]), (2.0, 0.0, "\\N"))

    def test_the_other_rows_go_by_change_highest_first(self):
        got = self.run_sql(COST, self.rows())
        self.assertEqual([row["change_pct"] for row in got[2:]], ["100", "0", "-75"])

    def test_a_seeded_series_is_a_row_with_zeros(self):
        rows = [cost("seeded", 20, 0), cost("seeded", 3, 0, model="none"), tokens("seeded", 3, 0)]
        got = self.run_sql(COST, rows)
        self.assertEqual([(row["workflow"], row["tokens"], float(row["cost_aud"]), row["flag"]) for row in got],
                         [("seeded", "0", 0.0, "")])


class WorkflowCostEvidenceFixture(QueryRunner):
    def test_two_workflows_with_a_cost_after_since_are_recorded(self):
        rows = [cost("a", 3, 0), cost("a", 0.5, 1), cost("b", 3, 0), cost("b", 0.5, 2)]
        row = self.run_sql(EVIDENCE, rows)[0]
        self.assertEqual((row["present"], row["reason"]), ("1", "workflow_cost_recorded"))

    def test_one_workflow_only(self):
        rows = [cost("a", 3, 0), cost("a", 0.5, 1), cost("b", 3, 0), cost("b", 0.5, 0, model="none")]
        row = self.run_sql(EVIDENCE, rows)[0]
        self.assertEqual((row["present"], row["reason"]), ("0", "one_workflow_only"))

    def test_seeded_zero_points_are_no_workflow_run_yet(self):
        rows = [cost("a", 3, 0, model="none"), cost("a", 0.5, 0, model="none"), cost("b", 0.5, 0, model="none")]
        row = self.run_sql(EVIDENCE, rows)[0]
        self.assertEqual((row["present"], row["reason"]), ("0", "no_workflow_run_yet"))

    def test_a_cost_before_since_is_no_workflow_run_yet(self):
        rows = [cost("a", 3, 1), cost("a", 2, 1), cost("b", 3, 1), cost("b", 2, 1)]
        row = self.run_sql(EVIDENCE, rows)[0]
        self.assertEqual((row["present"], row["reason"]), ("0", "no_workflow_run_yet"))

    def test_no_point_at_all_is_no_workflow_run_yet(self):
        self.assertEqual(self.run_sql(EVIDENCE, [])[0]["reason"], "no_workflow_run_yet")


class WorkflowCostText(unittest.TestCase):
    def test_the_columns_are_the_ones_the_spec_names(self):
        match = re.search(r"-- Columns: (.*?)\.\n", COST, re.S)
        names = [name.strip(" `") for name in match.group(1).replace("\n-- ", " ").split(",")]
        self.assertEqual(names, COLUMNS)

    def test_the_header_names_the_windows_the_flags_and_the_order(self):
        text = "\n".join(line for line in COST.splitlines() if line.startswith("--")).lower()
        for part in ("note:", "waits for:", "7 days", "doubled", "new", "21 days", "counter rule"):
            self.assertIn(part, text, part)

    def test_the_query_reads_the_two_counters_and_never_writes(self):
        for name in ("harold.workflow.cost", "harold.workflow.tokens"):
            self.assertIn(f"'{name}'", COST)
        self.assertNotRegex(body(COST).upper(), r"\b(INSERT|ALTER|DROP|CREATE|TRUNCATE|DELETE)\b")
        self.assertIn("LIMIT", body(COST).upper())

    def test_the_evidence_query_follows_the_evidence_standard(self):
        text = body(EVIDENCE)
        for part in ("{since:DateTime}", "INTERVAL 30 DAY", "LIMIT 1", "workflow_cost_recorded",
                     "one_workflow_only", "no_workflow_run_yet", "harold.workflow.cost"):
            self.assertIn(part, EVIDENCE, part)
        for column in ("present", "reason"):
            self.assertRegex(text, rf"\bAS {column}\b")
        self.assertNotRegex(text.upper(), r"\b(INSERT|ALTER|DROP|CREATE|TRUNCATE|DELETE)\b")
        self.assertLess(len(EVIDENCE.encode("utf-8")), 64 * 1024)

    def test_the_readmes_list_both_files(self):
        self.assertIn("`workflow-cost.sql`", README)
        evidence_readme = (QUERIES / "evidence" / "README.md").read_text(encoding="utf-8")
        self.assertIn("`workflow-cost-1.sql`", evidence_readme)


if __name__ == "__main__":
    unittest.main()
