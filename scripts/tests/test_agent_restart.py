"""Check the two queries of the spec agent-restart: the named query restart-checks and the
evidence query agent-restart-1. The text tests need no ClickHouse. The fixture tests run through
bin/logq on fixture rows, as the other query tests do."""

import sys
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import test_decision_tool_sql as dt  # noqa: E402

QUERIES = SCRIPTS.parent / 'docs' / 'references' / 'telemetry' / 'queries'
EVIDENCE = QUERIES / 'evidence' / 'agent-restart-1.sql'
NAMED = QUERIES / 'restart-checks.sql'
WRITE_WORDS = ('INSERT', 'ALTER', 'DROP', 'CREATE', 'DELETE')


def result_log(group, result='ok', hours_ago=5, check='identity', role='pnem', service='harold'):
    attrs = (f"map('role', '{role}', 'group', '{group}', 'check', '{check}', 'result', '{result}')")
    return (f"SELECT '{service}' AS ServiceName, 'restart check result' AS Body, {attrs} AS LogAttributes, "
            f"now() - INTERVAL {int(hours_ago * 3600)} SECOND AS Timestamp")


class QueryTextTests(unittest.TestCase):
    """These tests read the files as text. They need no ClickHouse."""

    def test_the_evidence_file_follows_the_form_of_the_other_evidence_files(self):
        text = EVIDENCE.read_text(encoding='utf-8')
        for word in ('{since:DateTime}', 'INTERVAL 30 DAY', 'LIMIT 1', 'AS present', 'AS reason', 'AS checks',
                     'groups', 'otel.otel_logs', "'checked'", "'checked_identity_communication'",
                     "'no_restart_yet'", '-- Note:', '-- Waits for:', '-- Shows:', '-- Columns:'):
            self.assertIn(word, text)
        for word in WRITE_WORDS:
            self.assertNotIn(word, text.upper().replace('--', ''))
        self.assertLess(len(text.encode('utf-8')), 64 * 1024)

    def test_the_named_query_is_a_read_only_select_with_a_limit(self):
        text = NAMED.read_text(encoding='utf-8')
        for word in ('SELECT', 'LIMIT', 'INTERVAL 7 DAY', "'restart check result'", "'fail'", 'AS fails',
                     '-- Note:', '-- Waits for:', '-- Columns:'):
            self.assertIn(word, text)
        for word in WRITE_WORDS:
            self.assertNotIn(word, text.upper().replace('--', ''))


class EvidenceQueryFixtureTests(dt.LogqTest):
    def run_since(self, logs=()):
        sql = EVIDENCE.read_text(encoding='utf-8')
        sql = sql.replace('{since:DateTime}', '(now() - INTERVAL 172800 SECOND)')
        sql = sql.replace('otel.otel_logs', '(' + ' UNION ALL '.join(logs or [dt.EMPTY_LOGS]) + ')')
        rows = self.rows(sql)
        self.assertEqual(len(rows), 1, rows)
        return rows[0]

    def test_identity_and_communication_lines_alone_give_checked_identity_communication(self):
        row = self.run_since([result_log('identity'), result_log('communication', check='chief')])
        self.assertEqual((row['present'], row['reason'], row['checks'], row['groups']),
                         ('1', 'checked_identity_communication', '2', '2'))

    def test_all_four_groups_give_checked(self):
        row = self.run_since([result_log(group) for group in ('identity', 'communication', 'setup', 'memory')])
        self.assertEqual((row['present'], row['reason'], row['groups']), ('1', 'checked', '4'))

    def test_one_group_alone_no_line_a_line_before_since_or_of_another_service_is_no_restart_yet(self):
        self.assertEqual(self.run_since([result_log('identity')])['reason'], 'no_restart_yet')
        self.assertEqual(self.run_since([])['reason'], 'no_restart_yet')
        old = [result_log('identity', hours_ago=24 * 3), result_log('communication', hours_ago=24 * 3)]
        self.assertEqual(self.run_since(old)['reason'], 'no_restart_yet')
        other = [result_log('identity', service='other'), result_log('communication', service='other')]
        self.assertEqual(self.run_since(other)['reason'], 'no_restart_yet')


class NamedQueryFixtureTests(dt.LogqTest):
    def test_only_fail_lines_of_the_last_7_days_count_by_role_group_and_check(self):
        logs = [result_log('communication', 'fail', 5, 'chief'), result_log('communication', 'fail', 6, 'chief'),
                result_log('identity', 'fail', 7), result_log('identity', 'ok', 5),
                result_log('identity', 'fail', 24 * 10)]
        sql = NAMED.read_text(encoding='utf-8').replace('otel.otel_logs', '(' + ' UNION ALL '.join(logs) + ')')
        rows = self.rows(sql)
        self.assertEqual([(r['group'], r['check'], r['fails']) for r in rows],
                         [('communication', 'chief', '2'), ('identity', 'identity', '1')])


if __name__ == '__main__':
    unittest.main()
