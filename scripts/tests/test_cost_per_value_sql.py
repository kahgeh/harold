"""Check the files of the spec cost-per-value: queries/cost-per-value.sql, queries/fix-cost-by-cause.sql
and queries/evidence/cost-per-value-1.sql.

These tests read the files as text. They need no ClickHouse, no port and no server.
The fixture tests of the two queries run in the container test set (scripts/container-tests/port-tests.txt).
"""

import re
import unittest

from test_flow_spec_review_sql import QUERIES

COST = (QUERIES / "cost-per-value.sql").read_text()
CAUSE = (QUERIES / "fix-cost-by-cause.sql").read_text()
EVIDENCE = (QUERIES / "evidence" / "cost-per-value-1.sql").read_text()


def header(text):
    return "\n".join(line for line in text.splitlines() if line.startswith("--"))


def body(text):
    return "\n".join(line for line in text.splitlines() if not line.startswith("--"))


def columns(text):
    """Return the names in the `Columns:` comment, joined across its lines."""
    match = re.search(r"-- Columns: (.*?)\.\n(?!--\s+\w+,)", text + "\n", re.S)
    line = " ".join(part.lstrip("- ").strip() for part in match.group(1).splitlines())
    return [name.strip() for name in line.split(",")]


class CostPerValueQuery(unittest.TestCase):
    def test_the_columns_are_the_ones_the_spec_names(self):
        self.assertEqual(columns(COST), [
            "cost_per_value", "cost_per_value_change_pct", "value", "total_cost", "token_cost",
            "attention_cost", "cloud_cost", "fix_cost", "unpriced_models", "unvalued"])

    def test_the_select_returns_each_column_under_its_name(self):
        final = body(COST).split("\nSELECT\n")[-1]
        for name in columns(COST):
            self.assertRegex(final, re.compile(rf"\bAS {name}\b|^\s+{name}$", re.M), name)

    def test_the_header_names_the_read_windows_and_the_only_cloud_source(self):
        text = header(COST)
        for part in ("21 days", "15 days", "14 days", "watch.cost_usd", "only the newest list prices",
                     "Build hours are not in the cost"):
            self.assertIn(part.lower(), text.lower(), part)

    def test_the_query_reads_and_never_writes(self):
        text = body(COST).upper()
        for word in ("INSERT", "ALTER", "DROP", "CREATE", "TRUNCATE", "DELETE"):
            self.assertNotRegex(text, rf"\b{word}\b", word)
        self.assertIn("LIMIT 1", text)

    def test_the_comments_name_no_table_that_the_fixture_replaces(self):
        for name in ("otel.otel_metrics_sum", "otel.otel_metrics_gauge", "otel.otel_metrics_histogram",
                     "otel.otel_logs", "otel.harold_settings"):
            self.assertNotIn(name, header(COST), name)
            self.assertNotIn(name, header(CAUSE), name)

    def test_the_weights_come_from_the_gauge_and_a_feature_without_one_counts_one(self):
        self.assertIn("'pod.spec.value'", COST)
        self.assertIn("if(weight = 0, 1, weight)", COST)

    def test_only_the_other_specs_count_by_their_merge(self):
        self.assertIn("'started_to_completed'", COST)
        self.assertIn("kind = 'other'", COST)
        self.assertNotIn("pod.fix.confirmed_at", body(COST))


class FixCostByCauseQuery(unittest.TestCase):
    def test_the_columns_and_the_window(self):
        self.assertEqual(columns(CAUSE), ["cause", "fixes", "fix_cost", "last_seen"])
        self.assertIn("INTERVAL 30 DAY", body(CAUSE))
        self.assertIn("ORDER BY last_seen DESC", body(CAUSE))
        self.assertIn("LIMIT", body(CAUSE))

    def test_it_reads_the_fix_specs_by_their_cause(self):
        self.assertIn("'spec.kind'] = 'fix'", body(CAUSE))
        self.assertIn("'spec.cause'", body(CAUSE))


SLICING = (QUERIES / "slicing-and-sizing.sql").read_text()


class SlicingAndSizingQuery(unittest.TestCase):
    ROW = "specs over 8 build hours, share of specs (%)"

    def test_it_has_the_row_over_all_specs_whatever_their_size(self):
        self.assertEqual(body(SLICING).count(f"'{self.ROW}'"), 1)
        branch = body(SLICING).split(f"'{self.ROW}'")[1].split("UNION ALL")[0]
        self.assertIn("> 8", branch)
        self.assertNotIn("spec.size", branch)

    def test_the_header_states_both_changes(self):
        text = header(SLICING)
        self.assertIn("specs over 8 build hours", text)
        self.assertIn("split rule", text)
        self.assertIn("30 days after the last older spec merges", text)


class EvidenceQuery(unittest.TestCase):
    def test_it_follows_the_evidence_standard(self):
        text = body(EVIDENCE)
        self.assertIn("{since:DateTime}", EVIDENCE)
        self.assertIn("LIMIT", text.upper())
        for reason in ("value_seen", "no_value_yet", "no_values"):
            self.assertIn(reason, EVIDENCE)
        for column in ("present", "reason"):
            self.assertRegex(text, rf"\bAS {column}\b")
        self.assertNotRegex(text.upper(), r"\b(INSERT|ALTER|DROP|CREATE|TRUNCATE|DELETE)\b")


if __name__ == "__main__":
    unittest.main()
