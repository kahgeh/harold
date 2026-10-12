import contextlib
import io
import json
import os
import re
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
    }) for name in ('harold', 'tmx-agent-dash'))


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

    def test_no_terminal_requires_config(self):
        with patch('sys.stdin.isatty', return_value=False):
            with self.assertRaisesRegex(RuntimeError, '--config'):
                self.install.choose_config(None, self.prefix / 'harold')

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

    def test_manual_service_has_no_login_registration(self):
        metadata = self.install.make_metadata(self.prefix, '127.0.0.1:50060')
        path = self.prefix / 'service.plist'
        self.install.write_plist(path, metadata)
        plist = plistlib.loads(path.read_bytes())
        self.assertEqual(metadata['plist'], str(self.prefix / 'harold/service.plist'))
        self.assertFalse(plist.get('RunAtLoad', False))
        self.assertFalse(plist.get('KeepAlive', False))

    def test_signing_and_invalid_config_fail_before_stopping_old_service(self):
        repo = self.prefix / 'repo'
        for name in ('target/release/harold', 'target/release/tmx-agent-dash',
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
                     patch.object(self.install.service, 'stop') as stop:
                    with self.assertRaises((RuntimeError, subprocess.CalledProcessError)):
                        self.install.install(args, repo)
                stop.assert_not_called()
                self.assertEqual((bundle / 'config/local.toml').read_text(), 'secret = "never print"')

    def test_installs_cargo_reported_binaries_from_custom_target_directory(self):
        repo = self.prefix / 'repo'
        for name in ('target/release/harold', 'target/release/tmx-agent-dash',
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
        for name in ('harold', 'tmx-agent-dash'):
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
                output = artifact_output(repo, artifacts)
            elif '--check-config' in argv:
                self.assertEqual(Path(argv[0]).read_text(), 'built harold')
                output = json.dumps({'grpc_addr': '127.0.0.1:50123',
                                     'store_path': str(self.prefix / 'harold/data/events')})
            else:
                output = ''
            return subprocess.CompletedProcess(argv, 0, output, '')

        printed = io.StringIO()
        with patch.object(self.install, 'prerequisites'), \
             patch.object(subprocess, 'run', side_effect=process), \
             patch.object(self.install.service, 'listener_pids', return_value=set()), \
             patch.object(self.install.service, 'stop'), \
             patch.object(self.install.service, 'stop_unmanaged'), \
             patch.object(self.install.service, 'start', return_value=123):
            with contextlib.redirect_stdout(printed):
                self.install.install(args, repo)
        self.assertEqual((self.prefix / 'harold/harold').read_text(), 'built harold')
        self.assertEqual((self.prefix / 'tmx-agent-dash').read_text(), 'built tmx-agent-dash')
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
        for name in ('harold', 'tmx-agent-dash', 'harold.proto',
                     'hooks/harold_turn_complete.py', 'hooks/claude_turn_complete.py',
                     'hooks/codex_turn_complete.py', 'scripts/harold_service.py',
                     'hooks/claude-mod/hooks/register.ts', 'hooks/claude-mod/hooks/accept.ts', 'hooks/claude-mod/hooks/hooks.json',
                     'hooks/claude-mod/.claude-plugin/plugin.json',
                     'config/default.toml', 'config/local.template.toml', SKILL_FILE):
            path = release / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('release ' + name)
            if name in ('harold', 'tmx-agent-dash'):
                path.chmod(0o755)
        config = self.prefix / 'import.toml'
        config.write_text('agents = []')
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
                                     'store_path': str(self.prefix / 'harold/data/events')})
            return subprocess.CompletedProcess(argv, 0, output, '')

        with patch.object(sys, 'platform', 'darwin'), \
             patch.object(self.install.os, 'getuid', return_value=501), \
             patch.object(self.install.shutil, 'which', side_effect=which), \
             patch.object(self.install, 'build_binaries') as build, \
             patch.object(subprocess, 'run', side_effect=process), \
             patch.object(self.install.service, 'listener_pids', return_value=set()), \
             patch.object(self.install.service, 'stop'), \
             patch.object(self.install.service, 'stop_unmanaged'), \
             patch.object(self.install.service, 'start', return_value=123):
            self.install.install(args, self.prefix / 'nonexistent source')
        build.assert_not_called()
        self.assertEqual((self.prefix / 'harold/harold').read_text(), 'release harold')
        self.assertEqual((self.prefix / 'tmx-agent-dash').read_text(), 'release tmx-agent-dash')
        self.assertEqual((self.prefix / 'harold/config/default.toml').read_text(),
                         'release config/default.toml')
        self.assertEqual((self.prefix / 'harold/config/local.toml').read_text(), 'agents = []')
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

    def test_release_missing_any_mod_runtime_file_is_rejected(self):
        for missing in RUNTIME_FILES:
            release = self.prefix / ('release-' + missing.replace('/', '-'))
            for name in ('harold', 'tmx-agent-dash', 'harold.proto',
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

    def test_release_without_the_skill_is_rejected(self):
        release = self.prefix / 'release-no-skill'
        for name in ('harold', 'tmx-agent-dash', 'harold.proto',
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

    def test_missing_cargo_artifact_fails_without_falling_back_to_stale_file(self):
        repo = self.prefix / 'repo'
        (repo / 'target/release').mkdir(parents=True)
        (repo / 'target/release/harold').write_text('stale')
        reply = subprocess.CompletedProcess([], 0, '{"reason":"build-finished","success":true}\n', '')
        with patch.object(subprocess, 'run', return_value=reply):
            with self.assertRaisesRegex(RuntimeError, 'did not report'):
                self.install.build_binaries(repo, offline=True)


class ServiceTests(unittest.TestCase):
    def setUp(self):
        import harold_service
        self.service = harold_service

    def test_managed_environment_clears_inherited_overrides_and_locale(self):
        inherited = {'HAROLD__GRPC__PORT': '9999', 'HAROLD_ENV': 'evil',
                     'LC_MESSAGES': 'C', 'LC_ALL': 'C', 'HOME': '/home/test'}
        controlled = {'HAROLD_ENV': 'local', 'HAROLD_CONFIG_DIR': '/bundle/config',
                      'HAROLD__STORE__PATH': '/bundle/data/events', 'PATH': '/tools',
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
             patch.object(self.service, 'stop') as stop:
            with self.assertRaisesRegex(RuntimeError, 'readiness'):
                self.service.start(metadata, timeout=0)
        stop.assert_called_once_with(metadata)

    def test_revalidation_refreshes_edited_endpoint_without_exposing_config_output(self):
        metadata = {'bundle': '/bundle', 'grpc_addr': '127.0.0.1:50060', 'env': {}}
        reply = subprocess.CompletedProcess([], 0, '{"grpc_addr":"127.0.0.1:50123","store_path":"/bundle/data/events"}', '')
        with patch.object(self.service, 'run', return_value=reply):
            updated = self.service.validated_metadata(metadata)
        self.assertEqual(updated['grpc_addr'], '127.0.0.1:50123')
        reply.returncode = 1
        reply.stderr = 'secret credential'
        with patch.object(self.service, 'run', return_value=reply):
            with self.assertRaisesRegex(RuntimeError, '^Configuration validation failed') as error:
                self.service.validated_metadata(metadata)
        self.assertNotIn('secret', str(error.exception))


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
        self.assertIn('{{HAROLD_BIN}} send-message-to-away-device --src-id "$TMUX_PANE" \'', text)
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


if __name__ == '__main__':
    unittest.main()
