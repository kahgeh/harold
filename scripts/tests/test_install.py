import json
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
                     'scripts/harold_service.py', 'harold/config/default.toml',
                     'harold/config/local.template.toml'):
            path = repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('fixture')
        bundle = self.prefix / 'harold'
        (bundle / 'config').mkdir(parents=True)
        (bundle / 'config/local.toml').write_text('secret = "never print"')
        args = argparse.Namespace(prefix=str(self.prefix), config=None, reinstall=False,
                                  offline=True, signing_identity='-')
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
                     'scripts/harold_service.py', 'harold/config/default.toml',
                     'harold/config/local.template.toml'):
            path = repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('stale')
        artifacts = self.prefix / 'custom target/aarch64-apple-darwin/release'
        artifacts.mkdir(parents=True)
        for name in ('harold', 'tmx-agent-dash'):
            (artifacts / name).write_text('built ' + name)
        config = self.prefix / 'import.toml'
        config.write_text('agents = []')
        args = argparse.Namespace(prefix=str(self.prefix), config=str(config), reinstall=False,
                                  offline=True, signing_identity='-')

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

        with patch.object(self.install, 'prerequisites'), \
             patch.object(subprocess, 'run', side_effect=process), \
             patch.object(self.install.service, 'listener_pids', return_value=set()), \
             patch.object(self.install.service, 'stop'), \
             patch.object(self.install.service, 'stop_unmanaged'), \
             patch.object(self.install.service, 'start', return_value=123):
            self.install.install(args, repo)
        self.assertEqual((self.prefix / 'harold/harold').read_text(), 'built harold')
        self.assertEqual((self.prefix / 'tmx-agent-dash').read_text(), 'built tmx-agent-dash')

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


if __name__ == '__main__':
    unittest.main()
