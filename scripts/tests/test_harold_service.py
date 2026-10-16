import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import harold_service as service  # noqa: E402

PID = 42


class FakeLaunchctl:
    """Stands in for every command harold_service runs: launchctl, lsof and the readiness
    check. It records each launchctl call and keeps the job's state the way launchd would.
    With HAROLD_TEST_EVIDENCE=1 it prints each launchctl call."""

    def __init__(self, label, loaded=False, running=False, disabled=False, ready=True,
                 disabled_spelling=('disabled', 'enabled'), bootout_fails=False, config=None):
        self.label = label
        self.bootout_fails = bootout_fails
        self.config = config
        self.loaded = loaded
        self.running = running
        self.disabled = disabled
        self.ready = ready
        self.disabled_spelling = disabled_spelling
        self.calls = []

    def __call__(self, args, **kwargs):
        args = [str(arg) for arg in args]
        if args[0] == 'launchctl':
            self.calls.append(args[1:])
            if os.environ.get('HAROLD_TEST_EVIDENCE') == '1':
                print('launchctl ' + ' '.join(args[1:]))
            return self.launchctl(args)
        if args[0] == 'lsof' and any(arg.startswith('-iTCP:') for arg in args):
            if self.running:
                return subprocess.CompletedProcess(args, 0, 'p{}\n'.format(PID), '')
            return subprocess.CompletedProcess(args, 1, '', '')
        if args[-1] == '--check-config' and self.config is not None:
            return subprocess.CompletedProcess(args, 0, json.dumps(self.config), '')
        if args[-1] == '--check-ready':
            return subprocess.CompletedProcess(
                args, 0, '{{"ready": {}}}'.format('true' if self.ready else 'false'), '')
        raise AssertionError('unexpected command: {}'.format(args))

    def launchctl(self, args):
        command = args[1]
        ok = subprocess.CompletedProcess(args, 0, '', '')
        if command == 'print':
            if not self.loaded:
                return subprocess.CompletedProcess(args, 113, '', 'Could not find service\n')
            return subprocess.CompletedProcess(
                args, 0, '\tpid = {}\n'.format(PID) if self.running else '\tstate = waiting\n', '')
        if command == 'print-disabled':
            on, off = self.disabled_spelling
            lines = ['disabled services = {',
                     '\t"com.apple.other" => {}'.format(on),
                     '\t"{}" => {}'.format(self.label, on if self.disabled else off), '}']
            return subprocess.CompletedProcess(args, 0, '\n'.join(lines) + '\n', '')
        if command == 'bootstrap':
            self.loaded = True
        elif command == 'kickstart':
            self.running = True
        elif command == 'bootout':
            if self.bootout_fails:
                return subprocess.CompletedProcess(args, 5, '', 'Boot-out failed: 5: Input/output error\n')
            self.loaded = self.running = False
        elif command == 'enable':
            self.disabled = False
        elif command == 'disable':
            self.disabled = True
        else:
            raise AssertionError('unexpected launchctl command: {}'.format(args))
        return ok

    def commands(self):
        return [call[0] for call in self.calls]


class ServiceControlTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name).resolve()
        self.bundle = root / 'bin/harold'
        (self.bundle / 'data').mkdir(parents=True)
        self.crashes = self.bundle / 'data/crashes'
        self.metadata = {'bundle': str(self.bundle), 'label': 'com.harold.agent.test',
                         'grpc_addr': '127.0.0.1:50060', 'plist': str(self.bundle / 'service.plist'),
                         'launch_agent': str(root / 'home/Library/LaunchAgents/com.harold.agent.test.plist'),
                         'env': {}}
        Path(self.metadata['launch_agent']).parent.mkdir(parents=True)
        Path(self.metadata['launch_agent']).write_text('plist')
        self.target = 'gui/{}/com.harold.agent.test'.format(os.getuid())
        for name, value in (('pid_alive', lambda pid: False), ('time', FakeTime())):
            patcher = patch.object(service, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def fake(self, **state):
        launchctl = FakeLaunchctl('com.harold.agent.test', **state)
        patcher = patch.object(service, 'run', launchctl)
        patcher.start()
        self.addCleanup(patcher.stop)
        return launchctl

    def test_stop_disables_then_boots_out(self):
        launchctl = self.fake(loaded=True, running=True)
        service.stop(self.metadata)
        self.assertIn(['disable', self.target], launchctl.calls)
        self.assertIn(['bootout', self.target], launchctl.calls)
        self.assertLess(launchctl.calls.index(['disable', self.target]),
                        launchctl.calls.index(['bootout', self.target]))
        self.assertTrue(launchctl.disabled)
        self.assertFalse(launchctl.loaded)

    def test_start_enables_clears_the_crash_file_and_bootstraps_the_launch_agent(self):
        launchctl = self.fake(disabled=True)
        self.crashes.write_text('gave-up\n1\n2\n3\n')
        self.assertEqual(service.start(self.metadata), PID)
        enable = ['enable', self.target]
        bootstrap = ['bootstrap', 'gui/{}'.format(os.getuid()), self.metadata['launch_agent']]
        kickstart = ['kickstart', self.target]
        for call in (enable, bootstrap, kickstart):
            self.assertIn(call, launchctl.calls)
        self.assertLess(launchctl.calls.index(enable), launchctl.calls.index(bootstrap))
        self.assertLess(launchctl.calls.index(bootstrap), launchctl.calls.index(kickstart))
        self.assertFalse(self.crashes.exists())

    def test_without_the_launch_agents_copy_start_loads_the_bundle_plist(self):
        # Deleting the copy turns off start at login; haroldctl start still works for this login.
        launchctl = self.fake()
        Path(self.metadata['launch_agent']).unlink()
        self.assertEqual(service.start(self.metadata), PID)
        self.assertIn(['bootstrap', 'gui/{}'.format(os.getuid()), self.metadata['plist']], launchctl.calls)

    def test_a_crash_file_that_is_not_text_does_not_stop_ensure(self):
        self.fake()
        self.crashes.write_bytes(b'\xff\xfe\x00')
        self.assertFalse(service.gave_up(self.metadata))
        self.assertEqual(service.ensure(self.metadata), PID)

    def test_ensure_starts_an_enabled_job(self):
        launchctl = self.fake()
        self.assertEqual(service.ensure(self.metadata), PID)
        self.assertIn('bootstrap', launchctl.commands())
        self.assertIn('kickstart', launchctl.commands())
        self.assertNotIn('enable', launchctl.commands())

    def test_ensure_leaves_a_stopped_job_off(self):
        launchctl = self.fake(disabled=True)
        with self.assertRaisesRegex(service.Off, 'stopped with haroldctl stop'):
            service.ensure(self.metadata)
        for command in ('bootstrap', 'kickstart', 'enable'):
            self.assertNotIn(command, launchctl.commands())

    def test_ensure_leaves_a_crash_loop_off(self):
        launchctl = self.fake()
        self.crashes.write_text('gave-up\n1\n2\n3\n')
        with self.assertRaisesRegex(service.Off, 'crash loop'):
            service.ensure(self.metadata)
        for command in ('bootstrap', 'kickstart', 'enable'):
            self.assertNotIn(command, launchctl.commands())
        self.assertEqual(self.crashes.read_text(), 'gave-up\n1\n2\n3\n')

    def test_a_crash_file_still_counting_lets_ensure_start(self):
        self.fake()
        self.crashes.write_text('running\n1\n2\n')
        self.assertEqual(service.ensure(self.metadata), PID)

    def test_older_macos_spelling_of_disabled_is_read(self):
        self.fake(disabled=True, disabled_spelling=('true', 'false'))
        self.assertTrue(service.disabled(self.metadata))
        self.fake(disabled=False, disabled_spelling=('true', 'false'))
        self.assertFalse(service.disabled(self.metadata))
        # Another label disabled says nothing about this one.
        self.fake(disabled=False)
        self.assertFalse(service.disabled(self.metadata))

    def test_a_failed_launch_unloads_but_leaves_the_job_enabled(self):
        launchctl = self.fake(ready=False)
        with self.assertRaisesRegex(RuntimeError, 'readiness'):
            service.start(self.metadata, timeout=0)
        self.assertIn('bootout', launchctl.commands())
        self.assertNotIn('disable', launchctl.commands())
        self.assertFalse(launchctl.disabled)

    def test_status_reports_autostart(self):
        self.fake()
        self.assertEqual(service.autostart(self.metadata), 'on')
        self.fake(disabled=True)
        self.assertEqual(service.autostart(self.metadata), 'off (stopped with haroldctl stop)')
        self.fake()
        self.crashes.write_text('gave-up\n')
        self.assertEqual(service.autostart(self.metadata), 'off (gave up after a crash loop)')


class CommandTests(unittest.TestCase):
    """haroldctl's commands through main(), from a copy of service.py in a temporary bundle."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name).resolve()
        self.bundle = root / 'bin/harold'
        (self.bundle / 'data').mkdir(parents=True)
        launch_agent = root / 'home/Library/LaunchAgents/com.harold.agent.test.plist'
        launch_agent.parent.mkdir(parents=True)
        launch_agent.write_text('plist')
        self.metadata = {'bundle': str(self.bundle), 'label': 'com.harold.agent.test',
                         'grpc_addr': '127.0.0.1:50060', 'plist': str(self.bundle / 'service.plist'),
                         'launch_agent': str(launch_agent), 'env': {}}
        (self.bundle / 'service.json').write_text(json.dumps(self.metadata))
        shutil.copyfile(service.__file__, self.bundle / 'service.py')
        spec = importlib.util.spec_from_file_location('installed_harold_service', self.bundle / 'service.py')
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)
        self.target = 'gui/{}/com.harold.agent.test'.format(os.getuid())
        for name, value in (('pid_alive', lambda pid: False), ('time', FakeTime())):
            patcher = patch.object(self.module, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def haroldctl(self, command, **state):
        """Runs `haroldctl <command>`; returns the fake launchctl, the exit code and stdout."""
        launchctl = FakeLaunchctl('com.harold.agent.test', config={
            'grpc_addr': '127.0.0.1:50060', 'store_path': str(self.bundle / 'data/store')}, **state)
        printed = io.StringIO()
        # main() replaces the environment with the managed one; patch.dict puts it back.
        with patch.object(self.module, 'run', launchctl), \
             patch.object(sys, 'argv', ['service.py', command]), \
             patch.dict(os.environ), \
             contextlib.redirect_stdout(printed), \
             contextlib.redirect_stderr(io.StringIO()):
            code = self.module.main()
        return launchctl, code, printed.getvalue()

    def test_ensure_is_a_command(self):
        launchctl, code, printed = self.haroldctl('ensure')
        self.assertEqual(code, 0)
        self.assertEqual(printed, 'Harold ready (PID {})\n'.format(PID))
        self.assertIn('kickstart', launchctl.commands())
        self.assertNotIn('enable', launchctl.commands())

    def test_ensure_leaves_a_stopped_harold_off_and_exits_one(self):
        launchctl, code, printed = self.haroldctl('ensure', disabled=True)
        self.assertEqual(code, 1)
        self.assertNotIn('kickstart', launchctl.commands())

    def test_status_prints_autostart(self):
        _, code, printed = self.haroldctl('status', loaded=True, running=True, disabled=True)
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(printed), {'ready': True, 'pid': PID, 'grpc_addr': '127.0.0.1:50060',
                                               'autostart': 'off (stopped with haroldctl stop)'})

    def test_a_restart_whose_bootout_fails_leaves_the_job_enabled(self):
        launchctl, code, _ = self.haroldctl('restart', loaded=True, running=True, bootout_fails=True)
        self.assertEqual(code, 1)
        self.assertNotIn('disable', launchctl.commands())
        self.assertFalse(launchctl.disabled)

    def test_restart_enables_and_starts_again(self):
        launchctl, code, printed = self.haroldctl('restart', loaded=True, running=True)
        self.assertEqual(code, 0)
        self.assertEqual(printed, 'Harold ready (PID {})\n'.format(PID))
        self.assertLess(launchctl.calls.index(['bootout', self.target]),
                        launchctl.calls.index(['enable', self.target]))
        self.assertFalse(launchctl.disabled)


class FakeTime:
    """time for harold_service: monotonic advances by a second each read, so deadlines pass
    without waiting, and sleep returns at once."""

    def __init__(self):
        self.now = 0.0

    def monotonic(self):
        self.now += 1.0
        return self.now

    def sleep(self, seconds):
        pass


if __name__ == '__main__':
    unittest.main()
