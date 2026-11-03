import contextlib
from datetime import datetime, timezone
import importlib.machinery
import importlib.util
import io
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / 'test-run'


def load():
    loader = importlib.machinery.SourceFileLoader('test_run_script', str(SCRIPT))
    spec = importlib.util.spec_from_loader('test_run_script', loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def git(cwd, *args):
    return subprocess.run(['git', *args], cwd=cwd, check=True, capture_output=True,
                          text=True).stdout.strip()


def make_repo(root, branch='main'):
    repo = Path(root) / 'repo'
    repo.mkdir()
    git(repo, 'init', '-b', branch)
    git(repo, 'config', 'user.email', 't@example.com')
    git(repo, 'config', 'user.name', 't')
    (repo / 'a.txt').write_text('a\n')
    git(repo, 'add', 'a.txt')
    git(repo, 'commit', '-m', 'first')
    return repo


class TestRun(unittest.TestCase):
    def setUp(self):
        self.mod = load()
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.repo = make_repo(self._tmp.name).resolve()

    def run_quiet(self, spec_id, name, command, cwd=None):
        out = io.BytesIO()
        fake = type('F', (), {'buffer': out, 'flush': lambda s: None})()
        with patch.object(sys, 'stdout', fake):
            code = self.mod.run(spec_id, name, command, cwd or self.repo)
        return code, out.getvalue()

    def logs(self, spec_id='1'):
        folder = self.repo / '.superpowers' / 'pod' / 'test-logs' / spec_id
        return sorted(folder.glob('*.log')) if folder.exists() else []

    def only_log(self, spec_id='1'):
        logs = self.logs(spec_id)
        self.assertEqual(len(logs), 1)
        return logs[0].read_bytes()

    def test_pass_writes_header_and_footer(self):
        code, out = self.run_quiet('1', 'echo', ['echo', 'hi'])
        self.assertEqual(code, 0)
        self.assertEqual(out, b'hi\n')
        text = self.only_log().decode()
        self.assertTrue(text.startswith('# test-run: 1\n'))
        self.assertIn('# commit: ' + git(self.repo, 'rev-parse', '--short', 'HEAD') + '\n', text)
        self.assertIn('# exit: 0\n', text)
        self.assertIn('# result: passed\n', text)
        keys = [l.split(':')[0] for l in text.splitlines() if l.startswith('# ')]
        self.assertEqual(keys, ['# test-run', '# id', '# name', '# command', '# cwd',
                                '# commit', '# started', '# finished', '# duration_s',
                                '# exit', '# result'])

    def test_failure_keeps_exit_code(self):
        code, _ = self.run_quiet('1', 'f', ['sh', '-c', 'exit 3'])
        self.assertEqual(code, 3)
        text = self.only_log().decode()
        self.assertIn('# exit: 3\n', text)
        self.assertIn('# result: failed\n', text)

    def test_signal_is_stopped(self):
        code, _ = self.run_quiet('1', 's', ['sh', '-c', 'kill -TERM $$'])
        self.assertEqual(code, 128 + signal.SIGTERM)
        text = self.only_log().decode()
        self.assertIn('# exit: 143\n', text)
        self.assertIn('# result: stopped\n', text)

    def test_signal_to_runner_reaches_child(self):
        def poke():
            time.sleep(1.0)
            os.kill(os.getpid(), signal.SIGINT)
        threading.Thread(target=poke, daemon=True).start()
        code, _ = self.run_quiet('1', 'int', ['sleep', '30'])
        self.assertEqual(code, 128 + signal.SIGINT)
        self.assertIn('# result: stopped\n', self.only_log().decode())

    def test_missing_command(self):
        code, _ = self.run_quiet('1', 'm', ['no-such-command-xyz'])
        self.assertEqual(code, 127)
        text = self.only_log().decode()
        self.assertIn('# exit: 127\n', text)
        self.assertIn('# result: failed\n', text)

    def test_three_test_result_lines_are_summed(self):
        script = ("print('test result: ok. 5 passed; 0 failed; 2 ignored; 0 measured')\n"
                  "print('test result: FAILED. 4 passed; 1 failed; 1 ignored; 0 measured')\n"
                  "print('test result: ok. 6 passed; 0 failed; 1 ignored; 0 measured')\n"
                  "raise SystemExit(101)\n")
        code, _ = self.run_quiet('1', 'sum', [sys.executable, '-c', script])
        self.assertEqual(code, 101)
        self.assertIn('# result: failed, 15 passed, 1 failed, 4 ignored\n',
                      self.only_log().decode())

    def test_sum_results(self):
        self.assertIsNone(self.mod.sum_results('nothing here\n'))
        self.assertEqual(self.mod.sum_results('test result: ok. 2 passed; 0 failed; 1 ignored;\n'),
                         '2 passed, 0 failed, 1 ignored')

    def test_bad_id_and_name_refused(self):
        for spec_id, name in (('Bad Id', 'ok'), ('1', 'a/b'), ('1', 'Cap'), ('', 'x')):
            code, _ = self.run_quiet(spec_id, name, ['echo', 'hi'])
            self.assertNotEqual(code, 0)
        self.assertFalse((self.repo / '.superpowers').exists())

    def test_no_base_checkout_refuses(self):
        other = make_repo(tempfile.mkdtemp(dir=self._tmp.name), 'other').resolve()
        code, out = self.run_quiet('1', 'x', ['echo', 'hi'], other)
        self.assertNotEqual(code, 0)
        self.assertEqual(out, b'')
        self.assertFalse((other / '.superpowers').exists())

    def test_worker_worktree_writes_to_base(self):
        worker = Path(self._tmp.name) / 'worker'
        git(self.repo, 'worktree', 'add', '-b', 'work', str(worker))
        code, _ = self.run_quiet('1', 'w', ['echo', 'hi'], worker.resolve())
        self.assertEqual(code, 0)
        self.assertEqual(len(self.logs()), 1)
        self.assertFalse((worker / '.superpowers').exists())

    def test_dirty_tree(self):
        (self.repo / 'new.txt').write_text('x')
        self.run_quiet('1', 'd', ['echo', 'hi'])
        self.assertIn(' dirty\n', self.only_log().decode())

    def test_clean_tree_ignores_earlier_logs(self):
        self.run_quiet('1', 'one', ['echo', 'hi'])
        self.run_quiet('1', 'two', ['echo', 'hi'])
        for log in self.logs():
            self.assertNotIn('dirty', log.read_text())

    def test_header_holds_arguments_no_environment(self):
        with patch.dict(os.environ, {'SECRET_X': 'zzz9'}):
            _, out = self.run_quiet('1', 'env', ['sh', '-c', 'printf %s "$SECRET_X"'])
        self.assertEqual(out, b'zzz9')
        text = self.only_log().decode()
        self.assertIn('printf %s "$SECRET_X"', text)
        for line in text.splitlines():
            if line.startswith('# '):
                self.assertNotIn('zzz9', line)

    def test_env_for_command(self):
        _, out = self.run_quiet('1', 'dir', ['sh', '-c',
                                             'echo "$TEST_RUN_DIR"; echo "$TEST_RUN_STEM"'])
        log = self.logs()[0]
        lines = out.decode().splitlines()
        self.assertEqual(Path(lines[0]), log.parent)
        self.assertEqual(lines[1], log.stem)

    def test_same_second_gets_suffix(self):
        now = datetime(2026, 11, 3, 1, 2, 3, tzinfo=timezone.utc)
        base = self.repo
        first = self.mod.log_path(base, '1', 'n', now)
        self.assertTrue(first.name == '20261103T010203Z-n.log')
        first.parent.mkdir(parents=True)
        first.write_text('x')
        second = self.mod.log_path(base, '1', 'n', now)
        self.assertTrue(second.name.endswith('-n-2.log'))
        second.write_text('x')
        self.assertTrue(self.mod.log_path(base, '1', 'n', now).name.endswith('-n-3.log'))

    def test_no_final_newline_and_binary(self):
        _, out = self.run_quiet('1', 'bin', ['printf', 'a\\377'])
        self.assertEqual(out, b'a\xff')
        data = self.only_log()
        self.assertIn(b'a\xff\n# finished: ', data)

    def test_empty_output_footer_after_header(self):
        self.run_quiet('1', 'quiet', ['true'])
        text = self.only_log().decode()
        self.assertIn('# started: ', text)
        self.assertIn('\n# finished: ', text)

    def test_show_rows(self):
        self.run_quiet('1', 'one', ['echo', 'hi'])
        folder = self.logs()[0].parent
        (folder / '20200101T000000Z-old.log').write_text('plain tee log\n')
        (folder / '20200101T000001Z-x-harold.log').write_text('part\n')
        rows = self.mod.show('1', self.repo)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]['name'], 'one')
        self.assertEqual(rows[0]['result'], 'passed')

    def test_show_unfinished(self):
        folder = self.repo / '.superpowers' / 'pod' / 'test-logs' / '1'
        folder.mkdir(parents=True)
        (folder / '20200101T000000Z-cut.log').write_text(
            '# test-run: 1\n# id: 1\n# name: cut\n# command: cargo test --features timing-tests\n')
        rows = self.mod.show('1', self.repo)
        self.assertEqual(rows[0]['result'], 'unfinished')
        self.assertEqual(rows[0]['features'], 'timing')


if __name__ == '__main__':
    unittest.main()
