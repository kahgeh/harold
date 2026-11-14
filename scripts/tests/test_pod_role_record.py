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
