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
    def run_bootstrap(self, corrupt=False, unsafe=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / 'bin'
            tools.mkdir()
            package = 'harold-aarch64-apple-darwin'
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
                'uname': '#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n',
                'sw_vers': '#!/bin/sh\necho 15.0\n',
                'curl': '''#!/usr/bin/env python3
import os, pathlib, shutil, sys
args = sys.argv[1:]
output = args[args.index('--output') + 1]
shutil.copyfile(pathlib.Path(os.environ['FIXTURE']) / args[-1].rsplit('/', 1)[-1], output)
''',
            }
            for name, value in stubs.items():
                path = tools / name
                path.write_text(value)
                path.chmod(0o755)
            env = dict(os.environ, PATH=str(tools) + ':' + os.environ['PATH'], FIXTURE=str(root))
            result = subprocess.run(['sh', '-s', '--', '--config', '/a path/config.toml'],
                                    input=BOOTSTRAP.read_text(), capture_output=True, text=True, env=env)
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
