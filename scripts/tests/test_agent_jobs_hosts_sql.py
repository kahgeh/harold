"""Check evidence/agent-jobs-hosts-1.sql on fake rows.

The tests use the helpers of test_agent_jobs_sql.py: the table otel.otel_metrics_sum becomes a SELECT of
constants (SELECT only, no write), sent to the read-only ClickHouse through bin/logq. They skip when
bin/logq or ClickHouse is not there, as in CI.
"""

import unittest

import test_agent_jobs_sql as jobs
from test_agent_jobs_sql import point


def host_ends(days_ago, value, host="omarchy", end_line="typed"):
    return point("harold.job.ends", {"how": "ended", "end_line": end_line, "host": host}, days_ago, value)


class AgentJobsHostsSqlTest(jobs.AgentJobsSqlTest):
    def test_an_end_with_a_host_after_since_is_host_end_seen(self):
        row = self.run_query("agent-jobs-hosts-1.sql", [host_ends(3, 0), host_ends(0.5, 1)])
        self.assertEqual((row["present"], row["reason"]), ("1", "host_end_seen"))

    def test_an_end_with_an_empty_host_is_no_host_end_yet(self):
        row = self.run_query("agent-jobs-hosts-1.sql", [host_ends(3, 0, ""), host_ends(0.5, 1, "")])
        self.assertEqual((row["present"], row["reason"]), ("0", "no_host_end_yet"))

    def test_an_end_with_a_host_before_since_is_no_host_end_yet(self):
        row = self.run_query("agent-jobs-hosts-1.sql", [host_ends(3, 1), host_ends(2, 1), host_ends(0.5, 1)])
        self.assertEqual((row["present"], row["reason"]), ("0", "no_host_end_yet"))

    def test_no_point_at_all_is_no_host_end_yet(self):
        self.assertEqual(self.run_query("agent-jobs-hosts-1.sql", [])["reason"], "no_host_end_yet")

    def test_the_query_is_a_read_only_select_with_a_limit(self):
        sql = (jobs.QUERIES / "evidence" / "agent-jobs-hosts-1.sql").read_text(encoding="utf-8")
        body = "\n".join(line for line in sql.splitlines() if not line.startswith("--"))
        self.assertTrue(body.lstrip().upper().startswith("SELECT"))
        self.assertIn("LIMIT", body)
        self.assertIn("{since:DateTime}", body)
        for word in ("INSERT", "ALTER", "DROP", "DELETE", "CREATE"):
            self.assertNotIn(word, body.upper())


if __name__ == "__main__":
    unittest.main()
