import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

BOOTSTRAP = Path(__file__).resolve().parents[1] / 'bootstrap.sh'


class BootstrapTests(unittest.TestCase):
    def run_bootstrap(self, corrupt=False, unsafe=False, system='Darwin', machine='arm64'):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / 'bin'
            tools.mkdir()
            package = ('harold-x86_64-unknown-linux-gnu' if system == 'Linux'
                       else 'harold-aarch64-apple-darwin')
            archive = root / (package + '.tar.gz')
            marker = root / 'installed'
            with tarfile.open(archive, 'w:gz') as tar:
                payload = ('import pathlib, sys\n'
                           'pathlib.Path({!r}).write_text(repr(sys.argv[1:]))\n'.format(str(marker))).encode()
                entry = tarfile.TarInfo('../escape' if unsafe else package + '/scripts/install.py')
                entry.size = len(payload)
                tar.addfile(entry, io.BytesIO(payload))
            checksum = '0' * 64 if corrupt else hashlib.sha256(archive.read_bytes()).hexdigest()
            (root / (package + '.tar.gz.sha256')).write_text(checksum + '  ' + package + '.tar.gz\n')
            stubs = {
                'uname': '#!/bin/sh\ncase "$1" in -s) echo {};; -m) echo {};; esac\n'.format(system, machine),
                'sw_vers': '#!/bin/sh\necho 15.0\n',
                'curl': '''#!/usr/bin/env python3
import os, pathlib, shutil, sys
with open(os.environ['FIXTURE'] + '/curl-called', 'a') as called:
    called.write('called\\n')
args = sys.argv[1:]
output = args[args.index('--output') + 1]
shutil.copyfile(pathlib.Path(os.environ['FIXTURE']) / args[-1].rsplit('/', 1)[-1], output)
''',
            }
            if system == 'Linux':
                stubs['sha256sum'] = '#!/bin/sh\nexec /usr/bin/sha256sum "$@"\n'
            for name, value in stubs.items():
                path = tools / name
                path.write_text(value)
                path.chmod(0o755)
            env = dict(os.environ, PATH=str(tools) + ':' + os.environ['PATH'], FIXTURE=str(root))
            result = subprocess.run(['sh', '-s', '--', '--config', '/a path/config.toml'],
                                    input=BOOTSTRAP.read_text(), capture_output=True, text=True, env=env)
            self.curl_called = (root / 'curl-called').exists()
            return result, marker.read_text() if marker.exists() else None

    def test_pipe_download_verifies_and_forwards_arguments(self):
        result, installed = self.run_bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--prebuilt-dir', installed)
        self.assertIn('/a path/config.toml', installed)

    def test_bad_checksum_does_not_execute_installer(self):
        result, installed = self.run_bootstrap(corrupt=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(installed)

    def test_archive_traversal_does_not_execute_installer(self):
        result, installed = self.run_bootstrap(unsafe=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Unsafe release archive', result.stderr)
        self.assertIsNone(installed)

    def test_linux_x86_64_takes_the_linux_package(self):
        result, installed = self.run_bootstrap(system='Linux', machine='x86_64')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--prebuilt-dir', installed)

    def test_linux_bad_checksum_does_not_execute_installer(self):
        result, installed = self.run_bootstrap(corrupt=True, system='Linux', machine='x86_64')
        self.assertNotEqual(result.returncode, 0)
        self.assertIsNone(installed)

    def test_linux_arm64_is_refused(self):
        result, installed = self.run_bootstrap(system='Linux', machine='aarch64')
        self.assertEqual(result.returncode, 1)
        self.assertIn('aarch64', result.stderr)
        self.assertFalse(self.curl_called)
        self.assertIsNone(installed)

    def test_unknown_system_is_refused(self):
        result, installed = self.run_bootstrap(system='FreeBSD', machine='amd64')
        self.assertEqual(result.returncode, 1)
        self.assertIn('FreeBSD', result.stderr)
        self.assertFalse(self.curl_called)
        self.assertIsNone(installed)
