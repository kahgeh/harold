import contextlib
import hashlib
import io
import json
import os
import re
import shutil
import shlex
import argparse
import plistlib
from pathlib import Path
import sys
import tempfile
import unittest
import subprocess
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

RUNTIME_FILES = ('.claude-plugin/plugin.json', 'hooks/hooks.json',
                 'hooks/register.ts', 'hooks/accept.ts')


SKILL_FILE = 'skills/send-message-to-away-device/SKILL.md'
SKILL_SOURCE = 'Run {{HAROLD_BIN}} send-message-to-away-device. Again: {{HAROLD_BIN}}\n'


def artifact_output(repo, directory):
    return '\n'.join(json.dumps({
        'reason': 'compiler-artifact',
        'manifest_path': str(repo / name / 'Cargo.toml'),
        'target': {'name': name, 'kind': ['bin']},
        'profile': {'test': False},
        'executable': str(directory / name),
    }) for name in ('harold', 'control-tower'))


class InstallTests(unittest.TestCase):
    def setUp(self):
        import install
        self.install = install
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.prefix = Path(self.temp.name).resolve() / 'prefix with spaces'
        self.prefix.mkdir()
        # Never the real ~/.claude/settings.json.
        self.claude_settings = Path(self.temp.name).resolve() / 'claude/settings.json'
        self.claude_settings.parent.mkdir()
        self.claude_settings.write_text('{}')
        patcher = patch.object(install, 'CLAUDE_SETTINGS', self.claude_settings)
        patcher.start()
        self.addCleanup(patcher.stop)
        # Never the real ~/.claude/skills or ~/.codex/skills.
        self.claude_skills = Path(self.temp.name).resolve() / 'claude/skills'
        self.codex_skills = Path(self.temp.name).resolve() / 'codex/skills'
        patcher = patch.object(install, 'AGENT_SKILL_FOLDERS', (
            ('Claude Code', self.claude_skills), ('Codex', self.codex_skills)))
        patcher.start()
        self.addCleanup(patcher.stop)
        # Never the real ~/Library/LaunchAgents.
        patcher = patch.dict('os.environ', {'HOME': str(Path(self.temp.name).resolve() / 'home')})
        patcher.start()
        self.addCleanup(patcher.stop)
        patcher = patch.object(sys, 'platform', 'darwin')  # these tests cover the Mac install
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_symlinked_managed_data_rejected_before_mutation(self):
        bundle = self.prefix / 'harold'
        bundle.mkdir()
        external = Path(self.temp.name) / 'external'
        external.mkdir()
        (external / 'keep').write_text('safe')
        (bundle / 'data').symlink_to(external)
        with self.assertRaisesRegex(RuntimeError, 'symlink'):
            self.install.validate_destination(self.prefix)
        self.assertEqual((external / 'keep').read_text(), 'safe')

    def test_normal_replacement_preserves_data_and_fresh_reinstall_archives_it(self):
        bundle = self.prefix / 'harold'
        (bundle / 'data/events').mkdir(parents=True)
        (bundle / 'data/events/marker').write_text('history')
        (bundle / 'config').mkdir()
        (bundle / 'config/local.toml').write_text('secret')
        stage = self.prefix / 'stage'
        (stage / 'data/events').mkdir(parents=True)
        self.install.replace_bundle(stage, bundle, False)
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'history')
        stage.mkdir()
        (stage / 'data/events').mkdir(parents=True)
        backup = self.install.replace_bundle(stage, bundle, True)
        self.assertFalse((bundle / 'data/events/marker').exists())
        self.assertEqual((backup / 'data/events/marker').read_text(), 'history')

    def test_the_pairing_file_is_carried_only_when_it_is_a_file(self):
        bundle = self.prefix / 'harold'
        (bundle / 'config').mkdir(parents=True)
        self.assertIsNone(self.install.carried_pairing(bundle))
        target = Path(self.temp.name) / 'elsewhere.env'
        target.write_text('HAROLD_PAIRING_CODE=linked-code\n')
        (bundle / 'config/pairing.env').symlink_to(target)
        self.assertIsNone(self.install.carried_pairing(bundle))
        (bundle / 'config/pairing.env').unlink()
        (bundle / 'config/pairing.env').write_bytes(b'HAROLD_PAIRING_CODE=abc123\n')
        self.assertEqual(self.install.carried_pairing(bundle), b'HAROLD_PAIRING_CODE=abc123\n')

    def test_no_terminal_requires_config(self):
        with patch('sys.stdin.isatty', return_value=False):
            with self.assertRaisesRegex(RuntimeError, '--config'):
                self.install.choose_config(None, self.prefix / 'harold')

    def test_no_channel_question_is_asked_and_imessage_is_written(self):
        answers = iter(['me@example.com', '1,36'])
        prompts = []

        def answer(prompt):
            prompts.append(prompt)
            return next(answers)

        with patch('sys.stdin.isatty', return_value=True), patch('builtins.input', side_effect=answer):
            config = self.install.choose_config(None, self.prefix / 'harold').decode()
        self.assertEqual(len(prompts), 2)
        self.assertFalse(any('channel' in prompt.lower() or 'telegram' in prompt.lower() for prompt in prompts))
        self.assertIn('[imessage]\nrecipient = "me@example.com"\nhandle_ids = [1, 36]\n', config)
        self.assertNotIn('telegram', config.lower())
        self.assertNotIn('away_channel', config)

    def test_failed_bundle_swap_restores_original_data(self):
        bundle = self.prefix / 'harold'
        (bundle / 'data/events').mkdir(parents=True)
        (bundle / 'data/events/marker').write_text('irreplaceable')
        stage = self.prefix / 'stage'
        (stage / 'data/events').mkdir(parents=True)
        rename = Path.rename

        def fail_final_swap(source, destination):
            if source == stage:
                raise OSError('injected rename failure')
            return rename(source, destination)

        with patch.object(Path, 'rename', fail_final_swap):
            with self.assertRaisesRegex(OSError, 'injected'):
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'irreplaceable')

    def old_bundle_with_data(self):
        bundle = self.prefix / 'harold'
        (bundle / 'data/store').mkdir(parents=True)
        (bundle / 'data/store/state.db').write_bytes(b'live')
        (bundle / 'data/events').mkdir()
        (bundle / 'data/events/marker').write_text('history')
        stage = self.prefix / 'stage'
        (stage / 'data/store').mkdir(parents=True)
        return bundle, stage

    def test_an_interrupt_during_the_data_copy_leaves_the_live_data_in_place(self):
        bundle, stage = self.old_bundle_with_data()

        def interrupted(source, target):
            (target / 'store').mkdir(parents=True)
            (target / 'store/state.db').write_bytes(b'part')
            raise KeyboardInterrupt

        with patch.object(self.install, 'copy_data_to_backup', interrupted):
            with self.assertRaises(KeyboardInterrupt):
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual((bundle / 'data/store/state.db').read_bytes(), b'live')
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'history')
        self.assertEqual(list(self.prefix.glob('harold.backup-*')), [])
        self.assertTrue(stage.is_dir())

    def test_an_interrupt_after_the_move_puts_the_data_back_and_leaves_no_copy(self):
        bundle, stage = self.old_bundle_with_data()
        rename = Path.rename

        def interrupted_swap(source, destination):
            if source == stage:
                raise KeyboardInterrupt
            return rename(source, destination)

        with patch.object(Path, 'rename', interrupted_swap):
            with self.assertRaises(KeyboardInterrupt):
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual((bundle / 'data/store/state.db').read_bytes(), b'live')
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'history')
        self.assertEqual(list(bundle.glob('data-copy')), [])

    def test_an_interrupt_right_after_the_copy_rename_keeps_the_live_data(self):
        bundle, stage = self.old_bundle_with_data()
        (bundle / 'data/events/marker').write_text('unique-live-marker')
        rename = Path.rename

        def interrupted_after_copy_rename(source, destination):
            result = rename(source, destination)
            if source.name == 'data-copy':
                raise KeyboardInterrupt
            return result

        with patch.object(Path, 'rename', interrupted_after_copy_rename):
            with self.assertRaises(KeyboardInterrupt):
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'unique-live-marker')
        self.assertEqual((bundle / 'data/store/state.db').read_bytes(), b'live')
        self.assertEqual(list(bundle.glob('data-*')), [])

    def test_an_interrupt_right_after_the_live_data_moved_keeps_the_live_data(self):
        bundle, stage = self.old_bundle_with_data()
        (bundle / 'data/events/marker').write_text('unique-live-marker')
        rename = Path.rename

        def interrupted_after_data_move(source, destination):
            result = rename(source, destination)
            if destination == stage / 'data':
                raise KeyboardInterrupt
            return result

        with patch.object(Path, 'rename', interrupted_after_data_move):
            with self.assertRaises(KeyboardInterrupt):
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'unique-live-marker')
        self.assertEqual((bundle / 'data/store/state.db').read_bytes(), b'live')
        self.assertEqual(list(bundle.glob('data-*')), [])

    def test_an_interrupt_at_the_start_of_the_copy_keeps_the_live_store(self):
        bundle, stage = self.old_bundle_with_data()

        def interrupted(source, target):
            raise KeyboardInterrupt

        with patch.object(self.install, 'copy_data_to_backup', interrupted):
            with self.assertRaises(KeyboardInterrupt):
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual((bundle / 'data/store/state.db').read_bytes(), b'live')
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'history')
        self.assertEqual(list(bundle.glob('data-*')), [])

    def test_a_stray_marker_in_the_live_data_does_not_cost_the_live_store(self):
        bundle, stage = self.old_bundle_with_data()
        (bundle / 'data' / self.install.COPY_MARKER).write_text('harold.backup-other-run')

        def interrupted(source, target):
            raise KeyboardInterrupt

        with patch.object(self.install, 'copy_data_to_backup', interrupted):
            with self.assertRaises(KeyboardInterrupt):
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual((bundle / 'data/store/state.db').read_bytes(), b'live')
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'history')
        self.assertEqual(list(bundle.glob('data-*')), [])

    def test_an_interrupt_after_a_failed_copy_cleanup_keeps_the_live_store(self):
        bundle, stage = self.old_bundle_with_data()
        real_rmtree = shutil.rmtree
        interrupts = []

        def rmtree_then_interrupt(path, *args, **kwargs):
            real_rmtree(path, *args, **kwargs)
            if Path(path).name == 'data-copy' and not interrupts:
                interrupts.append(path)
                raise KeyboardInterrupt

        with patch.object(self.install, 'copy_data_to_backup', side_effect=OSError(28, 'full')), \
                patch.object(self.install.shutil, 'rmtree', rmtree_then_interrupt):
            with self.assertRaises(KeyboardInterrupt):
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual((bundle / 'data/store/state.db').read_bytes(), b'live')
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'history')

    def test_a_failed_data_copy_raises_and_puts_everything_back(self):
        bundle, stage = self.old_bundle_with_data()

        def partial_then_full(source, target):
            (target / 'store').mkdir(parents=True)
            (target / 'store/state.db').write_bytes(b'part')
            raise OSError(28, 'No space left on device')

        with patch.object(self.install, 'copy_data_to_backup', partial_then_full):
            with self.assertRaises(self.install.DataCopyFailed) as raised:
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual(raised.exception.reason, 'No space left on device')
        self.assertEqual(str(raised.exception), 'No space left on device')
        self.assertEqual((bundle / 'data/store/state.db').read_bytes(), b'live')
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'history')
        self.assertTrue(bundle.is_dir())
        self.assertTrue(stage.exists())
        self.assertEqual(list(self.prefix.glob('harold.backup-*')), [])
        self.assertEqual(list(bundle.glob('data-copy')), [])

    def test_an_interrupt_right_after_the_failed_copy_keeps_the_live_data(self):
        bundle, stage = self.old_bundle_with_data()
        real_rmtree = shutil.rmtree
        interrupts = []

        def interrupt_once(path, *args, **kwargs):
            if Path(path).name == 'data-copy' and not interrupts:
                interrupts.append(path)
                raise KeyboardInterrupt
            return real_rmtree(path, *args, **kwargs)

        with patch.object(self.install, 'copy_data_to_backup', side_effect=OSError(28, 'full')), \
                patch.object(self.install.shutil, 'rmtree', interrupt_once):
            with self.assertRaises(KeyboardInterrupt):
                self.install.replace_bundle(stage, bundle, False)
        self.assertEqual(len(interrupts), 1)
        self.assertEqual((bundle / 'data/store/state.db').read_bytes(), b'live')
        self.assertEqual((bundle / 'data/events/marker').read_text(), 'history')
        self.assertEqual(list(self.prefix.glob('harold.backup-*')), [])

    def test_a_reinstall_does_not_copy_and_does_not_stop(self):
        bundle, stage = self.old_bundle_with_data()
        with patch.object(self.install, 'copy_data_to_backup', side_effect=OSError(28, 'full')):
            backup = self.install.replace_bundle(stage, bundle, True)
        self.assertTrue(backup.is_dir())

    def test_a_finished_install_leaves_no_copy_marker_and_counts_only_data_bytes(self):
        bundle, stage = self.old_bundle_with_data()
        info = {}
        backup = self.install.replace_bundle(stage, bundle, False, info)
        self.assertFalse((backup / 'data' / self.install.COPY_MARKER).exists())
        self.assertEqual(info['bytes'], len(b'live') + len('history'))

    def test_a_copy_with_no_store_or_events_copies_nothing(self):
        bundle = self.prefix / 'harold'
        (bundle / 'data/pod').mkdir(parents=True)
        stage = self.prefix / 'stage'
        (stage / 'data').mkdir(parents=True)
        info = {}
        backup = self.install.replace_bundle(stage, bundle, False, info)
        self.assertEqual(info, {})
        self.assertFalse((backup / 'data').exists())
        self.assertTrue((bundle / 'data/pod').is_dir())

    def test_the_service_starts_at_login_and_restarts_after_a_crash(self):
        home = Path(self.temp.name).resolve() / 'home'
        with patch.dict('os.environ', {'HOME': str(home)}):
            metadata = self.install.make_metadata(self.prefix, '127.0.0.1:50060')
        path = self.prefix / 'service.plist'
        self.install.write_plist(path, metadata)
        plist = plistlib.loads(path.read_bytes())
        self.assertEqual(metadata['plist'], str(self.prefix / 'harold/service.plist'))
        self.assertEqual(metadata['launch_agent'],
                         str(home / 'Library/LaunchAgents' / (metadata['label'] + '.plist')))
        self.assertIs(plist['RunAtLoad'], True)
        self.assertEqual(plist['KeepAlive'], {'SuccessfulExit': False})
        self.assertEqual(plist['ThrottleInterval'], 30)
        self.assertEqual(plist['EnvironmentVariables'], dict(metadata['env'], HAROLD_LAUNCHD='1'))
        # Only launchd's start says so; haroldctl's checks run with metadata['env'].
        self.assertNotIn('HAROLD_LAUNCHD', metadata['env'])

    def test_a_failed_launch_agents_write_leaves_no_temporary_file(self):
        folder = Path(self.temp.name).resolve() / 'home/Library/LaunchAgents'
        metadata = {'plist': str(self.prefix / 'missing/service.plist'),
                    'launch_agent': str(folder / 'com.harold.agent.test.plist')}
        with self.assertRaises(OSError):
            self.install.write_launch_agent(metadata)
        self.assertEqual(list(folder.iterdir()), [])

    def test_the_launch_agents_copy_is_written_through_a_file_launchd_does_not_load(self):
        folder = Path(self.temp.name).resolve() / 'home/Library/LaunchAgents'
        (self.prefix / 'service.plist').write_text('plist')
        metadata = {'plist': str(self.prefix / 'service.plist'),
                    'launch_agent': str(folder / 'com.harold.agent.test.plist')}
        names = []
        replace = os.replace

        def record(source, destination):
            names.append(Path(source).name)
            return replace(source, destination)

        with patch.object(self.install.os, 'replace', record):
            self.install.write_launch_agent(metadata)
        pending, = names
        self.assertFalse(pending.endswith('.plist'), pending)
        self.assertEqual([path.name for path in folder.iterdir()], ['com.harold.agent.test.plist'])

    def test_signing_and_invalid_config_fail_before_stopping_old_service(self):
        repo = self.prefix / 'repo'
        for name in ('target/release/harold', 'target/release/control-tower',
                     'harold-api/proto/harold.proto', 'hooks/shared/harold_turn_complete.py',
                     'hooks/providers/claude_turn_complete.py', 'hooks/providers/codex_turn_complete.py',
                     'hooks/claude-mod/hooks/register.ts', 'hooks/claude-mod/hooks/register.test.ts',
                     'hooks/claude-mod/hooks/accept.ts', 'hooks/claude-mod/hooks/hooks.json',
                     'hooks/claude-mod/.claude-plugin/plugin.json',
                     'scripts/harold_service.py', 'harold/config/default.toml',
                     'harold/config/local.template.toml', SKILL_FILE):
            path = repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('fixture')
        bundle = self.prefix / 'harold'
        (bundle / 'config').mkdir(parents=True)
        (bundle / 'config/local.toml').write_text('secret = "never print"')
        args = argparse.Namespace(prefix=str(self.prefix), config=None, reinstall=False,
                                  offline=True, signing_identity='-', prebuilt_dir=None)
        for failure in ('codesign', '--check-config'):
            with self.subTest(failure=failure):
                def process(argv, **kwargs):
                    if failure in argv:
                        if kwargs.get('check'):
                            raise subprocess.CalledProcessError(1, argv)
                        return subprocess.CompletedProcess(argv, 1, '', 'secret = never print')
                    if argv[0] == 'cargo':
                        return subprocess.CompletedProcess(argv, 0, artifact_output(repo, repo / 'target/release'), '')
                    return subprocess.CompletedProcess(argv, 0, '', '')
                with patch.object(self.install, 'prerequisites'), \
                     patch.object(subprocess, 'run', side_effect=process), \
                     patch.object(self.install.service, 'unload') as unload:
                    with self.assertRaises((RuntimeError, subprocess.CalledProcessError)):
                        self.install.install(args, repo)
                unload.assert_not_called()
                self.assertEqual((bundle / 'config/local.toml').read_text(), 'secret = "never print"')

    def test_installs_cargo_reported_binaries_from_custom_target_directory(self):
        repo = self.prefix / 'repo'
        for name in ('target/release/harold', 'target/release/control-tower',
                     'harold-api/proto/harold.proto', 'hooks/shared/harold_turn_complete.py',
                     'hooks/providers/claude_turn_complete.py', 'hooks/providers/codex_turn_complete.py',
                     'hooks/claude-mod/hooks/register.ts', 'hooks/claude-mod/hooks/register.test.ts', 'hooks/claude-mod/hooks/accept.ts', 'hooks/claude-mod/hooks/hooks.json',
                     'hooks/claude-mod/.claude-plugin/plugin.json',
                     'scripts/harold_service.py', 'harold/config/default.toml',
                     'harold/config/local.template.toml', SKILL_FILE):
            path = repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('stale')
        (repo / SKILL_FILE).write_text(SKILL_SOURCE)
        self.claude_skills.mkdir()
        artifacts = self.prefix / 'custom target/aarch64-apple-darwin/release'
        artifacts.mkdir(parents=True)
        for name in ('harold', 'control-tower'):
            (artifacts / name).write_text('built ' + name)
        config = self.prefix / 'import.toml'
        config.write_text('agents = []')
        args = argparse.Namespace(prefix=str(self.prefix), config=str(config), reinstall=False,
                                  offline=True, signing_identity='-', prebuilt_dir=None)

        def process(argv, **kwargs):
            if argv[0] == 'cargo':
                self.assertIn('--locked', argv)
                self.assertIn('--offline', argv)
                self.assertIn('--message-format=json-render-diagnostics', argv)
                self.assertEqual(argv[argv.index('control-tower') - 1], '-p')
                self.assertNotIn('tmx-agent-dash', argv)
                output = artifact_output(repo, artifacts)
            elif '--check-config' in argv:
                self.assertEqual(Path(argv[0]).read_text(), 'built harold')
                output = json.dumps({'grpc_addr': '127.0.0.1:50123',
                                     'store_path': str(self.prefix / 'harold/data/store')})
            else:
                output = ''
            return subprocess.CompletedProcess(argv, 0, output, '')

        printed = io.StringIO()
        with patch.object(self.install, 'prerequisites'), \
             patch.object(subprocess, 'run', side_effect=process), \
             patch.object(self.install.service, 'listener_pids', return_value=set()), \
             patch.object(self.install.service, 'unload'), \
             patch.object(self.install.service, 'stop_unmanaged'), \
             patch.object(self.install.service, 'start', return_value=123):
            with contextlib.redirect_stdout(printed):
                self.install.install(args, repo)
        self.assertEqual((self.prefix / 'harold/harold').read_text(), 'built harold')
        self.assertEqual((self.prefix / 'harold' / 'control-tower').read_text(), 'built control-tower')
        for name in ('claude_turn_complete.py', 'codex_turn_complete.py'):
            self.assertEqual((self.prefix / 'harold/hooks' / name).read_text(), 'stale')
        mod = self.prefix / 'harold/hooks/claude-mod'
        self.assertEqual((mod / 'hooks/register.ts').read_text(), 'stale')
        for runtime in RUNTIME_FILES:
            self.assertTrue((mod / runtime).is_file(), runtime)
        self.assertFalse((mod / 'hooks/register.test.ts').exists())
        self.assertIn(str(mod), self.claude_settings.read_text())
        binary = self.prefix / 'harold/harold'
        skill = self.prefix / 'harold' / SKILL_FILE
        # The prefix has spaces, so the path is quoted for the shell.
        self.assertEqual(skill.read_text(),
                         "Run '{0}' send-message-to-away-device. Again: '{0}'\n".format(binary))
        link = self.claude_skills / 'send-message-to-away-device'
        self.assertEqual(os.readlink(link), str(skill.parent))
        self.assertEqual((link / 'SKILL.md').read_text(), skill.read_text())
        self.assertIn('Claude Code skill: linked\n', printed.getvalue())
        self.assertIn('Codex skill: not linked ({} not found)\n'.format(self.codex_skills),
                      printed.getvalue())
        self.assertFalse(self.codex_skills.exists())


    def test_prebuilt_install_needs_no_source_or_build_tools(self):
        release = self.prefix / 'release'
        for name in ('harold', 'control-tower', 'harold.proto',
                     'hooks/harold_turn_complete.py', 'hooks/claude_turn_complete.py',
                     'hooks/codex_turn_complete.py', 'scripts/harold_service.py',
                     'hooks/claude-mod/hooks/register.ts', 'hooks/claude-mod/hooks/accept.ts', 'hooks/claude-mod/hooks/hooks.json',
                     'hooks/claude-mod/.claude-plugin/plugin.json',
                     'config/default.toml', 'config/local.template.toml', SKILL_FILE):
            path = release / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('release ' + name)
            if name in ('harold', 'control-tower'):
                path.chmod(0o755)
        config = self.prefix / 'import.toml'
        config.write_text('agents = []')
        (self.prefix / 'harold/config').mkdir(parents=True)
        old_pairing = self.prefix / 'harold/config/pairing.env'
        old_pairing.write_text('HAROLD_PAIRING_CODE=test-code-0000\n')
        old_pairing.chmod(0o644)
        args = argparse.Namespace(prefix=str(self.prefix), config=str(config), reinstall=False,
                                  offline=False, signing_identity='-', prebuilt_dir=str(release))

        def which(name):
            if name in ('cargo', 'rustc', 'protoc'):
                return None
            return '/usr/bin/' + name

        def process(argv, **kwargs):
            self.assertNotEqual(argv[0], 'cargo')
            output = ''
            if '--check-config' in argv:
                self.assertEqual(Path(argv[0]).read_text(), 'release harold')
                output = json.dumps({'grpc_addr': '127.0.0.1:50123',
                                     'store_path': str(self.prefix / 'harold/data/store')})
            return subprocess.CompletedProcess(argv, 0, output, '')

        with patch.object(sys, 'platform', 'darwin'), \
             patch.object(self.install.os, 'getuid', return_value=501), \
             patch.object(self.install.shutil, 'which', side_effect=which), \
             patch.object(self.install, 'build_binaries') as build, \
             patch.object(subprocess, 'run', side_effect=process), \
             patch.object(self.install.service, 'listener_pids', return_value=set()), \
             patch.object(self.install.service, 'unload'), \
             patch.object(self.install.service, 'stop_unmanaged'), \
             patch.object(self.install.service, 'start', return_value=123):
            self.install.install(args, self.prefix / 'nonexistent source')
        build.assert_not_called()
        self.assertEqual((self.prefix / 'harold/harold').read_text(), 'release harold')
        self.assertEqual((self.prefix / 'harold' / 'control-tower').read_text(), 'release control-tower')
        self.assertEqual((self.prefix / 'harold/config/default.toml').read_text(),
                         'release config/default.toml')
        self.assertEqual((self.prefix / 'harold/config/local.toml').read_text(), 'agents = []')
        carried = self.prefix / 'harold/config/pairing.env'
        self.assertEqual(carried.read_text(), 'HAROLD_PAIRING_CODE=test-code-0000\n')
        self.assertEqual(carried.stat().st_mode & 0o777, 0o600)
        for name in ('claude_turn_complete.py', 'codex_turn_complete.py'):
            self.assertEqual((self.prefix / 'harold/hooks' / name).read_text(),
                             'release hooks/' + name)
        mod = self.prefix / 'harold/hooks/claude-mod'
        self.assertEqual((mod / 'hooks/register.ts').read_text(),
                         'release hooks/claude-mod/hooks/register.ts')
        for runtime in RUNTIME_FILES:
            self.assertTrue((mod / runtime).is_file(), runtime)
        self.assertIn(str(mod), self.claude_settings.read_text())
        skill = self.prefix / 'harold' / SKILL_FILE
        self.assertEqual(skill.read_text(), 'release ' + SKILL_FILE)
        self.assertFalse(self.claude_skills.exists())
        self.assertFalse(self.codex_skills.exists())


    def test_prebuilt_requires_runtime_tools(self):
        with patch.object(sys, 'platform', 'darwin'), \
             patch.object(self.install.os, 'getuid', return_value=501), \
             patch.object(self.install.shutil, 'which', return_value=None):
            with self.assertRaisesRegex(RuntimeError, 'Missing tools: tmux, grpcurl') as error:
                self.install.prerequisites(self.prefix, prebuilt=True)
        self.assertNotIn('rustup', str(error.exception))
        self.assertNotIn('protobuf', str(error.exception))

    def test_a_source_install_no_longer_needs_the_events_submodule(self):
        repo = self.prefix / 'repo'
        repo.mkdir()
        (repo / 'Cargo.lock').write_text('# lock\n')
        with patch.object(sys, 'platform', 'darwin'), \
             patch.object(self.install.os, 'getuid', return_value=501), \
             patch.object(self.install.shutil, 'which', return_value='/usr/bin/tool'):
            self.install.prerequisites(repo)
        self.assertFalse((repo / 'events').exists())

    def test_release_missing_any_mod_runtime_file_is_rejected(self):
        for missing in RUNTIME_FILES:
            release = self.prefix / ('release-' + missing.replace('/', '-'))
            for name in ('harold', 'control-tower', 'harold.proto',
                         'hooks/harold_turn_complete.py', 'hooks/claude_turn_complete.py',
                         'hooks/codex_turn_complete.py', 'scripts/harold_service.py',
                         'config/default.toml', 'config/local.template.toml', SKILL_FILE,
                         *('hooks/claude-mod/' + f for f in RUNTIME_FILES if f != missing)):
                path = release / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('x')
                path.chmod(0o755)
            with self.assertRaisesRegex(RuntimeError, re.escape(missing)):
                self.install.prebuilt_files(release)

    def test_incomplete_release_rejected(self):
        with self.assertRaisesRegex(RuntimeError, 'Missing regular release file'):
            self.install.prebuilt_files(self.prefix)

    def test_prebuilt_without_control_tower_is_rejected(self):
        release = self.prefix / 'release-no-control-tower'
        for name in ('harold', 'harold.proto',
                     'hooks/harold_turn_complete.py', 'hooks/claude_turn_complete.py',
                     'hooks/codex_turn_complete.py', 'scripts/harold_service.py',
                     'config/default.toml', 'config/local.template.toml', SKILL_FILE,
                     *('hooks/claude-mod/' + f for f in RUNTIME_FILES)):
            path = release / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('x')
            path.chmod(0o755)
        with self.assertRaisesRegex(RuntimeError, re.escape(str(release / 'control-tower'))):
            self.install.prebuilt_files(release)
        (release / 'control-tower').write_text('x')
        (release / 'control-tower').chmod(0o755)
        self.assertEqual(sorted(self.install.prebuilt_files(release)), ['control-tower', 'harold'])

    def test_release_without_the_skill_is_rejected(self):
        release = self.prefix / 'release-no-skill'
        for name in ('harold', 'control-tower', 'harold.proto',
                     'hooks/harold_turn_complete.py', 'hooks/claude_turn_complete.py',
                     'hooks/codex_turn_complete.py', 'scripts/harold_service.py',
                     'config/default.toml', 'config/local.template.toml',
                     *('hooks/claude-mod/' + f for f in RUNTIME_FILES)):
            path = release / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('x')
            path.chmod(0o755)
        with self.assertRaisesRegex(RuntimeError, re.escape(SKILL_FILE)):
            self.install.prebuilt_files(release)
        (release / SKILL_FILE).parent.mkdir(parents=True)
        (release / SKILL_FILE).write_text('x')
        self.install.prebuilt_files(release)

    def test_no_signing_identity_argument_means_the_saved_one(self):
        with patch.object(sys, 'argv', ['install.py', '--prefix', str(self.prefix)]), \
             patch.object(self.install, 'install') as install:
            self.assertEqual(self.install.main(), 0)
        self.assertIsNone(install.call_args.args[0].signing_identity)

    def test_missing_cargo_artifact_fails_without_falling_back_to_stale_file(self):
        repo = self.prefix / 'repo'
        (repo / 'target/release').mkdir(parents=True)
        (repo / 'target/release/harold').write_text('stale')
        reply = subprocess.CompletedProcess([], 0, '{"reason":"build-finished","success":true}\n', '')
        with patch.object(subprocess, 'run', return_value=reply):
            with self.assertRaisesRegex(RuntimeError, 'did not report'):
                self.install.build_binaries(repo, offline=True)

    def test_a_source_install_records_its_commit_and_source_as_its_version(self):
        stage = self.prefix / 'stage'
        stage.mkdir()
        reply = subprocess.CompletedProcess([], 0, 'b4142e9f00d5\n', '')
        with patch.object(subprocess, 'run', return_value=reply) as run:
            self.install.record_version(stage, None, self.prefix / 'repo')
        self.assertEqual(run.call_args.args[0], ['git', 'rev-parse', 'HEAD'])
        self.assertEqual(run.call_args.kwargs['cwd'], self.prefix / 'repo')
        self.assertEqual((stage / 'VERSION').read_text(), 'source\n')
        self.assertEqual((stage / 'REVISION').read_text(), 'b4142e9f00d5\n')

    def test_a_source_install_without_git_records_no_revision(self):
        stage = self.prefix / 'stage'
        stage.mkdir()
        with patch.object(subprocess, 'run', side_effect=FileNotFoundError('git')):
            self.install.record_version(stage, None, self.prefix / 'repo')
        self.assertEqual((stage / 'VERSION').read_text(), 'source\n')
        self.assertFalse((stage / 'REVISION').exists())

    def test_an_unreadable_service_file_names_no_python(self):
        bundle = self.prefix / 'old'
        bundle.mkdir()
        self.assertIsNone(self.install.plist_python(bundle))
        (bundle / 'service.plist').write_text('not a plist')
        self.assertIsNone(self.install.plist_python(bundle))

    def test_a_truncated_service_file_names_no_python(self):
        bundle = self.prefix / 'old'
        bundle.mkdir()
        (bundle / 'service.plist').write_bytes(b'<?xml version="1.0"?><plist><dict>')
        self.assertIsNone(self.install.plist_python(bundle))

    def test_a_version_file_that_is_not_text_reads_as_not_recorded(self):
        bundle = self.prefix / 'old'
        bundle.mkdir()
        (bundle / 'VERSION').write_bytes(b'\xff\xfe')
        self.assertIsNone(self.install.recorded(bundle, 'VERSION'))


# Leaves `signing_identity` out of an installed service.json, as installs before it did.
NOT_RECORDED = object()


class InstalledServiceTests(unittest.TestCase):
    """A whole install from a release fixture, with codesign, launchctl and lsof faked."""

    def setUp(self):
        import install
        self.install = install
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name).resolve()
        self.prefix = root / 'prefix with spaces'
        self.prefix.mkdir()
        self.bundle = self.prefix / 'harold'
        # Never the real ~/.claude/settings.json, ~/.claude/skills or ~/.codex/skills.
        for name, value in (('CLAUDE_SETTINGS', root / 'claude/settings.json'),
                            ('AGENT_SKILL_FOLDERS', ())):
            patcher = patch.object(install, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        # Never the real ~/Library/LaunchAgents.
        self.home = root / 'home'
        patcher = patch.dict('os.environ', {'HOME': str(self.home)})
        patcher.start()
        self.addCleanup(patcher.stop)
        patcher = patch.object(sys, 'platform', 'darwin')  # these tests cover the Mac install
        patcher.start()
        self.addCleanup(patcher.stop)
        self.release = root / 'release'
        for name in ('harold', 'control-tower', 'harold.proto',
                     'hooks/harold_turn_complete.py', 'hooks/claude_turn_complete.py',
                     'hooks/codex_turn_complete.py', 'scripts/harold_service.py',
                     'config/default.toml', 'config/local.template.toml', SKILL_FILE,
                     *('hooks/claude-mod/' + f for f in RUNTIME_FILES)):
            path = self.release / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('release ' + name)
        for name in ('harold', 'control-tower'):
            (self.release / name).chmod(0o755)
        (self.release / 'config/default.toml').write_text('[notify]\nmachines = []\n')
        self.config = root / 'import.toml'
        self.config.write_text('[imessage]\nrecipient = "me@example.com"\nhandle_ids = [1]\n')
        self.requirements = {}
        self.sign_error = None
        self.calls = []
        self.order = []

    def unloaded(self, metadata):
        self.order.append(('unload', metadata['label']))

    def started(self, metadata):
        self.order.append(('start', Path(metadata['launch_agent']).is_file()))
        return 123

    def process(self, argv, **kwargs):
        self.calls.append(list(argv))
        if argv[:2] == ['codesign', '-dr']:
            requirement = self.requirements.get(
                'old' if Path(argv[-1]).read_text() == 'old harold' else 'new')
            if requirement is None:
                return subprocess.CompletedProcess(argv, 1, '', argv[-1] + ': code object is not signed at all\n')
            return subprocess.CompletedProcess(argv, 0, '# designated => {}\n'.format(requirement),
                                               'Executable={}\n'.format(argv[-1]))
        if argv[:2] == ['codesign', '--force'] and self.sign_error:
            return subprocess.CompletedProcess(argv, 1, '', self.sign_error + '\n')
        if '--check-config' in argv:
            return subprocess.CompletedProcess(argv, 0, json.dumps({
                'grpc_addr': '127.0.0.1:50123', 'store_path': str(self.bundle / 'data/store')}), '')
        return subprocess.CompletedProcess(argv, 0, '', '')

    def install_existing(self, saved=NOT_RECORDED):
        (self.bundle / 'config').mkdir(parents=True)
        (self.bundle / 'harold').write_text('old harold')
        (self.bundle / 'config/local.toml').write_text(self.config.read_text())
        metadata = self.install.make_metadata(self.prefix, '127.0.0.1:50123')
        if saved is not NOT_RECORDED:
            metadata['signing_identity'] = saved
        (self.bundle / 'service.json').write_text(json.dumps(metadata))

    def install_existing_programs(self, log):
        """The rest of an install from v0.5.0: its haroldctl, control-tower program, data, version and revision.
        The old haroldctl writes `old <command>` to `log`."""
        (self.prefix / 'haroldctl').write_text('#!/bin/sh\necho "old $*" >> {}\n'.format(shlex.quote(str(log))))
        (self.prefix / 'haroldctl').chmod(0o755)
        (self.prefix / 'tmx-agent-dash').write_text('old dash')
        (self.bundle / 'data/events').mkdir(parents=True)
        (self.bundle / 'data/events/marker').write_text('history')
        (self.bundle / 'data/events/harold/main').mkdir(parents=True)
        (self.bundle / 'data/events/harold/main/catalog.db').write_bytes(bytes(range(256)) * 64)
        (self.bundle / 'VERSION').write_text('v0.5.0\n')
        (self.bundle / 'REVISION').write_text('08095dde67ea035ff4e0aaa3109a344d39ceb019\n')

    def rollback(self):
        """The one backup folder and the commands its ROLLBACK.md gives."""
        backup, = self.prefix.glob('harold.backup-*')
        note = (backup / 'ROLLBACK.md').read_text()
        return backup, note, note.split('```sh\n', 1)[1].split('\n```', 1)[0]

    def run_install(self, identity='-', reinstall=False, start_error=None):
        self.printed = io.StringIO()
        args = argparse.Namespace(prefix=str(self.prefix), config=str(self.config), reinstall=reinstall,
                                  offline=False, signing_identity=identity, prebuilt_dir=str(self.release))
        with patch.object(self.install, 'prerequisites'), \
             patch.object(subprocess, 'run', side_effect=self.process), \
             patch.object(self.install.service, 'listener_pids', return_value=set()), \
             patch.object(self.install.service, 'stop') as self.stop, \
             patch.object(self.install.service, 'unload', side_effect=self.unloaded) as self.unload, \
             patch.object(self.install.service, 'stop_unmanaged'), \
             patch.object(self.install.service, 'start',
                          side_effect=start_error or self.started) as self.start, \
             contextlib.redirect_stdout(self.printed):
            self.install.install(args, self.prefix / 'no source')
        return self.printed.getvalue()

    def installed_metadata(self):
        return json.loads((self.bundle / 'service.json').read_text())

    def old_events(self):
        """Every file under data/events, by its path there, with the SHA-256 of its bytes."""
        events = self.bundle / 'data/events'
        return {str(path.relative_to(events)): hashlib.sha256(path.read_bytes()).hexdigest()
                for path in sorted(events.rglob('*')) if path.is_file()}

    def not_carried_over(self):
        return 'Not carried over from {}: messages still waiting to be delivered, and the mute and pause settings'.format(
            self.bundle / 'data/events')

    def test_an_upgrade_starts_a_new_store_and_leaves_the_old_events_byte_for_byte(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        before = self.old_events()
        self.assertEqual(len(before), 2)
        output = self.run_install('-')
        self.assertEqual(self.old_events(), before)
        self.assertEqual(self.installed_metadata()['env']['HAROLD__STORE__PATH'], str(self.bundle / 'data/store'))
        # The note is the installer's last output, after the mod and skill lines.
        self.assertTrue(output.endswith(
            'New store: {}\n'.format(self.bundle / 'data/store') + self.not_carried_over() +
            ' (voice is on, messaging unpaused). That folder is left as it was.\n'), output)
        self.assertLess(output.index('Claude Code mod:'), output.index('New store:'))

    def test_an_install_onto_a_bundle_that_already_has_the_new_store_says_nothing_about_it(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        (self.bundle / 'data/store').mkdir()
        (self.bundle / 'data/store/marker').write_text('new history')
        output = self.run_install('-')
        self.assertEqual((self.bundle / 'data/store/marker').read_text(), 'new history')
        self.assertIn('Harold ready at', output)
        self.assertNotIn('New store:', output)
        self.assertNotIn('Not carried over', output)

    def test_upgrading_a_bundle_without_old_events_says_nothing_about_the_old_store(self):
        self.install_existing(saved='-')
        self.assertFalse((self.bundle / 'data/events').exists())
        output = self.run_install('-')
        self.assertIn('Harold ready at', output)
        self.assertNotIn('New store:', output)
        self.assertNotIn('Not carried over', output)

    def test_a_fresh_reinstall_or_a_first_install_says_nothing_about_the_old_store(self):
        output = self.run_install('-')
        self.assertNotIn('Not carried over', output)
        self.assertNotIn('New store:', output)
        # Only the old events folder is left, as after an upgrade; --reinstall archives it all.
        (self.bundle / 'data/store').rmdir()
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        output = self.run_install('-', reinstall=True)
        self.assertIn('Harold ready at', output)
        self.assertNotIn('Not carried over', output)
        self.assertNotIn('New store:', output)

    def test_install_tells_to_restart_the_control_tower(self):
        output = self.run_install('-')
        line = 'restart the control-tower to see blocked, done and error'
        self.assertIn(line + '\n', output)
        self.assertLess(output.index('Harold ready at'), output.index(line))

    def test_launchd_starts_harold_itself(self):
        self.run_install()
        plist = plistlib.loads((self.bundle / 'service.plist').read_bytes())
        # One item, the whole path, although the prefix has a space in it.
        self.assertEqual(plist['ProgramArguments'], [str(self.bundle / 'harold')])
        for item in plist['ProgramArguments']:
            self.assertNotIn('python', item)
            self.assertNotIn('service.py', item)

    def test_every_other_service_file_key_keeps_its_value(self):
        self.run_install()
        plist = plistlib.loads((self.bundle / 'service.plist').read_bytes())
        metadata = self.installed_metadata()
        self.assertEqual(plist, {
            'Label': metadata['label'],
            'ProgramArguments': [str(self.bundle / 'harold')],
            'WorkingDirectory': str(self.bundle),
            'RunAtLoad': True,
            'KeepAlive': {'SuccessfulExit': False},
            'ThrottleInterval': 30,
            'ExitTimeOut': 10,
            'EnvironmentVariables': dict(metadata['env'], HAROLD_LAUNCHD='1'),
            'StandardOutPath': str(self.bundle / 'harold.log'),
            'StandardErrorPath': str(self.bundle / 'harold.log'),
        })
        self.assertEqual(sorted(metadata['env']), sorted([
            'HAROLD_CONFIG_DIR', 'HAROLD_ENV', 'HAROLD__STORE__PATH', 'PATH', 'LANG', 'LC_ALL']))

    def launch_agent(self):
        return self.home / 'Library/LaunchAgents' / (self.installed_metadata()['label'] + '.plist')

    def test_an_upgrade_boots_out_the_old_job_writes_both_copies_and_starts_through_haroldctl(self):
        self.install_existing(saved='-')
        old = self.install.make_metadata(self.prefix, '127.0.0.1:50123')
        old_plist = dict(Label=old['label'], ProgramArguments=[str(self.bundle / 'harold')], RunAtLoad=False)
        (self.bundle / 'service.plist').write_bytes(plistlib.dumps(old_plist))
        self.run_install()
        label = self.installed_metadata()['label']
        # The old job is booted out, never disabled; start then finds the LaunchAgents copy.
        self.assertEqual(self.order, [('unload', label), ('start', True)])
        self.stop.assert_not_called()
        self.assertEqual(self.start.call_args.args[0]['launch_agent'], str(self.launch_agent()))
        self.assertEqual(self.launch_agent().read_bytes(), (self.bundle / 'service.plist').read_bytes())
        plist = plistlib.loads(self.launch_agent().read_bytes())
        self.assertEqual((plist['Label'], plist['RunAtLoad'], plist['KeepAlive'], plist['ThrottleInterval'],
                          plist['EnvironmentVariables']['HAROLD_LAUNCHD']),
                         (label, True, {'SuccessfulExit': False}, 30, '1'))
        if os.environ.get('HAROLD_TEST_EVIDENCE') == '1':
            print('{}: Label={} RunAtLoad={} KeepAlive={} ThrottleInterval={} HAROLD_LAUNCHD={}'.format(
                self.launch_agent(), plist['Label'], plist['RunAtLoad'], plist['KeepAlive'],
                plist['ThrottleInterval'], plist['EnvironmentVariables']['HAROLD_LAUNCHD']))

    def test_an_install_replaces_an_older_launch_agents_copy(self):
        self.launch_agent_folder = self.home / 'Library/LaunchAgents'
        self.launch_agent_folder.mkdir(parents=True)
        label = self.install.make_metadata(self.prefix, '')['label']
        (self.launch_agent_folder / (label + '.plist')).write_text('stale')
        self.run_install()
        self.assertEqual(self.launch_agent().read_bytes(), (self.bundle / 'service.plist').read_bytes())
        self.assertEqual(sorted(path.name for path in self.launch_agent_folder.iterdir()), [label + '.plist'])

    def test_a_failed_install_leaves_the_launch_agents_folder_alone(self):
        self.install_existing(saved='Kah Geh Tan')
        self.sign_error = 'Kah Geh Tan: no identity found'
        with self.assertRaisesRegex(RuntimeError, 'no identity found'):
            self.run_install(None)
        self.assertFalse((self.home / 'Library/LaunchAgents').exists())

    def test_the_service_runs_at_normal_priority(self):
        self.run_install()
        plist = plistlib.loads((self.bundle / 'service.plist').read_bytes())
        self.assertNotIn('ProcessType', plist)

    def test_a_release_records_its_version_and_revision_in_the_bundle(self):
        (self.release / 'VERSION').write_text('v0.6.0\n')
        (self.release / 'REVISION').write_text('31323b9f00d5c0ffee\n')
        self.run_install()
        self.assertEqual((self.bundle / 'VERSION').read_text(), 'v0.6.0\n')
        self.assertEqual((self.bundle / 'REVISION').read_text(), '31323b9f00d5c0ffee\n')

    def test_a_release_without_a_version_installs_without_one(self):
        (self.release / 'REVISION').write_text('08095dde67ea035ff4e0aaa3109a344d39ceb019\n')
        self.run_install()
        self.assertFalse((self.bundle / 'VERSION').exists())
        self.assertEqual((self.bundle / 'REVISION').read_text(),
                         '08095dde67ea035ff4e0aaa3109a344d39ceb019\n')

    def signed(self):
        """(identity, identifier, file name) for each signing call, in order."""
        return [(argv[3], argv[5], Path(argv[6]).name) for argv in self.calls
                if argv[:2] == ['codesign', '--force']]

    def test_a_named_identity_signs_both_programs_with_fixed_identifiers_and_is_saved(self):
        self.run_install('Kah Geh Tan')
        self.assertEqual(self.signed(), [('Kah Geh Tan', 'harold', 'harold'),
                                         ('Kah Geh Tan', 'control-tower', 'control-tower')])
        self.assertEqual(self.installed_metadata()['signing_identity'], 'Kah Geh Tan')

    def test_without_a_name_the_saved_identity_is_used_and_kept(self):
        self.install_existing(saved='Kah Geh Tan')
        self.run_install(None)
        self.assertEqual({identity for identity, _, _ in self.signed()}, {'Kah Geh Tan'})
        self.assertEqual(self.installed_metadata()['signing_identity'], 'Kah Geh Tan')

    def test_a_name_given_wins_over_the_saved_one(self):
        self.install_existing(saved='Kah Geh Tan')
        self.run_install('-')
        self.assertEqual({identity for identity, _, _ in self.signed()}, {'-'})
        self.assertEqual(self.installed_metadata()['signing_identity'], '-')

    def test_with_no_name_and_nothing_installed_signing_is_ad_hoc(self):
        self.run_install(None)
        self.assertEqual(self.signed(), [('-', 'harold', 'harold'),
                                         ('-', 'control-tower', 'control-tower')])
        self.assertEqual(self.installed_metadata()['signing_identity'], '-')

    def test_an_install_from_before_saved_identities_reads_as_ad_hoc(self):
        self.install_existing(saved=NOT_RECORDED)
        self.run_install(None)
        self.assertEqual({identity for identity, _, _ in self.signed()}, {'-'})
        self.assertEqual(self.installed_metadata()['signing_identity'], '-')

    def test_a_signing_failure_shows_codesign_and_leaves_the_running_harold_alone(self):
        self.install_existing(saved='Kah Geh Tan')
        self.sign_error = 'Kah Geh Tan: no identity found'
        with self.assertRaisesRegex(RuntimeError, 'Kah Geh Tan: no identity found'):
            self.run_install(None)
        self.unload.assert_not_called()
        self.assertEqual((self.bundle / 'harold').read_text(), 'old harold')
        # The installed record is untouched: it still names the saved identity.
        self.assertEqual(self.installed_metadata()['signing_identity'], 'Kah Geh Tan')

    def test_a_saved_identity_that_is_not_text_stops_the_install(self):
        self.install_existing(saved=None)
        with self.assertRaisesRegex(RuntimeError, 'signing_identity must be text'):
            self.run_install(None)
        self.unload.assert_not_called()
        self.assertEqual((self.bundle / 'harold').read_text(), 'old harold')
        self.assertNotIn(['codesign', '--force'], [argv[:2] for argv in self.calls])

    CERTIFICATE = 'identifier "harold" and certificate leaf = H"ab12"'

    def assert_after_ready(self, output, text):
        self.assertIn(text, output)
        self.assertLess(output.index('Harold ready at'), output.index(text))
        self.assertLess(output.index(text), output.index('Claude Code mod:'))

    def test_the_same_signature_says_the_grants_carry_over(self):
        self.install_existing(saved='Kah Geh Tan')
        self.requirements = {'old': self.CERTIFICATE, 'new': self.CERTIFICATE}
        output = self.run_install(None)
        self.assert_after_ready(output, self.install.CARRY_OVER + '\n')
        self.assertNotIn('macOS permissions for this build', output)

    def test_a_new_signature_prints_the_steps_with_the_installed_path(self):
        self.install_existing(saved='-')
        self.requirements = {'old': 'cdhash H"315d"', 'new': self.CERTIFICATE}
        output = self.run_install('Kah Geh Tan')
        self.assert_after_ready(output, self.install.GRANT_STEPS.format(binary=self.bundle / 'harold') + '\n')
        self.assertIn('enter {}, press Enter'.format(self.bundle / 'harold'), output)
        self.assertNotIn(self.install.AD_HOC_NOTE, output)
        self.assertNotIn(self.install.CARRY_OVER, output)

    def test_an_installed_harold_without_a_readable_signature_prints_the_steps(self):
        self.install_existing(saved='Kah Geh Tan')
        self.requirements = {'new': self.CERTIFICATE}
        output = self.run_install(None)
        self.assertIn('macOS permissions for this build of harold:', output)
        self.assertNotIn(self.install.CARRY_OVER, output)

    def test_a_first_ad_hoc_install_prints_the_steps_and_the_note(self):
        self.requirements = {'new': 'cdhash H"9a9a"'}
        output = self.run_install('-')
        steps = self.install.GRANT_STEPS.format(binary=self.bundle / 'harold')
        self.assert_after_ready(output, steps + '\n' + self.install.AD_HOC_NOTE + '\n')

    def refusing_probe(self, stderr):
        def process(argv, **kwargs):
            if '--check-config' in argv:
                return subprocess.CompletedProcess(argv, 1, '', stderr)
            return self.process_without_probe(argv, **kwargs)
        self.process_without_probe = type(self).process.__get__(self)
        return process

    def run_refused(self, stderr):
        self.install_existing(saved='-')
        self.requirements = {'new': 'cdhash H"9a9a"'}
        process = self.refusing_probe(stderr)
        with patch.object(self, 'process', process):
            with self.assertRaises(RuntimeError) as raised:
                self.run_install(None)
        return str(raised.exception)

    def test_the_refusal_sentences_match_the_spec(self):
        self.assertEqual(len(self.install.REFUSAL_SENTENCES), 9)
        self.assertEqual(self.install.REFUSAL_SENTENCES[0], '[telegram] is no longer read: Harold reaches the phone by iMessage only. Remove it from config/local.toml or the HAROLD__ environment variables.')
        self.assertEqual(self.install.REFUSAL_SENTENCES[1], '[peers] is no longer read: two Macs share replies through iMessage, with no hub or member. Remove it from config/local.toml or the HAROLD__ environment variables.')
        self.assertEqual(self.install.REFUSAL_SENTENCES[2], 'notify.telegram is no longer read: the lists are imessage, web_app and machine_audio_output. Remove it from config/local.toml or the HAROLD__ environment variables.')
        self.assertEqual(self.install.REFUSAL_SENTENCES[3], 'notify.away_channel is no longer read: Harold reaches the phone by iMessage only. Remove it from config/local.toml or the HAROLD__ environment variables.')
        self.assertEqual(self.install.REFUSAL_SENTENCES[4], 'grpc.host must be a loopback address (127.0.0.0/8, ::1 or localhost): Harold no longer serves other Macs.')
        self.assertEqual(self.install.REFUSAL_SENTENCES[5], 'notify.imessage is set, but this Harold is built without iMessage: remove it from config/local.toml or the HAROLD__ environment variables.')
        self.assertEqual(self.install.REFUSAL_SENTENCES[6], 'imessage is set, but this Harold is built without iMessage: remove it from config/local.toml or the HAROLD__ environment variables.')
        self.assertEqual(self.install.REFUSAL_SENTENCES[7], 'chat_db is set, but this Harold is built without iMessage: remove it from config/local.toml or the HAROLD__ environment variables.')
        self.assertEqual(self.install.REFUSAL_SENTENCES[8], 'voice_input is set, but this Harold is built without calls: remove it from config/local.toml or the HAROLD__ environment variables.')

    def test_a_refused_config_relays_only_the_fixed_sentences_and_leaves_the_old_harold(self):
        stderr = '\n'.join(['Error: "invalid configuration"', '  ' + self.install.REFUSAL_SENTENCES[0] + '  ',
                            'review-sentinel-secret', self.install.REFUSAL_SENTENCES[0] + ' review-sentinel-secret',
                            self.install.REFUSAL_SENTENCES[4], ''])
        message = self.run_refused(stderr)
        self.assertEqual(message, 'Configuration validation failed. Remove these settings, then run the install again:\n'
                         + self.install.REFUSAL_SENTENCES[0] + '\n' + self.install.REFUSAL_SENTENCES[4])
        self.assertNotIn('sentinel', message)
        self.assertNotIn('sentinel', self.printed.getvalue())
        self.unload.assert_not_called()
        self.start.assert_not_called()
        self.assertEqual((self.bundle / 'harold').read_text(), 'old harold')
        self.assertFalse(list(self.prefix.glob('harold.backup-*')))

    def test_a_failed_probe_without_a_fixed_sentence_gives_the_generic_failure(self):
        message = self.run_refused('Error: "invalid configuration"\nreview-sentinel-secret\n')
        self.assertEqual(message, 'Configuration validation failed; check the supplied TOML and required iMessage fields')
        self.assertNotIn('sentinel', message)
        self.unload.assert_not_called()
        self.assertEqual((self.bundle / 'harold').read_text(), 'old harold')

    def test_a_failed_install_prints_neither(self):
        self.install_existing(saved='Kah Geh Tan')
        self.sign_error = 'Kah Geh Tan: ambiguous (matches "Kah Geh Tan" and "Kah Geh Tan")'
        with self.assertRaisesRegex(RuntimeError, 'ambiguous'):
            self.run_install(None)
        self.assertNotIn('macOS permissions', self.printed.getvalue())
        self.unload.assert_not_called()

    def test_the_backup_keeps_haroldctl_the_dashboard_and_a_rollback_note(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        output = self.run_install('-')
        backup, note, _ = self.rollback()
        self.assertIn('Previous installation archived at {}\n'.format(backup), output)
        self.assertEqual((backup / 'tmx-agent-dash').read_text(), 'old dash')
        self.assertIn('echo "old $*"', (backup / 'haroldctl').read_text())
        self.assertIn('- Version: v0.5.0\n', note)
        self.assertIn('- Revision: 08095dde67ea035ff4e0aaa3109a344d39ceb019\n', note)
        self.assertRegex(note, r'- Replaced: \d{4}-\d\d-\d\d \d\d:\d\d UTC\n')
        self.assertEqual((self.prefix / 'harold' / 'control-tower').read_text(), 'release control-tower')

    REMOVED = 'removed ~/bin/tmx-agent-dash (now harold start-control-tower)'

    def test_an_install_over_the_old_layout_keeps_a_copy_then_removes_the_old_program(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        output = self.run_install('-')
        backup, note, commands = self.rollback()
        self.assertEqual((backup / 'tmx-agent-dash').read_text(), 'old dash')
        self.assertIn('tmx-agent-dash', note)
        self.assertRegex(commands, r'(?m)^mv .*harold/tmx-agent-dash \.$')
        self.assertFalse((self.prefix / 'tmx-agent-dash').exists())
        self.assertTrue((self.prefix / 'harold' / 'control-tower').exists())
        self.assertIn(self.REMOVED, output)

    def test_a_first_install_beside_a_stale_old_program_removes_it(self):
        (self.prefix / 'tmx-agent-dash').write_text('old dash')
        output = self.run_install('-')
        self.assertEqual(list(self.prefix.glob('harold.backup-*')), [])
        self.assertFalse((self.prefix / 'tmx-agent-dash').exists())
        self.assertIn(self.REMOVED, output)

    def test_an_old_program_that_cannot_be_deleted_does_not_stop_the_install(self):
        old = self.prefix / 'tmx-agent-dash'
        old.write_text('old dash')
        real_unlink = Path.unlink

        def unlink(path, *args, **kwargs):
            if path == old:
                raise PermissionError(13, 'Permission denied')
            return real_unlink(path, *args, **kwargs)

        with patch.object(Path, 'unlink', autospec=False, new=unlink):
            output = self.run_install('-')
        self.assertIn('could not remove ~/bin/tmx-agent-dash (Permission denied); delete it by hand', output)
        self.assertIn('Harold ready at', output)
        self.assertNotIn('removed ~/bin', output)
        self.start.assert_called()

    def test_a_second_install_finds_control_tower_in_the_bundle_backup(self):
        self.run_install('-')
        output = self.run_install('-')
        backup, = self.prefix.glob('harold.backup-*')
        self.assertEqual((backup / 'control-tower').read_text(), 'release control-tower')
        self.assertFalse((backup / 'tmx-agent-dash').exists())
        self.assertNotIn('removed', output)

    def test_a_failed_rollback_copy_leaves_the_old_program_in_place(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        with patch.object(self.install, 'keep_rollback_copy', side_effect=OSError(28, 'No space left on device')):
            output = self.run_install('-')
        self.assertEqual((self.prefix / 'tmx-agent-dash').read_text(), 'old dash')
        self.assertNotIn('removed', output)
        self.assertIn('Rollback copy incomplete (No space left on device); the old tmx-agent-dash stays in place',
                      output)
        self.assertIn('Harold ready at', output)

    def test_a_failed_rollback_copy_over_the_new_layout_names_no_old_program(self):
        self.install_existing(saved='-')
        self.assertFalse((self.prefix / 'tmx-agent-dash').exists())
        with patch.object(self.install, 'keep_rollback_copy', side_effect=OSError(28, 'No space left on device')):
            output = self.run_install('-')
        self.assertIn('Rollback copy incomplete (No space left on device); the old bundle is still in the folder below.',
                      output)
        self.assertNotIn('stays in place', output)

    def test_a_symlinked_old_program_is_still_refused(self):
        target = self.prefix.parent / 'elsewhere'
        target.write_text('x')
        (self.prefix / 'tmx-agent-dash').symlink_to(target)
        with self.assertRaisesRegex(RuntimeError, 'Unsafe symlink'):
            self.install.validate_destination(self.prefix)

    def test_the_rollback_commands_put_the_old_install_back(self):
        log = self.prefix.parent / 'haroldctl.log'
        self.install_existing(saved='-')
        self.install_existing_programs(log)
        # The new haroldctl runs the bundle's service.py, which here only writes `new <command>`.
        (self.release / 'scripts/harold_service.py').write_text(
            'import sys\nwith open({!r}, "a") as log:\n'
            '    log.write("new " + " ".join(sys.argv[1:]) + "\\n")\n'.format(str(log)))
        self.run_install('-')
        (self.bundle / 'harold.log').write_text('new log\n')
        self.assertTrue(self.launch_agent().is_file())
        backup, _, commands = self.rollback()
        result = self.run_rollback(commands, log)
        self.assertEqual(result.returncode, 0, result.stderr)
        # The new stop disables the label; the older haroldctl's start cannot load a disabled job.
        label = self.installed_metadata()['label']
        self.assertEqual(log.read_text().splitlines(), [
            'new stop', 'launchctl enable gui/{}/{}'.format(os.getuid(), label), 'old start'])
        # The older bundle does not start at login, so its LaunchAgents copy is gone.
        self.assertFalse(self.launch_agent().exists())
        self.assertEqual((self.bundle / 'harold').read_text(), 'old harold')
        self.assertEqual((self.prefix / 'tmx-agent-dash').read_text(), 'old dash')
        self.assertIn('echo "old $*"', (self.prefix / 'haroldctl').read_text())
        self.assertEqual((self.bundle / 'data/events/marker').read_text(), 'history')
        self.assertEqual((self.bundle / 'harold.log').read_text(), 'new log\n')
        self.assertFalse((self.bundle / 'haroldctl').exists())
        self.assertFalse(backup.exists())
        replaced, = self.prefix.glob('harold.replaced-*')
        self.assertEqual((replaced / 'harold').read_text(), 'release harold')

    def run_rollback(self, commands, log):
        """Runs the rollback commands with a fake launchctl that writes `launchctl <args>` to `log`."""
        fake = self.prefix.parent / 'fake-bin'
        fake.mkdir(exist_ok=True)
        (fake / 'launchctl').write_text('#!/bin/sh\necho "launchctl $*" >> {}\n'.format(shlex.quote(str(log))))
        (fake / 'launchctl').chmod(0o755)
        env = dict(os.environ, PATH='{}:{}'.format(fake, os.environ['PATH']))
        return subprocess.run(['sh', '-e', '-c', commands], capture_output=True, text=True, env=env)

    def test_rolling_back_to_a_bundle_that_starts_at_login_puts_its_plist_back(self):
        log = self.prefix.parent / 'haroldctl.log'
        self.install_existing(saved='-')
        self.install_existing_programs(log)
        old = self.install.make_metadata(self.prefix, '127.0.0.1:50123')
        self.install.write_plist(self.bundle / 'service.plist', old)
        old_plist = (self.bundle / 'service.plist').read_bytes()
        (self.release / 'scripts/harold_service.py').write_text(
            'import sys\nwith open({!r}, "a") as log:\n'
            '    log.write("new " + " ".join(sys.argv[1:]) + "\\n")\n'.format(str(log)))
        self.run_install('-')
        (self.bundle / 'harold.log').write_text('new log\n')
        self.launch_agent().write_text('the newer copy')
        _, _, commands = self.rollback()
        result = self.run_rollback(commands, log)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.launch_agent().read_bytes(), old_plist)
        self.assertEqual(log.read_text().splitlines()[-1], 'old start')

    def test_a_reinstall_rollback_leaves_the_new_data_aside(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        self.run_install('-', reinstall=True)
        backup, note, commands = self.rollback()
        self.assertEqual((backup / 'data/events/marker').read_text(), 'history')
        self.assertNotIn('/data harold/data', commands)
        self.assertNotIn('harold.log', commands)

    TIPS_STEP = 'rm -f harold/data/pod/tips'
    TIPS_SENTENCE = ('The commands delete `harold/data/pod/tips`, the counted-tip file. A Harold built before '
                     '36a7c94 counts merges without moving it, so a later upgrade would count them again. '
                     'Deleting it is safe.')

    def test_the_rollback_note_deletes_the_counted_tip_file(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        self.run_install('-')
        _, note, commands = self.rollback()
        lines = commands.splitlines()
        # The backup holds a copy of the data, so the log is the last thing that moves back.
        log_back, = [i for i, line in enumerate(lines)
                     if line.startswith('mv harold.replaced-') and line.endswith('/harold.log harold/harold.log')]
        self.assertGreater(lines.index(self.TIPS_STEP), log_back)
        self.assertLess(lines.index(self.TIPS_STEP), lines.index('./haroldctl start'))
        self.assertIn(self.TIPS_SENTENCE, note)

    def test_a_reinstall_rollback_note_deletes_the_counted_tip_file(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        self.run_install('-', reinstall=True)
        backup, note, commands = self.rollback()
        lines = commands.splitlines()
        self.assertGreater(lines.index(self.TIPS_STEP), lines.index('mv {} harold'.format(backup.name)))
        self.assertLess(lines.index(self.TIPS_STEP), lines.index('./haroldctl start'))
        self.assertIn(self.TIPS_SENTENCE, note)

    def test_the_rollback_commands_remove_the_counted_tip_file(self):
        log = self.prefix.parent / 'haroldctl.log'
        self.install_existing(saved='-')
        self.install_existing_programs(log)
        (self.release / 'scripts/harold_service.py').write_text('')
        self.run_install('-')
        # The new install's data moves back into the restored bundle, tips file and all.
        (self.bundle / 'harold.log').write_text('new log\n')
        (self.bundle / 'data/pod').mkdir(parents=True)
        (self.bundle / 'data/pod/tips').write_text('abc123\n')
        _, _, commands = self.rollback()
        result = self.run_rollback(commands, log)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.bundle / 'data/pod/tips').exists())

    def test_a_first_install_creates_no_backup_folder(self):
        output = self.run_install('-')
        self.assertEqual(list(self.prefix.glob('harold.backup-*')), [])
        self.assertNotIn('Previous installation archived', output)

    def old_install_with_store(self, saved='-'):
        self.install_existing(saved=saved)
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        (self.bundle / 'data/store').mkdir(parents=True)
        (self.bundle / 'data/store/state.db').write_bytes(b'store-bytes' * 100)

    def test_an_install_over_an_old_bundle_copies_the_data_into_the_backup(self):
        self.old_install_with_store()
        output = self.run_install('-')
        backup, note, commands = self.rollback()
        self.assertEqual((backup / 'data/store/state.db').read_bytes(), b'store-bytes' * 100)
        self.assertEqual((backup / 'data/events/marker').read_text(), 'history')
        self.assertEqual((backup / 'data/events/harold/main/catalog.db').read_bytes(), bytes(range(256)) * 64)
        self.assertEqual((self.bundle / 'data/store/state.db').read_bytes(), b'store-bytes' * 100)
        self.assertIn('copied data to {}/data (0.0 MB)\n'.format(self.install.shown(backup)), output)
        self.assertIn('{}/data'.format(backup.name.replace('harold.backup-', 'harold.replaced-')), note)
        self.assertIn('Replies and events from after the install are lost.', note)
        self.assertNotIn('/data harold/data', commands)
        self.assertIn('/harold.log harold/harold.log', commands)

    def test_a_first_install_copies_no_data(self):
        output = self.run_install('-')
        self.assertNotIn('copied data', output)
        self.assertNotIn('data copy', output)
        self.assertEqual(list(self.prefix.glob('harold.backup-*')), [])

    def test_a_reinstall_copies_no_data_and_keeps_the_old_note(self):
        self.old_install_with_store()
        output = self.run_install('-', reinstall=True)
        backup, note, _ = self.rollback()
        self.assertNotIn('copied data', output)
        self.assertEqual((backup / 'data/store/state.db').read_bytes(), b'store-bytes' * 100)
        self.assertIn('data from before the reinstall', note)
        self.assertNotIn('Replies and events from after the install are lost.', note)

    def stopped_install(self, start_error=None):
        """Run an install over an old bundle with a store, with the data copy failing. Returns the exit code.
        The old service.json records another signing identity, so it differs from the new metadata."""
        self.old_install_with_store(saved='old-identity')
        self.old_service_json = (self.bundle / 'service.json').read_text()
        with patch.object(self.install, 'copy_data_to_backup', side_effect=OSError(28, 'No space left on device')):
            with self.assertRaises(SystemExit) as raised:
                self.run_install('-', start_error=start_error)
        return raised.exception.code

    def assert_old_harold_untouched(self):
        self.assertEqual((self.bundle / 'harold').read_text(), 'old harold')
        self.assertEqual((self.bundle / 'data/store/state.db').read_bytes(), b'store-bytes' * 100)
        self.assertEqual((self.bundle / 'data/events/marker').read_text(), 'history')
        self.assertEqual(list(self.prefix.glob('harold.backup-*')), [])
        self.assertEqual(list(self.prefix.glob('.harold-stage-*')), [])
        self.assertIn('echo "old $*"', (self.prefix / 'haroldctl').read_text())
        self.assertEqual((self.prefix / 'tmx-agent-dash').read_text(), 'old dash')

    def test_a_failed_data_copy_stops_the_install_and_starts_the_old_harold(self):
        code = self.stopped_install()
        output = self.printed.getvalue()
        self.assertEqual(code, 1)
        self.assertIn('Install stopped: the data copy failed (No space left on device). '
                      'Nothing changed: the old Harold runs again with its data. '
                      'Free the space or fix the cause, then install again.', output)
        self.assertNotIn('Harold ready', output)
        self.assert_old_harold_untouched()
        self.start.assert_called_once()
        self.assertEqual(self.start.call_args.args[0], json.loads(self.old_service_json))
        self.assertEqual(self.start.call_args.args[0]['signing_identity'], 'old-identity')
        self.assertEqual(self.order[-1], ('start', False))
        self.assertFalse(list(self.home.rglob('*.plist')))
        self.assertEqual(json.loads((self.bundle / 'service.json').read_text())['bundle'], str(self.bundle))

    def test_a_failed_data_copy_and_a_failed_old_start_name_both_problems(self):
        code = self.stopped_install(start_error=RuntimeError('boom'))
        output = self.printed.getvalue()
        self.assertEqual(code, 1)
        self.assertIn('Install stopped: the data copy failed (No space left on device).', output)
        self.assertIn('The old Harold did not start: boom. Start it with haroldctl start.', output)
        self.assertNotIn('Harold ready', output)
        self.assert_old_harold_untouched()

    def test_main_exits_with_code_1_after_a_failed_data_copy_and_removes_the_staging_folder(self):
        self.old_install_with_store()
        argv = ['install.py', '--prefix', str(self.prefix), '--config', str(self.config),
                '--signing-identity', '-', '--prebuilt-dir', str(self.release)]
        self.printed = io.StringIO()
        with patch.object(sys, 'argv', argv), \
             patch.object(self.install, 'prerequisites'), \
             patch.object(subprocess, 'run', side_effect=self.process), \
             patch.object(self.install.service, 'listener_pids', return_value=set()), \
             patch.object(self.install.service, 'unload', side_effect=self.unloaded), \
             patch.object(self.install.service, 'stop_unmanaged'), \
             patch.object(self.install.service, 'start', side_effect=self.started) as start, \
             patch.object(self.install, 'copy_data_to_backup', side_effect=OSError(28, 'No space left on device')), \
             contextlib.redirect_stdout(self.printed):
            with self.assertRaises(SystemExit) as raised:
                self.install.main()
        self.assertEqual(raised.exception.code, 1)
        self.assertIn('Install stopped: the data copy failed (No space left on device).', self.printed.getvalue())
        start.assert_called_once()
        self.assertEqual(list(self.prefix.glob('.harold-stage-*')), [])
        self.assert_old_harold_untouched()

    def test_the_rollback_note_moves_the_other_data_entries_back(self):
        self.old_install_with_store()
        for path in ('data/pod/tips', 'data/tools/x.db', 'data/my dir/y', 'data/restart-reminder.json'):
            (self.bundle / path).parent.mkdir(parents=True, exist_ok=True)
            (self.bundle / path).write_text('x')
        self.run_install('-')
        backup, note, commands = self.rollback()
        aside = backup.name.replace('harold.backup-', 'harold.replaced-')
        lines = commands.splitlines()
        for name in ('pod', 'tools', 'my dir', 'restart-reminder.json'):
            self.assertIn('mv {}/data/{} harold/data/{}'.format(aside, shlex.quote(name), shlex.quote(name)), lines)
        self.assertNotIn('mv {}/data/store harold/data/store'.format(aside), lines)
        self.assertNotIn('mv {}/data/events harold/data/events'.format(aside), lines)
        self.assertLess(lines.index('mv {}/data/pod harold/data/pod'.format(aside)), lines.index(self.TIPS_STEP))
        self.assertIn('The new store and events stay in {}/data.'.format(aside), note)
        self.assertIn('Replies and events from after the install are lost.', note)

    def test_an_install_from_before_versions_says_they_were_not_recorded(self):
        self.install_existing(saved='-')
        self.run_install('-')
        _, note, commands = self.rollback()
        self.assertIn('- Version: not recorded\n', note)
        self.assertIn('- Revision: not recorded\n', note)
        # No haroldctl or old control-tower program was installed, so none is moved back.
        self.assertNotIn('harold/haroldctl', commands)

    def test_a_bundle_started_through_python_names_that_python(self):
        self.install_existing(saved='-')
        (self.bundle / 'service.plist').write_bytes(plistlib.dumps({'ProgramArguments': [
            '/opt/homebrew/bin/python3.13', str(self.bundle / 'service.py'), 'run']}))
        self.run_install('-')
        _, note, _ = self.rollback()
        self.assertIn('Full Disk Access and Messages control must be on for /opt/homebrew/bin/python3.13', note)

    def test_a_bundle_started_directly_needs_no_python_line(self):
        self.install_existing(saved='-')
        (self.bundle / 'service.plist').write_bytes(plistlib.dumps({'ProgramArguments': [
            str(self.bundle / 'harold')]}))
        self.run_install('-')
        _, note, _ = self.rollback()
        self.assertNotIn('Python', note)

    def test_a_failed_rollback_copy_still_starts_harold(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        with patch.object(self.install, 'keep_rollback_copy',
                          side_effect=OSError(28, 'No space left on device')):
            output = self.run_install('-')
        self.assertIn('Rollback copy incomplete (No space left on device)', output)
        self.assertIn('Harold ready at', output)
        self.assertEqual((self.prefix / 'harold' / 'control-tower').read_text(), 'release control-tower')

    def test_any_rollback_copy_failure_still_starts_harold(self):
        self.install_existing(saved='-')
        self.install_existing_programs(self.prefix.parent / 'haroldctl.log')
        with patch.object(self.install, 'keep_rollback_copy', side_effect=RuntimeError('boom')):
            output = self.run_install('-')
        self.assertIn('Rollback copy incomplete (boom)', output)
        self.assertIn('Harold ready at', output)
        self.assertEqual((self.prefix / 'harold' / 'control-tower').read_text(), 'release control-tower')

    def test_darwin_install_is_unchanged(self):
        # The other tests in this class are the proof for the Mac steps. Here: no systemd on darwin.
        self.install_existing(saved='-')
        self.run_install('-')
        self.assertFalse([call for call in self.calls if call and call[0] in ('systemctl', 'loginctl', 'rex')],
                         self.calls)
        self.assertFalse((self.home / '.config/systemd').exists())
        self.assertIn('label', self.installed_metadata())


class FakeLinux:
    """Linux for an install: `sys.platform` is 'linux' and PATH holds only fake programs.

    The fakes `rex`, `grpcurl`, `curl`, `lsof`, `systemctl` and `loginctl` write `<name> <args>`
    to `state/calls`. The stubs for programs that a Linux install must never run (`pip`, `pip3`,
    `nvidia-smi`, `codesign`, `launchctl`, `plutil`, and `loginctl enable-linger`) write the
    marker `state/called-<name>` and exit 99. `systemctl --user start` writes PID to
    `state/running` and `stop` empties it; `lsof` shows a listener only while it is running.
    A flag file in `state` changes a fake: `no-rex-server`, `bus-down`, `linger-off`.
    The context can be entered more than once; the state stays."""

    FORBIDDEN = ('pip', 'pip3', 'nvidia-smi', 'codesign', 'launchctl', 'plutil')
    PID = 4242

    def __init__(self, root, bundle, missing=()):
        self.bin = root / 'fake bin'
        self.state = root / 'fake state'
        self.bundle = bundle
        self.missing = set(missing)

    def write(self, name, body):
        if name in self.missing:
            return
        path = self.bin / name
        path.write_text('#!/bin/sh\nD={}\necho "{} $*" >> "$D/calls"\n'.format(
            shlex.quote(str(self.state)), name) + body)
        path.chmod(0o755)

    def flag(self, name):
        self.state.mkdir(exist_ok=True)
        (self.state / name).write_text('')

    def calls(self, program=None):
        try:
            lines = (self.state / 'calls').read_text().splitlines()
        except FileNotFoundError:
            return []
        return [line for line in lines if program is None or line.split(' ', 1)[0] == program]

    def markers(self):
        return sorted(path.name for path in self.state.glob('called-*'))

    def __enter__(self):
        self.bin.mkdir(exist_ok=True)
        self.state.mkdir(exist_ok=True)
        for name in self.FORBIDDEN:
            self.write(name, 'echo "$*" > "$D/called-{}"\nexit 99\n'.format(name))
        self.write('rex', 'if [ -f "$D/no-rex-server" ]; then echo "rex: no server" >&2; exit 1; fi\n'
                          'echo omarchy\n')
        self.write('grpcurl', 'exit 0\n')
        self.write('curl', 'exit 0\n')
        self.write('systemctl', '\n'.join([
            'if [ -f "$D/bus-down" ]; then echo "Failed to connect to bus: No medium found" >&2; exit 1; fi',
            'case "$2" in',
            '  start) echo {} > "$D/running" ;;'.format(self.PID),
            '  stop) : > "$D/running" ;;',
            '  show) if [ -s "$D/running" ]; then read pid < "$D/running"; echo "MainPID=$pid";'
            ' else echo "MainPID=0"; fi ;;',
            '  is-enabled) echo enabled ;;',
            'esac', 'exit 0', '']))
        self.write('lsof', '\n'.join([
            'case "$*" in',
            '  *-iTCP:*) if [ -s "$D/running" ]; then read pid < "$D/running"; echo "p$pid"; exit 0; fi; exit 1 ;;',
            '  *"-d txt"*) echo n{} ;;'.format(shlex.quote(str(self.bundle / 'harold'))),
            '  *) exit 1 ;;',
            'esac', '']))
        self.write('loginctl', '\n'.join([
            'case "$1" in',
            '  enable-linger) echo "$*" > "$D/called-enable-linger"; exit 99 ;;',
            '  show-user) if [ -f "$D/linger-off" ]; then echo no; else echo yes; fi ;;',
            'esac', '']))
        self.stack = contextlib.ExitStack()
        self.stack.enter_context(patch.dict('os.environ', {'PATH': str(self.bin)}))
        self.stack.enter_context(patch.object(sys, 'platform', 'linux'))
        # The fake PID is not a real Harold: nothing may signal it.
        self.stack.enter_context(patch.object(os, 'kill', side_effect=AssertionError('os.kill called')))
        return self

    def __exit__(self, *exc):
        self.stack.close()
        return False


class LinuxInstallTests(unittest.TestCase):
    """A whole Linux install from a release fixture, with the programs it runs faked by FakeLinux."""

    def setUp(self):
        import install
        self.install = install
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.prefix = self.root / 'prefix with spaces'
        self.prefix.mkdir()
        self.bundle = self.prefix / 'harold'
        self.settings = self.root / 'claude/settings.json'
        self.settings.parent.mkdir()
        self.settings.write_text('{}')
        for name, value in (('CLAUDE_SETTINGS', self.settings), ('AGENT_SKILL_FOLDERS', ())):
            patcher = patch.object(install, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.home = self.root / 'home'
        patcher = patch.dict('os.environ', {'HOME': str(self.home)})
        patcher.start()
        self.addCleanup(patcher.stop)
        self.release = self.root / 'release'
        for name in ('control-tower', 'harold.proto',
                     'hooks/harold_turn_complete.py', 'hooks/claude_turn_complete.py',
                     'hooks/codex_turn_complete.py', 'scripts/harold_service.py',
                     'config/default.toml', 'config/local.template.toml', SKILL_FILE,
                     *('hooks/claude-mod/' + f for f in RUNTIME_FILES)):
            path = self.release / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('release ' + name)
        self.harold_log = self.root / 'harold-calls'
        prefix = shlex.quote(str(self.prefix))
        (self.release / 'harold').write_text('\n'.join([
            '#!/bin/sh',
            'LOG={}'.format(shlex.quote(str(self.harold_log))),
            'case "$1" in',
            '  --build-features)',
            '    copied=no',
            '    for f in {0}/harold {0}/.harold-stage-*; do [ -e "$f" ] && copied=yes; done'.format(prefix),
            '    echo "build-features copied=$copied" >> "$LOG"',
            '    echo agent-monitor; echo rex ;;',
            '  --check-config)',
            '    echo check-config >> "$LOG"',
            '    printf \'{"grpc_addr": "127.0.0.1:50123", "store_path": "%s"}\\n\' "$HAROLD__STORE__PATH" ;;',
            '  --check-ready) echo \'{"ready": true}\' ;;',
            '  *) exit 2 ;;',
            'esac', '']))
        for name in ('harold', 'control-tower'):
            (self.release / name).chmod(0o755)
        self.fake = FakeLinux(self.root, self.bundle)
        self.unit = self.home / '.config/systemd/user/harold.service'
        self.addCleanup(self.assert_no_forbidden_program_ran)

    def assert_no_forbidden_program_ran(self):
        if self.fake.state.is_dir():
            self.assertEqual(self.fake.markers(), [])

    def args(self, config=None, reinstall=False):
        return argparse.Namespace(prefix=str(self.prefix), config=config, reinstall=reinstall,
                                  offline=False, signing_identity=None, prebuilt_dir=str(self.release))

    def run_install(self, config=None, reinstall=False):
        self.printed = io.StringIO()
        with self.fake, contextlib.redirect_stdout(self.printed):
            self.install.install(self.args(config, reinstall), self.root / 'no source')
        return self.printed.getvalue()

    def run_main(self, *extra):
        """Runs main() as the command line does. Returns (exit code, stdout, stderr)."""
        out, err = io.StringIO(), io.StringIO()
        argv = ['install.py', '--prefix', str(self.prefix), '--prebuilt-dir', str(self.release), *extra]
        with self.fake, patch.object(sys, 'argv', argv), \
             contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = self.install.main()
        return code, out.getvalue(), err.getvalue()

    def rex(self):
        return str(self.fake.bin / 'rex')

    def local_toml(self):
        return (self.bundle / 'config/local.toml').read_text()

    def assert_nothing_copied(self):
        self.assertFalse(self.bundle.exists())
        self.assertEqual(list(self.prefix.glob('.harold-stage-*')), [])
        self.assertFalse(self.unit.exists())

    def test_features_read_from_the_package_binary_before_copy(self):
        output = self.run_install()
        log = self.harold_log.read_text().splitlines()
        self.assertEqual(log[0], 'build-features copied=no')
        self.assertLess(log.index('build-features copied=no'), log.index('check-config'))
        self.assertIn('Harold build features: agent-monitor, rex\n', output)

    def test_package_features_reads_one_name_per_line(self):
        with self.fake:
            self.assertEqual(self.install.package_features(self.release / 'harold'), ['agent-monitor', 'rex'])

    def test_missing_rex_stops_with_a_clear_message(self):
        self.fake.missing.add('rex')
        with self.fake:
            with self.assertRaisesRegex(RuntimeError, 'Rex not found'):
                self.install.find_rex()
        with self.assertRaisesRegex(RuntimeError, 'Missing tools: rex'):
            self.run_install()
        self.assert_nothing_copied()

    def test_no_rex_server_stops_nothing_copied(self):
        self.fake.flag('no-rex-server')
        with self.assertRaises(RuntimeError) as raised:
            self.run_install()
        self.assertIn("omarchy's agents run in Rex", str(raised.exception))
        self.assertIn('no Rex server answers', str(raised.exception))
        self.assertIn('rex --autostart=false whoami', self.fake.calls('rex'))
        self.assert_nothing_copied()

    def test_config_has_the_four_settings(self):
        output = self.run_install()
        text = self.local_toml()
        for line in ('[agent_monitor]', 'multiplexer = "rex"', 'rex = {}'.format(json.dumps(self.rex())),
                     '[tts]', 'command = ""', '[telemetry]', 'export = false'):
            self.assertEqual(text.splitlines().count(line), 1, (line, text))
        self.assertNotIn('already in', output)
        try:
            import tomllib
        except ImportError:  # Python before 3.11
            return
        self.assertEqual(tomllib.loads(text), {
            'agent_monitor': {'multiplexer': 'rex', 'rex': self.rex()},
            'tts': {'command': ''}, 'telemetry': {'export': False}})

    def test_existing_tables_are_not_repeated(self):
        config = self.root / 'import.toml'
        config.write_text('[tts]\ncommand = "x"\n')
        output = self.run_install(config=str(config))
        text = self.local_toml()
        self.assertEqual(text.count('[tts]'), 1, text)
        self.assertIn('command = "x"', text)
        self.assertNotIn('command = ""', text)
        self.assertEqual(text.count('[agent_monitor]'), 1, text)
        self.assertEqual(text.count('[telemetry]'), 1, text)
        # One "left as written" line and one warning, because "x" is not the value Linux needs.
        self.assertEqual(output.count('[tts]'), 2, output)
        self.assertIn('[tts] is already in the configuration', output)
        self.assertIn('Warning: [tts] command', output)

    def kept(self, base, rex='/usr/bin/rex'):
        """linux_config of `base`: (the text, the printed lines)."""
        printed = io.StringIO()
        with contextlib.redirect_stdout(printed):
            text = self.install.linux_config(base.encode(), rex).decode()
        return text, printed.getvalue().splitlines()

    def warnings(self, lines):
        return [line for line in lines if line.startswith('Warning: ')]

    def test_a_kept_table_without_the_needed_key_warns(self):
        text, lines = self.kept('[agent_monitor]\nrex = "/usr/bin/rex"\n')
        self.assertEqual(self.warnings(lines), [
            'Warning: [agent_monitor] multiplexer is not set to the value Harold on Linux needs: '
            'multiplexer = "rex". Edit config/local.toml.'])
        self.assertIn('[agent_monitor] is already in the configuration; left as written.', lines)
        self.assertEqual(text.count('[agent_monitor]'), 1, text)

    def test_a_kept_table_with_another_value_warns(self):
        for base, needed in (('[agent_monitor]\nmultiplexer = "tmux"\nrex = "/usr/bin/rex"\n', 'multiplexer = "rex"'),
                             ('[agent_monitor]\nmultiplexer = "rex"\nrex = "/old/rex"\n', 'rex = "/usr/bin/rex"'),
                             ('[tts]\ncommand = "x"\n', 'command = ""'),
                             ('[telemetry]\nexport = true\n', 'export = false'),
                             ('tts.command = "say"\n', 'command = ""')):
            with self.subTest(base=base):
                text, lines = self.kept(base)
                warnings = self.warnings(lines)
                self.assertEqual(len(warnings), 1, lines)
                self.assertIn(needed, warnings[0])
                self.assertTrue(text.startswith(base), text)

    def test_a_kept_table_with_the_needed_values_prints_no_warning(self):
        base = ('[agent_monitor]\nmultiplexer = "rex"  # comment\nrex = \'/usr/bin/rex\'\n'
                '[tts]\n command="" \n[telemetry]\nexport = false\n')
        text, lines = self.kept(base)
        self.assertEqual(self.warnings(lines), [])
        self.assertEqual(len(lines), 3, lines)
        self.assertEqual(text, base)
        for name in ('agent_monitor', 'tts', 'telemetry'):
            self.assertEqual(text.count('[{}]'.format(name)), 1, text)

    def test_a_key_in_another_table_does_not_count(self):
        _, lines = self.kept('[tts]\n[notify]\ncommand = ""\n')
        self.assertIn('command = ""', ' '.join(self.warnings(lines)))

    def test_each_existing_table_is_kept_as_the_user_wrote_it(self):
        rex = '/opt/my rex/rex'
        for name, table in (('agent_monitor', '[agent_monitor]\nmultiplexer = "tmux"\n'),
                            ('tts', '[ tts ]\ncommand = "say"\n'),
                            ('telemetry', '[telemetry]\nexport = true\n'),
                            ('tts', '[tts.voices]\nfast = "x"\n'),
                            ('tts', 'tts.command = "say"\n[notify]\nmachines = []\n')):
            with self.subTest(table=table):
                printed = io.StringIO()
                with contextlib.redirect_stdout(printed):
                    text = self.install.linux_config(table.encode(), rex).decode()
                self.assertTrue(text.startswith(table), text)
                self.assertEqual(text.count('[{}]'.format(name)), 1 if '[{}]'.format(name) in table else 0, text)
                # Each of these tables lacks a value Linux needs, so a warning follows the line.
                lines = printed.getvalue().splitlines()
                self.assertEqual(lines[0], '[{}] is already in the configuration; left as written.'.format(name))
                self.assertTrue(lines[1:], lines)
                self.assertTrue(all(line.startswith('Warning: [{}] '.format(name)) for line in lines[1:]), lines)
                try:
                    import tomllib
                except ImportError:  # Python before 3.11
                    continue
                parsed = tomllib.loads(text)
                self.assertEqual(sorted(parsed) if 'notify' not in parsed else sorted(set(parsed) - {'notify'}),
                                 ['agent_monitor', 'telemetry', 'tts'])

    def test_a_commented_or_nested_header_does_not_count_as_the_table(self):
        base = b'# [tts]\n[notify]\ntts = true\n[agent_monitor.extra]\n'
        printed = io.StringIO()
        with contextlib.redirect_stdout(printed):
            text = self.install.linux_config(base, '/usr/bin/rex').decode()
        self.assertIn('\n[tts]\ncommand = ""\n', text)
        self.assertIn('[agent_monitor] is already in the configuration', printed.getvalue())

    def test_a_config_without_a_final_newline_gets_one_before_the_tables(self):
        text = self.install.linux_config(b'[notify]\nmachines = []', '/usr/bin/rex').decode()
        self.assertTrue(text.startswith('[notify]\nmachines = []\n\n[agent_monitor]\n'), text)

    def test_no_question_is_asked_on_linux(self):
        with patch.object(sys, 'platform', 'linux'), patch('sys.stdin.isatty', return_value=True), \
             patch('builtins.input', side_effect=AssertionError('asked')):
            self.assertEqual(self.install.choose_config(None, self.bundle), b'')

    def test_unit_file_values(self):
        self.run_install()
        lines = self.unit.read_text().splitlines()
        bundle = str(self.bundle)
        for line in ('Restart=on-failure', 'RestartSec=30', 'StartLimitIntervalSec=600', 'StartLimitBurst=5',
                     'Environment=HAROLD_LAUNCHD=1', 'WantedBy=default.target',
                     # The bundle path has a space: ExecStart quotes it; the path settings take it whole.
                     'ExecStart="{}/harold"'.format(bundle),
                     'WorkingDirectory={}'.format(bundle),
                     'StandardOutput=append:{}/harold.log'.format(bundle),
                     'StandardError=append:{}/harold.log'.format(bundle)):
            self.assertIn(line, lines)
        path, = [line for line in lines if line.startswith('Environment="PATH=')]
        # rex's folder ("fake bin") has a space, so the whole assignment is quoted.
        self.assertEqual(path, 'Environment="PATH={}:{}/.local/bin:/usr/local/bin:/usr/bin"'.format(
            self.fake.bin, self.home))
        metadata = json.loads((self.bundle / 'service.json').read_text())
        self.assertTrue(metadata['env']['PATH'].startswith(str(self.fake.bin) + ':'))
        self.assertEqual(self.unit.stat().st_mode & 0o777, 0o644)
        self.assertEqual([p.name for p in self.unit.parent.iterdir()], ['harold.service'])

    def test_unit_values_are_escaped_by_systemd_rules(self):
        metadata = self.install.make_linux_metadata(Path('/x/50% "a" b\\c'), '', '/r $x/rex')
        path = self.root / 'unit/harold.service'
        self.install.write_unit(path, metadata)
        lines = path.read_text().splitlines()
        self.assertIn('ExecStart="/x/50%% \\"a\\" b\\\\c/harold/harold"', lines)
        self.assertIn('WorkingDirectory=/x/50%% "a" b\\c/harold', lines)
        path_line, = [line for line in lines if line.startswith('Environment="PATH=')]
        self.assertTrue(path_line.startswith('Environment="PATH=/r $x:'), path_line)

    def test_a_rex_folder_that_cannot_go_in_path_is_refused(self):
        for rex in ('/a:b/rex', '/a\nb/rex'):
            with self.subTest(rex=rex):
                with self.assertRaisesRegex(RuntimeError, 'rex'):
                    self.install.make_linux_metadata(self.prefix, '', rex)

    def test_systemctl_sequence(self):
        self.run_install()
        calls = self.fake.calls('systemctl')
        self.assertTrue(calls)
        for call in calls:
            self.assertEqual(call.split()[1], '--user', call)
        reload = len(calls) - 1 - calls[::-1].index('systemctl --user daemon-reload')
        enable = calls.index('systemctl --user enable harold.service')
        start = calls.index('systemctl --user start harold.service')
        self.assertLess(reload, enable)
        self.assertLess(enable, start)

    def test_reinstall_over_a_running_unit_stops_it_first_then_starts_the_new_one(self):
        self.run_install()
        (self.fake.state / 'calls').write_text('')
        output = self.run_install()
        calls = self.fake.calls('systemctl')
        stop = calls.index('systemctl --user stop harold.service')
        reload = len(calls) - 1 - calls[::-1].index('systemctl --user daemon-reload')
        self.assertLess(stop, reload)
        self.assertLess(reload, calls.index('systemctl --user enable harold.service'))
        self.assertLess(calls.index('systemctl --user enable harold.service'),
                        calls.index('systemctl --user start harold.service'))
        self.assertIn('Harold ready at 127.0.0.1:50123 (systemd PID 4242)', output)
        backup, = self.prefix.glob('harold.backup-*')
        note = (backup / 'ROLLBACK.md').read_text()
        self.assertIn('systemctl --user enable harold\n./haroldctl start\n', note)
        self.assertNotIn('launchctl', note)
        self.assertNotIn('LaunchAgents', note)

    def test_no_tts_environment_and_no_gpu_probe(self):
        self.run_install()
        for name in ('pip', 'pip3', 'nvidia-smi'):
            self.assertFalse((self.fake.state / ('called-' + name)).exists(), name)
            self.assertEqual(self.fake.calls(name), [])
        self.assertFalse((self.bundle / 'tts').exists())

    def test_no_mac_tools_called(self):
        self.run_install()
        for name in ('codesign', 'launchctl', 'plutil'):
            self.assertFalse((self.fake.state / ('called-' + name)).exists(), name)
        self.assertFalse((self.bundle / 'service.plist').exists())
        self.assertFalse((self.home / 'Library').exists())

    def test_linger_off_prints_a_warning_and_does_not_enable_it(self):
        self.fake.flag('linger-off')
        output = self.run_install()
        warning = [line for line in output.splitlines() if 'linger' in line.lower()]
        self.assertEqual(len(warning), 1, output)
        self.assertTrue(warning[0].startswith('Warning: '), warning)
        self.assertIn('loginctl enable-linger', warning[0])
        self.assertTrue(output.endswith(warning[0] + '\n'), output)
        self.assertEqual(self.fake.calls('loginctl'),
                         ['loginctl show-user {} -p Linger --value'.format(os.getuid())])
        self.assertFalse((self.fake.state / 'called-enable-linger').exists())

    def test_linger_on_prints_nothing(self):
        output = self.run_install()
        self.assertNotIn('linger', output.lower())

    def test_systemctl_user_manager_missing_stops_clearly(self):
        self.fake.flag('bus-down')
        code, out, err = self.run_main()
        self.assertEqual(code, 1)
        self.assertIn('Failed to connect to bus', err)
        self.assertIn('systemctl --user daemon-reload', err)
        self.assertNotIn('Traceback', err)
        self.assert_nothing_copied()

    def test_systemctl_calls_have_a_timeout(self):
        seen = []

        def run(argv, **kwargs):
            seen.append((list(argv), kwargs.get('timeout')))
            return subprocess.CompletedProcess(argv, 0, '', '')

        with patch.object(subprocess, 'run', side_effect=run):
            self.install.check_service_manager()
        self.assertEqual(seen, [(['systemctl', '--user', 'daemon-reload'], 10)])

    def test_reinstall_restarts_old_harold_when_data_copy_fails(self):
        self.run_install()
        (self.bundle / 'data/store/state.db').write_bytes(b'store-bytes')
        old_harold = (self.bundle / 'harold').read_bytes()
        (self.fake.state / 'calls').write_text('')
        with patch.object(self.install, 'copy_data_to_backup', side_effect=OSError(28, 'No space left on device')):
            with self.assertRaises(SystemExit) as raised:
                self.run_install()
        self.assertEqual(raised.exception.code, 1)
        output = self.printed.getvalue()
        self.assertIn('Install stopped: the data copy failed (No space left on device).', output)
        self.assertNotIn('Harold ready', output)
        calls = self.fake.calls('systemctl')
        stop = calls.index('systemctl --user stop harold.service')
        self.assertLess(stop, calls.index('systemctl --user enable harold.service'))
        self.assertLess(calls.index('systemctl --user enable harold.service'),
                        calls.index('systemctl --user start harold.service'))
        self.assertEqual((self.fake.state / 'running').read_text().strip(), str(FakeLinux.PID))
        self.assertEqual((self.bundle / 'data/store/state.db').read_bytes(), b'store-bytes')
        self.assertEqual((self.bundle / 'harold').read_bytes(), old_harold)
        self.assertEqual(list(self.prefix.glob('harold.backup-*')), [])
        self.assertEqual(list(self.prefix.glob('.harold-stage-*')), [])

    def test_unit_has_no_launchd_label_and_hooks_are_installed(self):
        output = self.run_install()
        metadata = json.loads((self.bundle / 'service.json').read_text())
        self.assertNotIn('label', metadata)
        self.assertNotIn('plist', metadata)
        self.assertNotIn('launch_agent', metadata)
        self.assertEqual(metadata['unit'], 'harold.service')
        self.assertEqual(metadata['unit_file'], str(self.unit))
        self.assertEqual(metadata['grpc_addr'], '127.0.0.1:50123')
        self.assertEqual(sorted(metadata['env']), sorted([
            'HAROLD_CONFIG_DIR', 'HAROLD_ENV', 'HAROLD__STORE__PATH', 'PATH', 'LANG', 'LC_ALL']))
        settings = json.loads(self.settings.read_text())
        mod = self.bundle / 'hooks/claude-mod'
        self.assertEqual(settings['env']['CLAUDE_CODE_PLUGIN_DIRS'], str(mod))
        hook = self.bundle / 'hooks' / 'claude_turn_complete.py'
        self.assertEqual(settings['hooks']['Stop'], [
            {'matcher': '', 'hooks': [{'type': 'command', 'command': 'python3 ' + shlex.quote(str(hook))}]}])
        self.assertIn('Claude Code mod: added\n', output)
        self.assertIn('Claude Code turn hook: added\n', output)
        self.assertNotIn('Full Disk Access', output)
        self.assertTrue((self.prefix / 'haroldctl').is_file())

    def test_linux_prerequisites_name_the_pacman_and_grpcurl_lines(self):
        with patch.object(sys, 'platform', 'linux'), patch.object(self.install.os, 'getuid', return_value=1000), \
             patch.object(self.install.shutil, 'which', return_value=None):
            with self.assertRaises(RuntimeError) as prebuilt:
                self.install.prerequisites(self.prefix, prebuilt=True)
            with self.assertRaises(RuntimeError) as source:
                self.install.prerequisites(self.prefix, prebuilt=False)
        message = str(prebuilt.exception)
        self.assertIn('Missing tools: rex, grpcurl, lsof, systemctl, curl.', message)
        self.assertIn('sudo pacman -S --needed python curl lsof inetutils base-devel pipewire', message)
        self.assertIn('grpcurl-bin', message)
        self.assertNotIn('brew', message)
        self.assertNotIn('xcode-select', message)
        self.assertIn('Missing tools: rex, grpcurl, lsof, systemctl, curl, cargo, rustc, protoc.',
                      str(source.exception))

    def test_linux_prerequisites_refuse_root(self):
        with patch.object(sys, 'platform', 'linux'), patch.object(self.install.os, 'getuid', return_value=0):
            with self.assertRaisesRegex(RuntimeError, 'without sudo'):
                self.install.linux_prerequisites(True)

    def test_the_rex_path_is_absolute_and_not_resolved(self):
        link = self.root / 'links'
        link.mkdir()
        (link / 'rex').symlink_to(self.root / 'fake bin' / 'rex')
        with self.fake, patch.dict('os.environ', {'PATH': str(link)}):
            self.assertEqual(self.install.find_rex(), str(link / 'rex'))


class PermissionsTests(unittest.TestCase):
    def setUp(self):
        import install
        self.install = install
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()

    def requirement(self, stdout, returncode=0):
        binary = self.root / 'harold'
        binary.write_text('binary')
        reply = subprocess.CompletedProcess([], returncode, stdout, 'Executable={}\n'.format(binary))
        with patch.object(subprocess, 'run', return_value=reply) as run:
            value = self.install.designated_requirement(binary)
        self.assertEqual(run.call_args.args[0], ['codesign', '-dr', '-', str(binary)])
        return value

    def test_the_requirement_is_read_from_codesign_output(self):
        self.assertEqual(self.requirement('# designated => identifier "harold" and certificate leaf = H"ab12"\n'),
                         'identifier "harold" and certificate leaf = H"ab12"')
        self.assertEqual(self.requirement('designated => cdhash H"315d"\n'), 'cdhash H"315d"')

    def test_an_unreadable_signature_has_no_requirement(self):
        self.assertIsNone(self.requirement('', returncode=1))
        self.assertIsNone(self.requirement('nothing useful\n'))
        with patch.object(subprocess, 'run') as run:
            self.assertIsNone(self.install.designated_requirement(self.root / 'missing'))
        run.assert_not_called()

    def message(self, old='same', new='same', identity='Kah Geh Tan'):
        return self.install.permissions_message(old, new, identity, Path('/Users/u/my bin/harold/harold'))

    def test_matching_requirements_carry_over(self):
        self.assertEqual(self.message(), self.install.CARRY_OVER)

    def test_a_changed_or_missing_requirement_prints_the_steps(self):
        steps = self.install.GRANT_STEPS.format(binary='/Users/u/my bin/harold/harold')
        self.assertEqual(self.message(new='other'), steps)
        self.assertEqual(self.message(old=None), steps)
        self.assertEqual(self.message(old=None, new=None), steps)
        self.assertIn('enter /Users/u/my bin/harold/harold, press Enter', steps)

    def test_ad_hoc_steps_end_with_the_note(self):
        steps = self.install.GRANT_STEPS.format(binary='/Users/u/my bin/harold/harold')
        self.assertEqual(self.message(new='other', identity='-'), steps + '\n' + self.install.AD_HOC_NOTE)

    def test_the_printed_text_matches_the_spec(self):
        self.assertEqual(self.install.CARRY_OVER,
                         'macOS permissions: this harold is signed like the one it replaced; '
                         'Full Disk Access and Messages control carry over.')
        self.assertEqual(self.install.GRANT_STEPS.format(binary='/B/harold'), '\n'.join([
            'macOS permissions for this build of harold:',
            '  Full Disk Access (needed to read replies from the phone):',
            '    System Settings > Privacy & Security > Full Disk Access.',
            '    If "harold" is in the list, select it and click "−".',
            '    Click "+", press Cmd+Shift+G, enter /B/harold, press Enter, click Open.',
            '    Check that the switch next to "harold" is on. No restart is needed.',
            '  Messages: the first time Harold sends a message, macOS asks whether "harold" may control',
            '    "Messages". Click Allow. Until then Harold cannot send.',
            '  If you switched on python3.13 for Harold before, you can switch it off now.']))
        self.assertEqual(self.install.AD_HOC_NOTE,
                         'Signed with no certificate: these steps are needed after every install of a new build. '
                         'See "Keep permissions across installs" in docs/how-tos/setup.md.')


class ServiceTests(unittest.TestCase):
    def setUp(self):
        import harold_service
        self.service = harold_service
        patcher = patch.object(sys, 'platform', 'darwin')  # these tests cover the launchd backend
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_managed_environment_clears_inherited_overrides_and_locale(self):
        inherited = {'HAROLD__GRPC__PORT': '9999', 'HAROLD_ENV': 'evil',
                     'LC_MESSAGES': 'C', 'LC_ALL': 'C', 'HOME': '/home/test'}
        controlled = {'HAROLD_ENV': 'local', 'HAROLD_CONFIG_DIR': '/bundle/config',
                      'HAROLD__STORE__PATH': '/bundle/data/store', 'PATH': '/tools',
                      'LANG': 'en_US.UTF-8', 'LC_ALL': 'en_US.UTF-8'}
        env = self.service.managed_environment(controlled, inherited)
        self.assertNotIn('HAROLD__GRPC__PORT', env)
        self.assertNotIn('LC_MESSAGES', env)
        self.assertEqual(env['HAROLD_ENV'], 'local')
        self.assertEqual(env['LC_ALL'], 'en_US.UTF-8')
        self.assertEqual(env['HOME'], '/home/test')

    def test_unrelated_listener_is_not_signalled(self):
        metadata = {'bundle': '/bundle', 'grpc_addr': '127.0.0.1:50060',
                    'label': 'test.harold', 'plist': '/test.plist', 'env': {}}
        with patch.object(self.service, 'listener_pids', return_value={123}), \
             patch.object(self.service, 'executable_matches', return_value=False), \
             patch.object(self.service.os, 'kill') as kill:
            with self.assertRaisesRegex(RuntimeError, 'unrelated'):
                self.service.stop_unmanaged(metadata)
        kill.assert_not_called()

    def test_exact_installed_daemon_on_previous_port_is_stopped(self):
        metadata = {'bundle': '/bundle', 'grpc_addr': '127.0.0.1:50123'}
        with patch.object(self.service, 'listener_pids', return_value=set()), \
             patch.object(self.service, 'executable_pids', return_value={456}), \
             patch.object(self.service, 'executable_matches', return_value=True), \
             patch.object(self.service, 'pid_alive', return_value=False), \
             patch.object(self.service.os, 'kill') as kill:
            self.service.stop_unmanaged(metadata)
        kill.assert_called_once_with(456, self.service.signal.SIGTERM)

    def test_failed_readiness_unloads_candidate(self):
        metadata = {'bundle': '/bundle', 'grpc_addr': '127.0.0.1:50060',
                    'label': 'test.harold', 'plist': '/test.plist', 'env': {}}
        with patch.object(self.service, 'launch_pid', return_value=42), \
             patch.object(self.service, 'listener_pids', return_value={42}), \
             patch.object(self.service, 'probe_ready', return_value=False), \
             patch.object(self.service, 'unload') as unload:
            with self.assertRaisesRegex(RuntimeError, 'readiness'):
                self.service.launch(metadata, timeout=0)
        unload.assert_called_once_with(metadata)

    def test_revalidation_refreshes_edited_endpoint_without_exposing_config_output(self):
        metadata = {'bundle': '/bundle', 'grpc_addr': '127.0.0.1:50060', 'env': {}}
        reply = subprocess.CompletedProcess([], 0, '{"grpc_addr":"127.0.0.1:50123","store_path":"/bundle/data/store"}', '')
        with patch.object(self.service, 'run', return_value=reply):
            updated = self.service.validated_metadata(metadata)
        self.assertEqual(updated['grpc_addr'], '127.0.0.1:50123')
        reply.returncode = 1
        reply.stderr = 'secret credential'
        with patch.object(self.service, 'run', return_value=reply):
            with self.assertRaisesRegex(RuntimeError, '^Configuration validation failed') as error:
                self.service.validated_metadata(metadata)
        self.assertNotIn('secret', str(error.exception))

    def test_revalidation_refuses_the_old_events_folder_as_the_store(self):
        metadata = {'bundle': '/bundle', 'grpc_addr': '127.0.0.1:50060', 'env': {}}
        reply = subprocess.CompletedProcess(
            [], 0, '{"grpc_addr":"127.0.0.1:50123","store_path":"/bundle/data/events"}', '')
        with patch.object(self.service, 'run', return_value=reply):
            with self.assertRaisesRegex(RuntimeError, '^Configuration does not use the managed store path$'):
                self.service.validated_metadata(metadata)

    def test_run_is_no_longer_a_command(self):
        error = io.StringIO()
        with patch.object(sys, 'argv', ['service.py', 'run']), \
             patch.object(self.service.os, 'execve') as execve, \
             contextlib.redirect_stderr(error):
            with self.assertRaises(SystemExit) as exit:
                self.service.main()
        self.assertEqual(exit.exception.code, 2)
        self.assertIn("invalid choice: 'run'", error.getvalue())
        execve.assert_not_called()

    def test_revalidation_keeps_every_other_key(self):
        metadata = {'bundle': '/bundle', 'label': 'test.harold', 'plist': '/bundle/service.plist',
                    'grpc_addr': '127.0.0.1:50060', 'env': {'HAROLD_ENV': 'local'},
                    'signing_identity': 'Kah Geh Tan'}
        reply = subprocess.CompletedProcess(
            [], 0, '{"grpc_addr":"127.0.0.1:50123","store_path":"/bundle/data/store"}', '')
        with patch.object(self.service, 'run', return_value=reply):
            updated = self.service.validated_metadata(metadata)
        self.assertEqual(updated, dict(metadata, grpc_addr='127.0.0.1:50123'))


class ClaudeSettingsTests(unittest.TestCase):
    MOD = '/home/u/bin/harold/hooks/claude-mod'

    def setUp(self):
        import install
        self.install = install
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / '.claude' / 'settings.json'
        self.backup = self.path.with_name('settings.json.before-harold')

    def link(self):
        return self.install.link_claude_mod(self.path, Path(self.MOD))

    def write(self, text):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(text)

    def assert_untouched(self, text, outcome):
        self.assertEqual(self.path.read_text(), text)
        self.assertFalse(self.backup.exists())
        self.assertIn('CLAUDE_CODE_PLUGIN_DIRS', outcome)
        self.assertIn(self.MOD, outcome)

    def test_missing_file_writes_nothing_and_prints_the_line(self):
        outcome = self.link()
        self.assertFalse(self.path.exists())
        self.assertFalse(self.backup.exists())
        self.assertIn('CLAUDE_CODE_PLUGIN_DIRS', outcome)
        self.assertIn(self.MOD, outcome)

    def test_unsafe_files_are_left_alone(self):
        for text in ('{not json', '[]', '{"env": []}', '{"env": {"CLAUDE_CODE_PLUGIN_DIRS": 3}}'):
            self.write(text)
            self.assert_untouched(text, self.link())

    def test_adds_the_variable_and_keeps_everything_else(self):
        original = '{"model": "x", "env": {"A": "1"}, "hooks": {"Stop": []}}'
        self.write(original)
        self.link()
        self.assertEqual(json.loads(self.path.read_text()), {
            'model': 'x', 'hooks': {'Stop': []},
            'env': {'A': '1', 'CLAUDE_CODE_PLUGIN_DIRS': self.MOD}})
        self.assertEqual(self.backup.read_text(), original)

    def test_adds_env_when_there_is_none(self):
        self.write('{"model": "x"}')
        self.link()
        self.assertEqual(json.loads(self.path.read_text()),
                         {'model': 'x', 'env': {'CLAUDE_CODE_PLUGIN_DIRS': self.MOD}})

    def test_appends_to_other_folders(self):
        self.write('{"env": {"CLAUDE_CODE_PLUGIN_DIRS": "/a:/b"}}')
        self.link()
        self.assertEqual(json.loads(self.path.read_text())['env']['CLAUDE_CODE_PLUGIN_DIRS'],
                         '/a:/b:' + self.MOD)
        self.assertTrue(self.backup.exists())

    def test_already_named_writes_nothing(self):
        for value in (self.MOD, '/a:{}:/b'.format(self.MOD), '/a:{}:'.format(self.MOD)):
            text = json.dumps({'env': {'CLAUDE_CODE_PLUGIN_DIRS': value}})
            self.write(text)
            self.assertEqual(self.link(), 'unchanged')
            self.assertEqual(self.path.read_text(), text)
            self.assertFalse(self.backup.exists())

    def test_an_existing_backup_is_never_overwritten(self):
        self.write('{"model": "new"}')
        self.backup.write_text('older copy')
        self.link()
        self.assertEqual(self.backup.read_text(), 'older copy')
        self.assertIn('CLAUDE_CODE_PLUGIN_DIRS', self.path.read_text())

    def test_a_symlinked_settings_file_stays_a_symlink(self):
        target = Path(self.temp.name) / 'repo' / 'settings.json'
        target.parent.mkdir()
        target.write_text('{"model": "x"}')
        self.path.parent.mkdir(parents=True)
        self.path.symlink_to(target)
        self.link()
        self.assertTrue(self.path.is_symlink())
        self.assertIn(self.MOD, json.loads(target.read_text())['env']['CLAUDE_CODE_PLUGIN_DIRS'])
        self.assertEqual((target.parent / 'settings.json.before-harold').read_text(), '{"model": "x"}')
        self.assertFalse(self.backup.exists())

    def test_rewrite_keeps_permissions_and_non_ascii_text(self):
        self.write('{"note": "caf\u00e9 \u2014 na\u00efve"}')
        self.path.chmod(0o600)
        self.link()
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        text = self.path.read_text(encoding='utf-8')
        self.assertIn('caf\u00e9 \u2014 na\u00efve', text)
        self.assertNotIn('\\u00e9', text)

    def test_a_folder_containing_a_colon_cannot_be_listed(self):
        self.write('{"model": "x"}')
        outcome = self.install.link_claude_mod(self.path, Path('/odd:dir/claude-mod'))
        self.assertEqual(self.path.read_text(), '{"model": "x"}')
        self.assertFalse(self.backup.exists())
        self.assertIn('/odd:dir/claude-mod', outcome)
        self.assertIn("':'", outcome)

    def test_a_failed_write_returns_the_manual_line_and_leaves_no_temp_file(self):
        self.write('{"model": "x"}')
        folder = self.path.parent
        folder.chmod(0o500)
        self.addCleanup(folder.chmod, 0o700)
        outcome = self.link()
        folder.chmod(0o700)
        self.assertIn('CLAUDE_CODE_PLUGIN_DIRS', outcome)
        self.assertEqual(self.path.read_text(), '{"model": "x"}')
        self.assertEqual(sorted(p.name for p in folder.iterdir()), ['settings.json'])

    def test_equivalent_spellings_of_the_folder_count_as_named(self):
        home = Path(self.temp.name) / 'home'
        mod = home / 'bin/harold/hooks/claude-mod'
        for value in (str(mod) + '/', '~/bin/harold/hooks/claude-mod'):
            text = json.dumps({'env': {'CLAUDE_CODE_PLUGIN_DIRS': value}})
            self.write(text)
            with patch.dict('os.environ', {'HOME': str(home)}):
                self.assertEqual(self.install.link_claude_mod(self.path, mod), 'unchanged')
            self.assertEqual(self.path.read_text(), text)

    def test_a_number_json_cannot_write_back_leaves_the_file_alone(self):
        self.write('{"big": 1e999}')
        outcome = self.link()
        self.assertEqual(self.path.read_text(), '{"big": 1e999}')
        self.assertIn('CLAUDE_CODE_PLUGIN_DIRS', outcome)
        self.assertEqual(sorted(p.name for p in self.path.parent.iterdir()), ['settings.json'])

    def test_symlinks_at_the_backup_and_temp_paths_are_not_written_through(self):
        self.write('{"model": "x"}')
        victim = Path(self.temp.name) / 'victim'
        victim.write_text('keep')
        self.backup.symlink_to(victim)
        self.path.with_name('settings.json.harold-tmp').symlink_to(victim)
        self.link()
        self.assertEqual(victim.read_text(), 'keep')
        self.assertIn(self.MOD, self.path.read_text())
        self.assertFalse(self.path.is_symlink())


class TurnHookTests(unittest.TestCase):
    def setUp(self):
        import install
        self.install = install
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.path = root / '.claude' / 'settings.json'
        self.backup = self.path.with_name('settings.json.before-harold')
        self.hook = root / 'bin/harold/hooks/claude_turn_complete.py'
        self.command = 'python3 {}'.format(self.hook)
        self.old_script = root / '.claude/hooks/turn_complete.py'
        self.old_script.parent.mkdir(parents=True)
        self.old_script.write_text('from harold_turn_complete import TurnComplete, notify_harold\n')

    def register(self):
        return self.install.register_turn_hook(self.path, self.hook)

    def write(self, data):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text(data if isinstance(data, str) else json.dumps(data))

    def stop(self, *commands):
        return {'matcher': '', 'hooks': [{'type': 'command', 'command': command} for command in commands]}

    def read(self):
        return json.loads(self.path.read_text())

    def assert_manual(self, text, outcome):
        self.assertEqual(self.path.read_text(), text)
        self.assertFalse(self.backup.exists())
        self.assertIn(self.command, outcome)
        self.assertIn('"Stop"', outcome)

    def test_a_hook_running_a_script_that_imports_the_notifier_is_replaced(self):
        other = {'matcher': 'Bash', 'hooks': [{'type': 'command', 'command': 'echo other'}]}
        before = {'model': 'x', 'env': {'A': '1'}, 'hooks': {'Stop': [self.stop('uv run {}'.format(self.old_script))],
                                                           'PreToolUse': [other]}}
        self.write(before)
        self.assertEqual(self.register(), 'replaced')
        after = self.read()
        self.assertEqual(after['hooks']['Stop'], [self.stop(self.command)])
        self.assertEqual(after['hooks']['PreToolUse'], [other])
        self.assertEqual((after['model'], after['env']), ('x', {'A': '1'}))
        self.assertEqual(json.loads(self.backup.read_text()), before)
        self.assertTrue(self.old_script.exists())

    def test_harolds_own_script_at_another_path_is_replaced_and_its_other_keys_kept(self):
        entry = {'matcher': '', 'hooks': [{'type': 'command', 'timeout': 9,
                                           'command': 'python3 /old/bin/harold/hooks/claude_turn_complete.py'}]}
        self.write({'hooks': {'Stop': [entry]}})
        self.assertEqual(self.register(), 'replaced')
        self.assertEqual(self.read()['hooks']['Stop'][0]['hooks'],
                         [{'type': 'command', 'timeout': 9, 'command': self.command}])

    def test_the_right_command_is_unchanged(self):
        text = json.dumps({'hooks': {'Stop': [self.stop(self.command)]}})
        self.write(text)
        self.assertEqual(self.register(), 'unchanged')
        self.assertEqual(self.path.read_text(), text)
        self.assertFalse(self.backup.exists())

    def test_no_stop_hook_gets_one_added(self):
        for before in ({'model': 'x'}, {'hooks': {}}, {'hooks': {'Stop': []}}):
            self.backup.unlink(missing_ok=True)
            self.write(before)
            self.assertEqual(self.register(), 'added')
            after = self.read()
            self.assertEqual(after['hooks']['Stop'], [self.stop(self.command)])
            self.assertEqual(json.loads(self.backup.read_text()), before)

    def test_another_tools_stop_hook_is_kept_and_ours_added(self):
        script = Path(self.temp.name) / 'other.py'
        script.write_text('print("other tool")\n')
        entries = [self.stop('python3 {}'.format(script)), self.stop('echo done')]
        self.write({'hooks': {'Stop': entries}})
        self.assertEqual(self.register(), 'added')
        self.assertEqual(self.read()['hooks']['Stop'], entries + [self.stop(self.command)])

    def test_unsure_cases_change_nothing_and_print_the_json(self):
        second = Path(self.temp.name) / 'second.py'
        second.write_text('import harold_turn_complete\n')
        cases = ('{not json', '[]', '{"hooks": []}', '{"hooks": {"Stop": {}}}', '{"hooks": {"Stop": [3]}}',
                 '{"hooks": {"Stop": [{"hooks": [4]}]}}',
                 json.dumps({'hooks': {'Stop': [self.stop('uv run {}'.format(self.old_script)),
                                                self.stop('python3 {}'.format(second))]}}),
                 # The right hook beside an old one still runs twice: unsure, not unchanged.
                 json.dumps({'hooks': {'Stop': [self.stop(self.command),
                                                self.stop('uv run {}'.format(self.old_script))]}}))
        for text in cases:
            self.write(text)
            self.assert_manual(text, self.register())

    def test_a_missing_file_writes_nothing_and_prints_the_json(self):
        outcome = self.register()
        self.assertFalse(self.path.exists())
        self.assertIn(self.command, outcome)

    def test_a_symlinked_settings_file_is_edited_through_the_link(self):
        real = Path(self.temp.name) / 'dotfiles/settings.json'
        real.parent.mkdir()
        real.write_text(json.dumps({'model': 'x'}))
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.symlink_to(real)
        self.assertEqual(self.register(), 'added')
        self.assertTrue(self.path.is_symlink())
        self.assertEqual(json.loads(real.read_text())['hooks']['Stop'], [self.stop(self.command)])
        self.assertEqual(json.loads(real.with_name('settings.json.before-harold').read_text()), {'model': 'x'})
        self.assertFalse(self.backup.exists())

    def test_a_hook_for_another_tool_is_never_touched(self):
        script = Path(self.temp.name) / 'notify.sh'
        script.write_text('#!/bin/sh\n')
        before = {'hooks': {'Stop': [self.stop('sh {}'.format(script), 'python3 /x/notes.py')]}}
        self.write(before)
        self.assertEqual(self.register(), 'added')
        self.assertEqual(self.read()['hooks']['Stop'][0], before['hooks']['Stop'][0])

    def test_the_file_keeps_its_mode_and_leaves_no_temporary_file(self):
        self.write({'model': 'x'})
        self.path.chmod(0o600)
        self.register()
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(sorted(p.name for p in self.path.parent.iterdir() if p.is_file()),
                         ['settings.json', 'settings.json.before-harold'])


class SkillTests(unittest.TestCase):
    NAME = 'send-message-to-away-device'

    def setUp(self):
        import install
        self.install = install
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name).resolve()
        self.target = root / 'bin/harold/skills' / self.NAME
        self.target.mkdir(parents=True)
        (self.target / 'SKILL.md').write_text('skill')
        self.skills = root / 'agent/skills'
        self.path = self.skills / self.NAME

    def link(self):
        return self.install.link_skill(self.skills, self.target)

    def assert_manual(self, outcome):
        self.assertEqual(outcome, 'not linked, {} already exists. To link it: ln -s {} {}'.format(
            self.path, self.target, self.path))

    def test_the_repository_skill_names_the_binary_by_its_placeholder(self):
        text = (SCRIPTS.parent / SKILL_FILE).read_text(encoding='utf-8')
        self.assertTrue(text.startswith('---\nname: send-message-to-away-device\ndescription: '))
        self.assertIn('{{HAROLD_BIN}} send-message-to-away-device --src-id "${TMUX_PANE:-$REX_BLOCK}" \'', text)
        self.assertNotIn('~/bin/harold/harold', text)

    def test_write_replaces_every_placeholder_with_the_quoted_binary(self):
        source = Path(self.temp.name) / 'SKILL.md'
        source.write_text(SKILL_SOURCE)
        destination = Path(self.temp.name) / 'stage/skills' / self.NAME / 'SKILL.md'
        self.install.write_skill(source, destination, Path('/Users/u/bin/harold/harold'))
        self.assertEqual(destination.read_text(),
                         'Run /Users/u/bin/harold/harold send-message-to-away-device. '
                         'Again: /Users/u/bin/harold/harold\n')
        self.install.write_skill(source, destination, Path('/Users/u/my bin/harold/harold'))
        self.assertIn("Run '/Users/u/my bin/harold/harold' send", destination.read_text())

    def test_an_unreadable_parent_folder_is_reported_and_not_raised(self):
        self.skills.mkdir(parents=True)
        self.addCleanup(self.skills.parent.chmod, 0o700)
        self.skills.parent.chmod(0o000)
        try:
            outcome = self.link()
        finally:
            self.skills.parent.chmod(0o700)
        self.assertIsInstance(outcome, str)
        self.assertTrue(outcome.startswith('not linked ('), outcome)

    def test_a_missing_skills_folder_is_not_created(self):
        self.assertEqual(self.link(), 'not linked ({} not found)'.format(self.skills))
        self.assertFalse(self.skills.exists())

    def test_nothing_there_creates_the_link(self):
        self.skills.mkdir(parents=True)
        self.assertEqual(self.link(), 'linked')
        self.assertEqual(os.readlink(self.path), str(self.target))
        self.assertEqual((self.path / 'SKILL.md').read_text(), 'skill')

    def test_a_link_to_the_installed_folder_is_unchanged(self):
        self.skills.mkdir(parents=True)
        self.link()
        self.assertEqual(self.link(), 'unchanged')
        self.assertEqual(os.readlink(self.path), str(self.target))

    def test_anything_else_there_is_left_alone(self):
        self.skills.mkdir(parents=True)
        elsewhere = Path(self.temp.name) / 'elsewhere'
        elsewhere.mkdir()

        self.path.mkdir()
        self.assert_manual(self.link())
        self.assertTrue(self.path.is_dir() and not self.path.is_symlink())
        self.path.rmdir()

        self.path.write_text('mine')
        self.assert_manual(self.link())
        self.assertEqual(self.path.read_text(), 'mine')
        self.assertFalse(self.path.is_symlink())
        self.path.unlink()

        self.path.symlink_to(elsewhere)
        self.assert_manual(self.link())
        self.assertEqual(os.readlink(self.path), str(elsewhere))

    def test_a_broken_link_counts_as_already_there(self):
        self.skills.mkdir(parents=True)
        self.path.symlink_to(Path(self.temp.name) / 'gone')
        self.assert_manual(self.link())
        self.assertEqual(os.readlink(self.path), str(Path(self.temp.name) / 'gone'))

    def test_a_skills_folder_that_is_a_link_is_linked_into(self):
        real = Path(self.temp.name).resolve() / 'dotfiles/skills'
        real.mkdir(parents=True)
        self.skills.parent.mkdir(parents=True)
        self.skills.symlink_to(real)
        self.assertEqual(self.link(), 'linked')
        self.assertEqual(os.readlink(real / self.NAME), str(self.target))

    def test_paths_under_home_are_shown_with_a_tilde_and_quoted_in_the_command(self):
        home = Path(self.temp.name).resolve() / 'home dir'
        skills = home / '.claude/skills'
        target = home / 'bin/harold/skills' / self.NAME
        with patch.object(Path, 'home', return_value=home):
            self.assertEqual(self.install.link_skill(skills, target),
                             'not linked (~/.claude/skills not found)')
            skills.mkdir(parents=True)
            (skills / self.NAME).mkdir()
            self.assertEqual(
                self.install.link_skill(skills, target),
                "not linked, ~/.claude/skills/{0} already exists. To link it: ln -s '{1}' '{2}'".format(
                    self.NAME, target, skills / self.NAME))

    def test_a_failure_to_link_is_reported_and_not_raised(self):
        self.skills.mkdir(parents=True)
        self.skills.chmod(0o500)
        self.addCleanup(self.skills.chmod, 0o700)
        outcome = self.link()
        self.skills.chmod(0o700)
        self.assertTrue(outcome.startswith('not linked ('), outcome)
        self.assertFalse(os.path.lexists(self.path))


class ReleaseArchiveTests(unittest.TestCase):
    """The archive the build workflow packs, which `--prebuilt-dir` installs from."""

    def test_the_archive_carries_the_release_tag_in_a_version_file(self):
        workflow = (SCRIPTS.parent / '.github/workflows/build.yml').read_text()
        start = workflow.index('- name: Package and smoke test')
        package = workflow[start:workflow.index('- name: Upload release assets')]
        self.assertIn('RELEASE_TAG: ${{ github.event.release.tag_name }}', package)
        self.assertIn('printf \'%s\\n\' "$RELEASE_TAG" > "dist/$package/VERSION"', package)
        self.assertLess(package.index('/VERSION"'), package.index('tar -czf'))


if __name__ == '__main__':
    unittest.main()
