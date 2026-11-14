"""Check the start line and the installer of the spec pod-role-record: scripts/claude-start and
register_session_start_hook. No port, no real home folder: a fake `claude` in a temp folder and
a temp settings file."""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import test_decision_tool_sql as dt  # noqa: E402

CLAUDE_START = SCRIPTS / 'claude-start'
FAKE_CLAUDE = '#!/bin/sh\nfor argument in "$@"; do printf "%s\\n" "$argument"; done > "$ARGS_FILE"\n'


class ClaudeStartTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        fake = self.root / 'bin' / 'claude'
        fake.parent.mkdir()
        fake.write_text(FAKE_CLAUDE)
        fake.chmod(0o755)
        self.args_file = self.root / 'args.txt'
        self.env = {'PATH': '{}:/usr/bin:/bin'.format(fake.parent), 'ARGS_FILE': str(self.args_file)}

    def run_start(self, *arguments):
        return subprocess.run([sys.executable, str(CLAUDE_START), *arguments], env=self.env,
                              capture_output=True, text=True, cwd=self.root)

    def test_builds_the_short_first_message(self):
        prompt = self.root / 'start-pnem.txt'
        prompt.write_text('long prompt')
        result = self.run_start('opus', str(prompt))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.args_file.read_text().splitlines(),
                         ['--dangerously-skip-permissions', '--model', 'opus',
                          'Read {} and follow it.'.format(prompt)])

    def test_refuses_a_prompt_file_that_does_not_exist(self):
        result = self.run_start('opus', str(self.root / 'missing.txt'))
        self.assertEqual(result.returncode, 1)
        self.assertFalse(self.args_file.exists())

    def test_refuses_a_wrong_argument_count(self):
        self.assertEqual(self.run_start('opus').returncode, 2)


class SessionStartHookTests(unittest.TestCase):
    def setUp(self):
        import install
        self.install = install
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.path = root / '.claude' / 'settings.json'
        self.hook = root / 'bin/harold/hooks/claude_session_start.py'
        self.command = 'python3 {}'.format(self.hook)
        self.path.parent.mkdir()

    def write(self, data):
        self.path.write_text(json.dumps(data))

    def read(self):
        return json.loads(self.path.read_text())

    def register(self):
        return self.install.register_session_start_hook(self.path, self.hook)

    def entry(self, command):
        return {'matcher': '', 'hooks': [{'type': 'command', 'command': command}]}

    def test_adds_the_hook_once(self):
        self.write({'model': 'x'})
        self.assertEqual(self.register(), 'added')
        self.assertEqual(self.register(), 'unchanged')
        self.assertEqual(self.read()['hooks']['SessionStart'], [self.entry(self.command)])
        self.assertEqual(self.read()['model'], 'x')

    def test_leaves_other_hooks_as_they_are(self):
        other = {'matcher': 'startup', 'hooks': [{'type': 'command', 'command': 'echo other'}]}
        stop = self.entry('python3 /somewhere/claude_turn_complete.py')
        self.write({'hooks': {'SessionStart': [other], 'Stop': [stop]}})
        self.assertEqual(self.register(), 'added')
        after = self.read()['hooks']
        self.assertEqual(after['SessionStart'], [other, self.entry(self.command)])
        self.assertEqual(after['Stop'], [stop])

    def test_an_old_path_of_the_same_hook_is_replaced(self):
        self.write({'hooks': {'SessionStart': [self.entry('python3 /old/bin/harold/hooks/claude_session_start.py')]}})
        self.assertEqual(self.register(), 'replaced')
        self.assertEqual(self.read()['hooks']['SessionStart'], [self.entry(self.command)])

    def test_a_file_that_is_not_json_is_left_and_the_entry_is_given(self):
        self.path.write_text('{not json')
        outcome = self.register()
        self.assertEqual(self.path.read_text(), '{not json')
        self.assertIn(self.command, outcome)
        self.assertIn('"SessionStart"', outcome)


EVIDENCE = SCRIPTS.parent / 'docs' / 'references' / 'telemetry' / 'queries' / 'evidence' / 'pod-role-record-1.sql'


def role_log(body, hours_ago, role='', outcome='', service='harold'):
    attrs = []
    if role:
        attrs += ["'role'", f"'{role}'"]
    if outcome:
        attrs += ["'outcome'", f"'{outcome}'"]
    mapping = f"map({', '.join(attrs)})" if attrs else dt.EMPTY_MAP
    return (f"SELECT '{service}' AS ServiceName, '{body}' AS Body, {mapping} AS LogAttributes, "
            f"now() - INTERVAL {int(hours_ago * 3600)} SECOND AS Timestamp")


def recorded_all():
    return [role_log('pod role recorded', 30, role) for role in ('chief', 'pnem', 'pod_coordination')]


class EvidenceQueryTextTests(unittest.TestCase):
    """These tests read the file as text. They need no ClickHouse."""

    def test_the_file_follows_the_form_of_the_other_evidence_files(self):
        text = EVIDENCE.read_text(encoding='utf-8')
        for word in ('{since:DateTime}', 'INTERVAL 14 DAY', 'LIMIT 1', 'AS present', 'AS reason', 'AS recorded',
                     'AS found', 'AS faults', 'otel.otel_logs', "'lookups_found'", "'lookup_faults'",
                     "'no_lookup_yet'", '-- Note:', '-- Waits for:', '-- Shows:', '-- Columns:'):
            self.assertIn(word, text)
        for word in ('INSERT', 'ALTER', 'DROP', 'CREATE', 'DELETE'):
            self.assertNotIn(word, text.upper().replace('--', ''))
        self.assertLess(len(text.encode('utf-8')), 64 * 1024)


class EvidenceQueryFixtureTests(dt.LogqTest):
    def run_since(self, logs=()):
        sql = EVIDENCE.read_text(encoding='utf-8')
        sql = sql.replace('{since:DateTime}', '(now() - INTERVAL 172800 SECOND)')
        sql = sql.replace('otel.otel_logs', '(' + ' UNION ALL '.join(logs or [dt.EMPTY_LOGS]) + ')')
        rows = self.rows(sql)
        self.assertEqual(len(rows), 1, rows)
        return rows[0]

    def test_every_role_recorded_and_a_found_lookup_without_fault_is_found(self):
        row = self.run_since(recorded_all() + [role_log('pod role lookup', 5, 'pnem', 'found')])
        self.assertEqual((row['present'], row['reason'], row['recorded'], row['found'], row['faults']),
                         ('1', 'lookups_found', '3', '1', '0'))

    def test_a_no_record_or_unreadable_lookup_is_a_fault(self):
        for outcome in ('no_record', 'session_file_unreadable'):
            row = self.run_since(recorded_all() + [role_log('pod role lookup', 5, 'pnem', 'found'),
                                                   role_log('pod role lookup', 4, 'pnem', outcome)])
            self.assertEqual((row['present'], row['reason'], row['faults']), ('0', 'lookup_faults', '1'), outcome)

    def test_agent_gone_is_no_fault(self):
        row = self.run_since(recorded_all() + [role_log('pod role lookup', 5, 'pnem', 'found'),
                                               role_log('pod role lookup', 4, 'pnem', 'agent_gone')])
        self.assertEqual((row['reason'], row['faults']), ('lookups_found', '0'))

    def test_a_role_with_no_recorded_line_or_no_found_lookup_is_no_lookup_yet(self):
        row = self.run_since([role_log('pod role recorded', 30, 'chief'),
                              role_log('pod role lookup', 5, 'chief', 'found')])
        self.assertEqual((row['present'], row['reason']), ('0', 'no_lookup_yet'))
        self.assertEqual(self.run_since(recorded_all())['reason'], 'no_lookup_yet')
        self.assertEqual(self.run_since([])['reason'], 'no_lookup_yet')

    def test_a_line_before_since_or_of_another_service_does_not_count(self):
        row = self.run_since(recorded_all() + [role_log('pod role lookup', 24 * 3, 'pnem', 'found'),
                                               role_log('pod role lookup', 5, 'pnem', 'found', 'other')])
        self.assertEqual(row['reason'], 'no_lookup_yet')


WORKFLOWS = SCRIPTS.parent / '.github' / 'workflows'


class WorkflowTests(unittest.TestCase):
    """Plain-text checks: no yaml module, no workflow is run."""

    def test_both_build_jobs_copy_the_hook_and_claude_start(self):
        text = (WORKFLOWS / 'build.yml').read_text(encoding='utf-8')
        self.assertEqual(text.count('cp hooks/providers/claude_session_start.py "dist/$package/hooks/"'), 2)
        self.assertEqual(text.count('cp scripts/claude-start "dist/$package/scripts/"'), 2)

    def test_both_test_workflows_run_the_hook_test(self):
        for name in ('linux-tests.yml', 'macos-checks.yml'):
            text = (WORKFLOWS / name).read_text(encoding='utf-8')
            self.assertIn('          python3 -m unittest hooks.providers.test_session_start\n', text, name)


if __name__ == '__main__':
    unittest.main()
