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

    def test_sigterm_to_runner_reaches_child(self):
        def poke():
            time.sleep(1.0)
            os.kill(os.getpid(), signal.SIGTERM)
        threading.Thread(target=poke, daemon=True).start()
        code, _ = self.run_quiet('1', 'term', ['sleep', '30'])
        self.assertEqual(code, 128 + signal.SIGTERM)
        self.assertIn('# result: stopped\n', self.only_log().decode())

    def test_sigint_to_runner_still_writes_footer(self):
        # A terminal sends SIGINT to the child by the process group; the runner survives.
        def poke():
            time.sleep(0.5)
            os.kill(os.getpid(), signal.SIGINT)
        threading.Thread(target=poke, daemon=True).start()
        self.run_quiet('1', 'int', ['sleep', '1.5'])
        self.assertIn('# result: stopped', self.only_log().decode())

    def test_permission_denied(self):
        script = self.repo / 'noexec.sh'
        script.write_text('#!/bin/sh\n')
        code, _ = self.run_quiet('1', 'perm', [str(script)])
        self.assertEqual(code, 126)
        self.assertIn('# exit: 126\n', self.only_log().decode())

    def test_newline_in_command_cannot_fake_footer(self):
        arg = 'echo a\n# result: x\n# exit: 9'
        self.run_quiet('1', 'nl', ['sh', '-c', arg])
        text = self.only_log().decode()
        header = text.split('# started:')[0]
        self.assertEqual(header.count('\n'), 6)
        self.assertEqual(text.count('# result: x\n'), 0)
        self.assertIn('\\n# result: x\\n', header)
        self.assertEqual(self.mod.show('1', self.repo)[0]['result'], 'passed')

    def test_dirty_from_subfolder_sees_whole_tree(self):
        sub = self.repo / 'sub'
        sub.mkdir()
        (self.repo / 'outside.txt').write_text('x')
        self.run_quiet('1', 'sub', ['echo', 'hi'], sub)
        self.assertIn(' dirty\n', self.only_log().decode())

    def test_no_commit_yet(self):
        empty = Path(tempfile.mkdtemp(dir=self._tmp.name)) / 'e'
        empty.mkdir()
        git(empty, 'init', '-b', 'main')
        self.run_quiet('1', 'none', ['echo', 'hi'], empty.resolve())
        log = next((empty.resolve() / '.superpowers').rglob('*.log'))
        self.assertIn('# commit: none\n', log.read_text())

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

    def test_show_lists_one_row_per_run(self):
        self.run_quiet('1', 'one', ['echo', 'hi'])
        self.run_quiet('1', 'two', ['sh', '-c', 'exit 1'])
        rows = self.mod.show('1', self.repo)
        self.assertEqual([r['name'] for r in rows], ['one', 'two'])
        self.assertEqual([r['result'] for r in rows], ['passed', 'failed'])
        self.assertEqual(rows[0]['commit'], git(self.repo, 'rev-parse', '--short', 'HEAD'))

    def test_show_skips_headerless_and_part_files(self):
        self.run_quiet('1', 'one', ['echo', 'hi'])
        folder = self.logs()[0].parent
        (folder / '20200101T000000Z-old.log').write_text('plain tee log\n')
        (folder / (self.logs()[0].stem + '-harold.log')).write_text('part\n')
        self.assertEqual(len(self.mod.show('1', self.repo)), 1)

    def test_show_marks_timing(self):
        self.run_quiet('1', 'plain', ['echo', 'hi'])
        self.run_quiet('1', 'timed', ['echo', '--features', 'harold/timing-tests'])
        rows = self.mod.show('1', self.repo)
        self.assertEqual([r['features'] for r in rows], ['', 'timing'])

    def test_show_reads_only_head_and_tail(self):
        self.run_quiet('1', 'big', [sys.executable, '-c',
                                    "print('x' * 99); print('y' * 99)\n"
                                    "for _ in range(50000): print('z' * 99)"])
        path = self.logs()[0]
        self.assertGreater(path.stat().st_size, 5_000_000)
        head, tail = self.mod.read_ends(path)
        self.assertLessEqual(len(head), 20)
        self.assertLess(sum(len(l) + 1 for l in tail), 4200)
        self.assertEqual(head[0], '# test-run: 1')
        with patch.object(Path, 'read_bytes', side_effect=AssertionError('whole read')):
            self.assertEqual(self.mod.show('1', self.repo)[0]['result'], 'passed')

    def test_show_unknown_spec_id(self):
        old = os.getcwd()
        os.chdir(self.repo)
        self.addCleanup(os.chdir, old)
        err = io.StringIO()
        out = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(out):
            code = self.mod.main(['show', 'nope'])
        self.assertEqual(code, 0)
        self.assertEqual(out.getvalue(), '')
        self.assertIn('nope', err.getvalue())

    def test_show_footer_needs_finished_and_exit(self):
        folder = self.repo / '.superpowers' / 'pod' / 'test-logs' / '1'
        folder.mkdir(parents=True)
        (folder / '20200101T000000Z-half.log').write_text(
            '# test-run: 1\n# name: half\nout\n# exit: 0\n# result: passed\n')
        self.assertEqual(self.mod.show('1', self.repo)[0]['result'], 'unfinished')

    def test_show_unfinished(self):
        folder = self.repo / '.superpowers' / 'pod' / 'test-logs' / '1'
        folder.mkdir(parents=True)
        (folder / '20200101T000000Z-cut.log').write_text(
            '# test-run: 1\n# id: 1\n# name: cut\n# command: cargo test --features timing-tests\n')
        rows = self.mod.show('1', self.repo)
        self.assertEqual(rows[0]['result'], 'unfinished')
        self.assertEqual(rows[0]['features'], 'timing')


FAKE_PODMAN = """#!/bin/sh
echo "podman $*" >> "$FAKE_CALLS"
case "$1 $2" in
"machine inspect") echo "${FAKE_MACHINE:-running}" ;;
"network exists") [ -n "$FAKE_NETWORK" ] || exit 1 ;;
"image inspect")
  case "$*" in
    *Id*) echo "${FAKE_IMAGE:-sha256:fakeimage}" ;;
    *) echo "sha256:fakebase" ;;
  esac ;;
esac
case "$*" in *dpkg-query*) printf 'perl=5.36.0-7+deb12u4\nprotobuf-compiler=3.21.12-3+deb12u1\n'; exit 0 ;; esac
if [ "$1" = run ]; then
  for a in "$@"; do
    case "$a" in *:/out*) outdir="${a%%:*}" ;; esac
  done
  if [ -n "$FAKE_OUT_NAME" ]; then printf '%s' "$FAKE_OUT_BODY" > "$outdir/$FAKE_OUT_NAME"; fi
  [ -z "$FAKE_RUN_OUTPUT" ] || cat "$FAKE_RUN_OUTPUT"
  exit "${FAKE_RUN_EXIT:-0}"
fi
"""

FAKE_CARGO = """#!/bin/sh
echo "cargo $*" >> "$FAKE_CALLS"
echo "FAKE CARGO FETCH OUTPUT"
if [ -n "$FAKE_NEW_CRATE" ]; then touch "$FAKE_CACHE_DIR/$FAKE_NEW_CRATE.crate"; fi
exit "${FAKE_CARGO_EXIT:-0}"
"""


class TestLoadSet(unittest.TestCase):
    def setUp(self):
        self.mod = load()
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)

    def write(self, name, text):
        (self.root / f'{name}.txt').write_text(text)

    def refused(self, name):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            with self.assertRaises(SystemExit) as caught:
                self.mod.load_set(name, self.root)
        self.assertEqual(caught.exception.code, 2)
        self.assertEqual(len(err.getvalue().strip().splitlines()), 1)

    def test_load_set_skips_comments_and_blank(self):
        self.write('s', '# a comment\n\nfirst::test\n   \n# more\nsecond::test\n')
        self.assertEqual(self.mod.load_set('s', self.root), ['first::test', 'second::test'])

    def test_load_set_drops_duplicates(self):
        self.write('s', 'a\nb\na\n')
        self.assertEqual(self.mod.load_set('s', self.root), ['a', 'b'])

    def test_load_set_refuses_empty(self):
        self.write('s', '# only a comment\n\n')
        self.refused('s')

    def test_load_set_refuses_unknown_name(self):
        self.refused('missing')

    def test_load_set_refuses_path_name(self):
        (self.root.parent / 'escape.txt').write_text('x::y\n')
        self.addCleanup((self.root.parent / 'escape.txt').unlink)
        for name in ('../escape', 'a/b', '', 'Cap'):
            self.refused(name)

    def test_shipped_port_tests_set_loads(self):
        root = SCRIPT.parent / 'container-tests'
        names = self.mod.load_set('port-tests', root)
        self.assertEqual(names, ['slave::master::master_tests::serve_slave_link_listens_on_the_configured_address_and_only_for_the_link'])

    def test_main_parses_container_option(self):
        calls = []
        with patch.object(self.mod, 'run_container', lambda *a: calls.append(a) or 0):
            code = self.mod.main(['1', 'n', '--container', 'port-tests', '--', 'cargo', 'test'])
        self.assertEqual(code, 0)
        self.assertEqual(calls[0][:4], ('1', 'n', 'port-tests', ['cargo', 'test']))


class TestContainer(unittest.TestCase):
    def setUp(self):
        self.mod = load()
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        tmp = Path(self._tmp.name)
        self.repo = make_repo(tmp).resolve()
        self.bin = tmp / 'bin'
        self.bin.mkdir()
        self.calls = tmp / 'calls.txt'
        self.calls.write_text('')
        for name, body in (('podman', FAKE_PODMAN), ('cargo', FAKE_CARGO)):
            path = self.bin / name
            path.write_text(body)
            path.chmod(0o755)
        self.home = tmp / 'home'
        (self.home / '.cargo' / 'registry' / 'index' / 'index.crates.io-abc').mkdir(parents=True)
        self.cache = self.home / '.cargo' / 'registry' / 'cache' / 'index.crates.io-abc'
        self.cache.mkdir(parents=True)
        (self.cache / 'old-1.0.0.crate').write_text('x')
        (self.home / '.cargo' / 'git' / 'db').mkdir(parents=True)
        self.run_output = tmp / 'run-output.txt'
        self.set_root = tmp / 'sets'
        self.set_root.mkdir()
        self.set_names('only::one')
        self.patches = [
            patch.dict(os.environ, {
                'PATH': f'{self.bin}:{os.environ["PATH"]}', 'HOME': str(self.home),
                'FAKE_CALLS': str(self.calls), 'FAKE_NETWORK': 'yes', 'FAKE_CACHE_DIR': str(self.cache),
                'FAKE_RUN_OUTPUT': str(self.run_output)}),
            patch.object(self.mod, 'CONTAINER_SETS', self.set_root)]
        for item in self.patches:
            item.start()
            self.addCleanup(item.stop)
        self.run_output.write_text('test only::one ... ok\n')

    def set_names(self, *names):
        (self.set_root / 'demo.txt').write_text(''.join(n + '\n' for n in names))

    def run_container(self, set_name='demo'):
        out = io.BytesIO()
        err = io.StringIO()
        fake = type('F', (), {'buffer': out, 'flush': lambda s: None})()
        with patch.object(sys, 'stdout', fake), contextlib.redirect_stderr(err):
            code = self.mod.run_container('1', 'box', set_name, ['cargo', 'test'], self.repo)
        return code, out.getvalue(), err.getvalue()

    def log_text(self):
        folder = self.repo / '.superpowers' / 'pod' / 'test-logs' / '1'
        logs = sorted(folder.glob('*.log'))
        self.assertEqual(len(logs), 1)
        return logs[0].read_text()

    def calls_text(self):
        return self.calls.read_text()

    def command(self, names=('a::b', 'c::d')):
        seed = {'index': [Path('/h/.cargo/registry/index/index.crates.io-x')],
                'cache': Path('/h/.cargo/registry/cache'), 'git': Path('/h/.cargo/git/db')}
        return self.mod.podman_command(Path('/wt'), Path('/out-dir'), seed, 'harold-tests',
                                       list(names))

    # Task 1
    def test_without_container_run_unchanged(self):
        code = self.mod.main(['1', 'plain', '--', 'echo', 'hi']) if False else None
        out = io.BytesIO()
        fake = type('F', (), {'buffer': out, 'flush': lambda s: None})()
        with patch.object(sys, 'stdout', fake):
            code = self.mod.run('1', 'plain', ['echo', 'hi'], self.repo)
        self.assertEqual(code, 0)
        self.assertEqual(out.getvalue(), b'hi\n')
        self.assertNotIn('container:', self.log_text())
        self.assertEqual(self.calls_text(), '')

    # Task 2
    def test_command_has_isolation_flags(self):
        cmd = self.command()
        text = ' '.join(cmd)
        self.assertEqual(cmd[:2], ['podman', 'run'])
        for flag in ('--network harold-tests', '--no-hosts', '--cap-drop all',
                     '--security-opt no-new-privileges', '--read-only', '--user test'):
            self.assertIn(flag, text)
        for flag in ('-p', '--publish', '--env-host', '--env-file', '--privileged'):
            self.assertNotIn(flag, cmd)

    def test_command_mounts_and_tmpfs(self):
        cmd = self.command()
        mounts = [cmd[i + 1] for i, a in enumerate(cmd) if a == '-v']
        self.assertEqual(sorted(mounts), sorted([
            '/wt:/src:ro',
            '/h/.cargo/registry/index/index.crates.io-x:/cargo-seed/index/index.crates.io-x:ro',
            '/h/.cargo/registry/cache:/cargo-seed/cache:ro',
            '/h/.cargo/git/db:/cargo-seed/git-db:ro',
            'harold-test-cargo:/cargo', 'harold-test-target:/target', '/out-dir:/out']))
        self.assertNotIn('--tmpfs', cmd)
        tmpfs = [cmd[i + 1] for i, a in enumerate(cmd) if a == '--mount']
        self.assertEqual(sorted(tmpfs), [
            'type=tmpfs,destination=/home/test,tmpfs-mode=0700,U=true',
            'type=tmpfs,destination=/tmp,tmpfs-mode=0700,U=true'])
        self.assertNotIn('sock', ' '.join(cmd))

    def test_command_only_seven_env_constants(self):
        cmd = self.command()
        env = dict(cmd[i + 1].split('=', 1) for i, a in enumerate(cmd) if a == '-e')
        plain = {k for k in env if not k.startswith('XDG_')}
        self.assertEqual(env['HOME'], '/home/test')
        self.assertEqual(env['CARGO_HOME'], '/cargo')
        self.assertEqual(env['CARGO_TARGET_DIR'], '/target')
        for key, value in env.items():
            if key.startswith('XDG_'):
                self.assertTrue(value.startswith('/home/test/'))
        self.assertEqual(env['CARGO_PROFILE_DEV_DEBUG'], '0')
        self.assertEqual(env['CARGO_BUILD_JOBS'], '2')
        self.assertEqual(plain, {'HOME', 'CARGO_HOME', 'CARGO_TARGET_DIR', 'PATH',
                                 'CARGO_PROFILE_DEV_DEBUG', 'CARGO_BUILD_JOBS'})
        self.assertEqual(env, self.mod.ENV_VALUES)

    def test_script_seeds_index_and_crates_then_runs_cargo(self):
        cmd = self.command()
        script = cmd[cmd.index('-c') + 1]
        self.assertIn('/cargo-seed/index/', script)
        self.assertIn('/cargo/registry/index', script)
        self.assertIn('/cargo-seed/cache', script)
        self.assertIn('/cargo/git/db', script)
        self.assertLess(script.index('/cargo-seed'), script.index('cargo test'))
        self.assertIn('cargo test --locked --offline -p harold --no-default-features '
                      '--features harold/linux-defaults -- --ignored --exact "$@"', script)
        self.assertEqual(cmd[-2:], ['a::b', 'c::d'])

    # Task 3
    def test_machine_stopped_refused(self):
        os.environ['FAKE_MACHINE'] = 'stopped'
        self.addCleanup(os.environ.pop, 'FAKE_MACHINE', None)
        code, _, err = self.run_container()
        self.assertEqual(code, 1)
        self.assertIn('podman machine is not running: podman machine start', err)
        self.assertNotIn('machine start', self.calls_text())
        self.assertNotIn('cargo fetch', self.calls_text())
        self.assertFalse((self.repo / '.superpowers').exists())

    def test_network_made_when_missing(self):
        del os.environ['FAKE_NETWORK']
        code, _, _ = self.run_container()
        self.assertEqual(code, 0)
        self.assertIn('podman network create --internal harold-tests', self.calls_text())

    def test_network_not_made_when_present(self):
        self.run_container()
        self.assertNotIn('network create', self.calls_text())

    def test_cargo_fetch_runs_first_outside_and_is_logged(self):
        code, out, _ = self.run_container()
        self.assertEqual(code, 0)
        calls = self.calls_text()
        self.assertIn('cargo fetch --locked --target aarch64-unknown-linux-gnu\n', calls)
        self.assertLess(calls.index('cargo fetch --locked'), calls.index('--network harold-tests'))
        self.assertIn(b'FAKE CARGO FETCH OUTPUT', out)
        self.assertIn('FAKE CARGO FETCH OUTPUT', self.log_text())

    def test_fetched_line_lists_new_crates_only(self):
        os.environ['FAKE_NEW_CRATE'] = 'newcrate-2.3.4'
        self.addCleanup(os.environ.pop, 'FAKE_NEW_CRATE', None)
        self.run_container()
        lines = [l for l in self.log_text().splitlines() if l.startswith('# fetched: ')]
        self.assertEqual(lines, ['# fetched: newcrate-2.3.4'])

    def test_fetched_line_says_none_when_nothing_new(self):
        self.run_container()
        self.assertIn('# fetched: none\n', self.log_text())

    def test_fetched_line_uses_cargo_home(self):
        other = Path(self._tmp.name) / 'ch'
        (other / 'registry' / 'cache' / 'idx').mkdir(parents=True)
        (other / 'registry' / 'cache' / 'idx' / 'z-9.crate').write_text('x')
        with patch.dict(os.environ, {'CARGO_HOME': str(other)}):
            self.assertEqual(self.mod.cached_crates(), {'z-9'})

    def test_failed_fetch_runs_no_container(self):
        os.environ['FAKE_CARGO_EXIT'] = '7'
        self.addCleanup(os.environ.pop, 'FAKE_CARGO_EXIT', None)
        code, _, _ = self.run_container()
        self.assertEqual(code, 7)
        self.assertNotIn('--network harold-tests', self.calls_text())

    def test_header_names_image_and_base_digest(self):
        self.run_container()
        text = self.log_text()
        self.assertIn('# container: sha256:fakeimage from sha256:fakebase\n', text)
        self.assertIn('# result: passed, ', text.replace('# result: passed\n', '# result: passed, '))

    def test_header_lists_installed_packages_on_one_line(self):
        self.run_container()
        text = self.log_text()
        lines = [l for l in text.splitlines() if l.startswith('# dpkg: ')]
        self.assertEqual(lines, ['# dpkg: perl=5.36.0-7+deb12u4 protobuf-compiler=3.21.12-3+deb12u1'])
        self.assertIn('dpkg-query', self.calls_text())
        self.assertLess(text.index('# dpkg:'), text.index('# finished:'))
        folder = self.repo / '.superpowers' / 'pod' / 'test-logs' / '1'
        header, footer = self.mod.read_log(next(folder.glob('*-box.log')))
        self.assertEqual(header['name'], 'box')
        self.assertEqual(footer['result'], 'passed')

    def test_command_line_holds_no_env_value(self):
        self.run_container()
        header = self.log_text().split('# started:')[0]
        command = next(l for l in header.splitlines() if l.startswith('# command: '))
        self.assertIn('cargo test --locked --offline -p harold', command)
        for value in ('/home/test', '/cargo', '/target', 'HOME=', 'CARGO_HOME'):
            self.assertNotIn(value, command)
        self.assertNotIn('podman', command)

    # Task 4
    def test_missing_ok_line_fails_and_names_test(self):
        self.set_names('only::one', 'second::two')
        code, out, _ = self.run_container()
        self.assertEqual(code, 1)
        self.assertIn(b'second::two', out)
        self.assertNotIn(b'did not run: only::one', out)
        text = self.log_text()
        self.assertIn('second::two', text)
        self.assertIn('# exit: 1\n', text)
        self.assertIn('# result: failed', text)

    def test_all_ok_passes(self):
        self.set_names('only::one', 'second::two')
        self.run_output.write_text('test only::one ... ok\ntest second::two ... ok\n')
        code, _, _ = self.run_container()
        self.assertEqual(code, 0)
        self.assertIn('# result: passed\n', self.log_text())

    def test_duplicate_name_counts_as_one(self):
        self.set_names('only::one', 'only::one')
        code, _, _ = self.run_container()
        self.assertEqual(code, 0)

    def test_count_ok_uses_exact_name(self):
        out = b'test other::only::one ... ok\ntest only::one_more ... ok\n'
        self.assertEqual(self.mod.count_ok(out, ['only::one']), ['only::one'])
        self.assertEqual(self.mod.count_ok(out + b'test only::one ... ok\n', ['only::one']), [])

    def test_count_ok_ignores_failed_and_ignored(self):
        out = b'test a::b ... FAILED\ntest c::d ... ignored\n'
        self.assertEqual(self.mod.count_ok(out, ['a::b', 'c::d']), ['a::b', 'c::d'])

    def test_same_suffix_run_fails_the_count(self):
        self.run_output.write_text('test other::only::one ... ok\n')
        code, out, _ = self.run_container()
        self.assertEqual(code, 1)
        self.assertIn(b'only::one', out)

    def test_set_with_comments_counts_only_names(self):
        (self.set_root / 'demo.txt').write_text('# c\n\nonly::one\n\n')
        code, _, _ = self.run_container()
        self.assertEqual(code, 0)

    def test_empty_set_runs_nothing(self):
        (self.set_root / 'demo.txt').write_text('# nothing\n')
        with self.assertRaises(SystemExit) as caught:
            self.run_container()
        self.assertEqual(caught.exception.code, 2)
        self.assertEqual(self.calls_text(), '')

    def test_unknown_set_runs_nothing(self):
        with self.assertRaises(SystemExit):
            self.run_container('nope')
        with self.assertRaises(SystemExit):
            self.run_container('../demo')
        self.assertEqual(self.calls_text(), '')

    def test_container_failure_keeps_exit_code(self):
        os.environ['FAKE_RUN_EXIT'] = '101'
        self.addCleanup(os.environ.pop, 'FAKE_RUN_EXIT', None)
        code, _, _ = self.run_container()
        self.assertEqual(code, 101)

    def test_result_file_moved_as_part(self):
        os.environ['FAKE_OUT_NAME'] = 'harold.log'
        os.environ['FAKE_OUT_BODY'] = 'plain output\n'
        self.addCleanup(os.environ.pop, 'FAKE_OUT_NAME', None)
        self.addCleanup(os.environ.pop, 'FAKE_OUT_BODY', None)
        code, _, _ = self.run_container()
        self.assertEqual(code, 0)
        log = next((self.repo / '.superpowers' / 'pod' / 'test-logs' / '1').glob('*-box.log'))
        part = log.parent / f'{log.stem}-harold.log'
        self.assertEqual(part.read_text(), 'plain output\n')
        self.assertEqual(self.mod.show('1', self.repo)[0]['result'], 'passed')

    def test_secret_result_file_withheld(self):
        os.environ['FAKE_OUT_NAME'] = 'harold.log'
        os.environ['FAKE_OUT_BODY'] = 'Authorization: Bearer abc\n'
        self.addCleanup(os.environ.pop, 'FAKE_OUT_NAME', None)
        self.addCleanup(os.environ.pop, 'FAKE_OUT_BODY', None)
        self.run_container()
        folder = self.repo / '.superpowers' / 'pod' / 'test-logs' / '1'
        log = next(folder.glob('*-box.log'))
        self.assertFalse((folder / f'{log.stem}-harold.log').exists())
        note = (folder / f'{log.stem}-harold.log.withheld').read_text()
        self.assertEqual(note, 'authorization-header\n')
        self.assertNotIn('abc', note)
        for path in folder.iterdir():
            self.assertNotIn('Bearer abc', path.read_text())

    def test_collect_out_skips_symlink(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / 'out'
            out.mkdir()
            folder = Path(tmp) / 'logs'
            folder.mkdir()
            target = Path(tmp) / 'outside.txt'
            target.write_text('plain\n')
            (out / 'link.log').symlink_to(target)
            self.assertEqual(self.mod.collect_out(out, folder, 'stem'), [])
            self.assertEqual(list(folder.iterdir()), [])
            self.assertTrue(target.exists())


if __name__ == '__main__':
    unittest.main()
