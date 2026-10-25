import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import release  # noqa: E402

REAL_RUN = release.run
# The build steps of a real v0.5.0 run (gh run view 38243819261), in order.
STEPS = ('Set up job', 'Run actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd',
         'Install build prerequisites', 'Test', 'Build release binaries',
         'Package and smoke test', 'Upload release assets')
PUBLISHED = '2026-10-15T09:00:00Z'
REVISION = '31323b9f00d5c0ffee31323b9f00d5c0ffee3132'


def git(cwd, *args):
    return subprocess.run(['git'] + list(args), cwd=str(cwd), check=True,
                          capture_output=True, text=True).stdout


def commit(cwd, subject, name):
    (Path(cwd) / name).write_text(subject + '\n')
    git(cwd, 'add', name)
    git(cwd, 'commit', '-q', '-m', subject)


def done(argv, code=0, stdout='', stderr=''):
    return subprocess.CompletedProcess(argv, code, stdout, stderr)


def option(args, name):
    """The value after `name` in `args`, or None."""
    return args[args.index(name) + 1] if name in args else None


class FakeClock:
    """Time that passes only when the command sleeps."""

    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class FakeGitHub:
    """kahgeh/harold as the `gh` calls under test see it, in the JSON shapes gh 2.73 prints.

    `gh release create` starts a build run, id 2, on the new tag. The run shows in `gh run list`
    from listing `appears_after + 1`; `gh run view` reports it finished at view `polls`, with
    `outcome`, and a successful run attaches both files when `attach` is set. `events` lists
    what changed on GitHub, and every download, in order.
    """

    def __init__(self):
        self.signed_in = True
        self.releases = {'v0.5.0': self.release('v0.5.0', latest=True)}
        # A run of an earlier v0.6.0, withdrawn with --cleanup-tag before this one was cut.
        self.runs = [{'databaseId': 1, 'headBranch': 'v0.6.0', 'createdAt': '2026-10-01T08:00:00Z',
                      'url': 'https://github.com/kahgeh/harold/actions/runs/1'}]
        self.new_run = None
        self.outcome, self.polls, self.attach, self.appears_after = 'success', 3, True, 0
        self.failing_views = 0
        # The `--json` fields for which `gh release view` fails.
        self.failing_release_views = ()
        self.files = {}
        self.events = []
        self.created = None
        self.listings = self.views = 0

    @staticmethod
    def release(tag, latest=False, prerelease=False, assets=()):
        return {'tagName': tag, 'isLatest': latest, 'isPrerelease': prerelease,
                'assets': [{'name': name} for name in assets], 'publishedAt': PUBLISHED,
                'url': 'https://github.com/kahgeh/harold/releases/tag/' + tag}

    @staticmethod
    def printed(argv, value):
        return done(argv, 0, json.dumps(value) + '\n')

    def __call__(self, argv):
        args = argv[1:]
        if args[:2] == ['auth', 'status']:
            if self.signed_in:
                return done(argv, 0)
            return done(argv, 1, '', 'You are not logged into any GitHub hosts.\n')
        assert option(args, '--repo') == 'kahgeh/harold', argv
        verb = tuple(args[:2])
        if verb == ('release', 'list'):
            return self.printed(argv, [{'tagName': item['tagName'], 'isLatest': item['isLatest']}
                                       for item in self.releases.values()])
        if verb == ('release', 'view'):
            item = self.releases.get(args[2])
            if item is None:
                return done(argv, 1, '', 'release not found\n')
            if option(args, '--json') in self.failing_release_views:
                return done(argv, 1, '', 'HTTP 502: Bad Gateway\n')
            return self.printed(argv, {field: item[field] for field in option(args, '--json').split(',')})
        if verb == ('release', 'create'):
            return self.create(argv, args)
        if verb == ('release', 'edit'):
            tag = args[2]
            assert '--prerelease=false' in args and '--latest' in args, argv
            for item in self.releases.values():
                item['isLatest'] = False
            self.releases[tag].update(isLatest=True, isPrerelease=False)
            self.events.append('edit ' + tag)
            return done(argv, 0, self.releases[tag]['url'] + '\n')
        if verb == ('release', 'download'):
            return self.download(argv, args)
        if verb == ('run', 'list'):
            assert (option(args, '--workflow'), option(args, '--event')) == ('build.yml', 'release'), argv
            self.listings += 1
            runs = list(self.runs)
            if self.new_run and self.listings > self.appears_after:
                runs.append(self.new_run)
            fields = option(args, '--json').split(',')
            return self.printed(argv, [{field: item[field] for field in fields} for item in reversed(runs)])
        if verb == ('run', 'view'):
            return self.view(argv, args)
        raise AssertionError('unexpected gh call: {}'.format(argv))

    def create(self, argv, args):
        tag = args[2]
        self.created = {'tag': tag, 'target': option(args, '--target'), 'title': option(args, '--title'),
                        'prerelease': '--prerelease' in args,
                        'notes': Path(option(args, '--notes-file')).read_text()}
        self.releases[tag] = self.release(tag, prerelease='--prerelease' in args)
        self.new_run = {'databaseId': 2, 'headBranch': tag, 'createdAt': '2026-10-15T09:00:02Z',
                        'url': 'https://github.com/kahgeh/harold/actions/runs/2'}
        self.events.append('create ' + tag)
        return done(argv, 0, self.releases[tag]['url'] + '\n')

    def view(self, argv, args):
        assert args[2] == '2', 'followed run {}, not the one this release started'.format(args[2])
        if self.failing_views:
            self.failing_views -= 1
            return done(argv, 1, '', 'HTTP 502: Bad Gateway\n')
        self.views += 1
        finished = self.views >= self.polls
        count = len(STEPS) if finished else len(STEPS) * self.views // self.polls
        steps = []
        for number, name in enumerate(STEPS, 1):
            if number <= count:
                last = finished and number == len(STEPS)
                steps.append({'name': name, 'number': number, 'status': 'completed',
                              'conclusion': self.outcome if last else 'success'})
            else:
                steps.append({'name': name, 'number': number, 'status': 'queued', 'conclusion': ''})
        if finished and self.views == self.polls:
            self.events.append('build ' + self.outcome)
            if self.outcome == 'success' and self.attach:
                self.releases[self.created['tag']]['assets'] = [
                    {'name': release.ARCHIVE}, {'name': release.CHECKSUM}]
        value = {'status': 'completed' if finished else 'in_progress',
                 'conclusion': self.outcome if finished else '',
                 'url': self.new_run['url'], 'jobs': [{'name': 'build', 'steps': steps}]}
        return self.printed(argv, {field: value[field] for field in option(args, '--json').split(',')})

    def download(self, argv, args):
        tag = args[2]
        self.events.append('download ' + tag)
        if tag not in self.releases:
            return done(argv, 1, '', 'release not found\n')
        folder = Path(option(args, '--dir'))
        for name, data in self.files.items():
            (folder / name).write_bytes(data)
        return done(argv, 0)


class ReleaseTestCase(unittest.TestCase):
    """A checkout whose `origin` is a local bare repository standing in for kahgeh/harold.

    git is real and runs only in the scratch folder, with the user's git configuration hidden.
    gh, the tests, the installer and haroldctl are fakes. Nothing contacts GitHub or touches ~/bin.
    """

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        (self.root / 'gitconfig').write_text('')
        self.patch(patch.dict(os.environ, {
            'GIT_CONFIG_GLOBAL': str(self.root / 'gitconfig'), 'GIT_CONFIG_NOSYSTEM': '1',
            'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.com',
            'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.com'}))
        self.origin = self.root / 'origin.git'
        git(self.root, 'init', '-q', '--bare', '-b', 'main', str(self.origin))
        self.work = self.root / 'work'
        git(self.root, 'init', '-q', '-b', 'main', str(self.work))
        git(self.work, 'remote', 'add', 'origin', str(self.origin))
        commit(self.work, 'First', 'README.md')
        git(self.work, 'tag', 'v0.5.0')
        commit(self.work, 'Second', 'second.txt')
        commit(self.work, 'Third', 'third.txt')
        git(self.work, 'push', '-q', 'origin', 'main', 'v0.5.0')
        # GitHub has the tag; this checkout gets it back only by fetching.
        git(self.work, 'tag', '-d', 'v0.5.0')
        self.github = FakeGitHub()
        self.clock = FakeClock()
        self.failing = None
        self.missing = ()
        self.calls = []
        self.prefix = self.root / 'prefix with spaces'
        self.installer = None
        self.installer_code = 0
        self.installed_version = None
        self.ready = True
        # What the command prints, kept out of the test output.
        self.stdout = io.StringIO()
        self.patch(patch.object(sys, 'stdout', self.stdout))
        self.patch(patch.object(release, 'run', self.fake_run))
        self.patch(patch.object(release, 'clock', self.clock))
        self.patch(patch.object(release, 'sleep', self.clock.sleep))
        self.patch(patch.object(release, 'DEFAULT_PREFIX', self.root / 'default prefix'))
        self.patch(patch.object(release.shutil, 'which',
                                lambda name: None if name in self.missing else '/usr/bin/' + name))

    def patch(self, patcher):
        patcher.start()
        self.addCleanup(patcher.stop)

    def fake_run(self, argv, cwd, inherit=False, merge=False):
        argv = [str(item) for item in argv]
        self.calls.append(argv)
        if argv[0] == 'git':
            return REAL_RUN(argv, cwd, inherit=inherit, merge=merge)
        if argv[0] == 'gh':
            return self.github(argv)
        if tuple(argv) in release.TESTS:
            failed = tuple(argv) == self.failing
            return done(argv, 1 if failed else 0, 'FAILED (failures=1)\n' if failed else 'OK\n', None)
        if argv[0] == sys.executable and argv[1].endswith('/scripts/install.py'):
            return self.fake_installer(argv)
        if argv[1:] == ['status'] and argv[0].endswith('/haroldctl'):
            status = {'ready': self.ready, 'pid': 16897 if self.ready else None, 'grpc_addr': '127.0.0.1:50060'}
            return done(argv, 0 if self.ready else 1, json.dumps(status) + '\n', '')
        raise AssertionError('unexpected command: {}'.format(argv))

    def fake_installer(self, argv):
        """Copies VERSION and REVISION as scripts/install.py does, or writes `installed_version`."""
        self.installer = argv
        installer = Path(argv[1])
        assert installer.is_file(), 'the installer was not extracted'
        package = installer.parents[1]
        bundle = Path(option(argv, '--prefix')) / 'harold'
        bundle.mkdir(parents=True, exist_ok=True)
        for name in ('VERSION', 'REVISION'):
            if (package / name).is_file():
                shutil.copy(str(package / name), str(bundle / name))
        if self.installed_version:
            (bundle / 'VERSION').write_text(self.installed_version + '\n')
        return done(argv, self.installer_code, None, None)

    def main(self, *argv):
        """Run release.py with `argv`; returns (exit status, stdout, stderr)."""
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = release.main(list(argv), cwd=self.work)
        return code, out.getvalue(), err.getvalue()

    def assert_refused(self, expected):
        """`release` exits 1 with exactly `expected` and changes nothing on GitHub."""
        code, _, err = self.main('release', '--prefix', str(self.prefix))
        self.assertEqual((code, err), (1, expected + '\n'))
        self.assertEqual(self.github.events, [])
        self.assertIsNone(self.installer)

    def pack(self, version='v0.6.0', checksum=None, entry=None):
        """Give the release the two files its build attaches: an archive holding the installer,
        VERSION (unless None) and REVISION, and its checksum (or `checksum` in its place).
        `entry` adds one more archive member with that name."""
        shutil.rmtree(str(self.root / 'pack'), ignore_errors=True)
        package = self.root / 'pack' / release.PACKAGE
        (package / 'scripts').mkdir(parents=True)
        (package / 'scripts/install.py').write_text('# the release installer\n')
        if version:
            (package / 'VERSION').write_text(version + '\n')
        (package / 'REVISION').write_text(REVISION + '\n')
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode='w:gz') as archive:
            archive.add(str(package), arcname=release.PACKAGE)
            if entry:
                extra = tarfile.TarInfo(entry)
                archive.addfile(extra, io.BytesIO(b''))
        data = buffer.getvalue()
        digest = checksum or hashlib.sha256(data).hexdigest()
        self.github.files = {release.ARCHIVE: data,
                             release.CHECKSUM: '{}  {}\n'.format(digest, release.ARCHIVE).encode()}


class CheckTests(ReleaseTestCase):
    def test_another_branch_is_refused(self):
        git(self.work, 'checkout', '-q', '-b', 'topic')
        self.assert_refused('Not released: this checkout is on topic, not main.')

    def test_a_modified_tracked_file_is_refused(self):
        (self.work / 'README.md').write_text('changed\n')
        self.assert_refused('Not released: uncommitted changes in README.md.')

    def test_a_staged_new_file_is_refused(self):
        (self.work / 'new.txt').write_text('new\n')
        git(self.work, 'add', 'new.txt')
        self.assert_refused('Not released: uncommitted changes in new.txt.')

    def test_only_the_first_three_changed_paths_are_named(self):
        for name in ('README.md', 'second.txt', 'third.txt'):
            (self.work / name).write_text('changed\n')
        (self.work / 'fourth.txt').write_text('new\n')
        git(self.work, 'add', 'fourth.txt')
        self.assert_refused('Not released: uncommitted changes in README.md, fourth.txt, second.txt.')

    def test_an_unpushed_commit_is_refused(self):
        commit(self.work, 'Fourth', 'fourth.txt')
        self.assert_refused('Not released: main is 1 ahead of and 0 behind origin/main. Push or pull first.')

    def test_a_commit_only_on_origin_is_refused(self):
        other = self.root / 'other'
        git(self.root, 'clone', '-q', str(self.origin), str(other))
        commit(other, 'Elsewhere', 'elsewhere.txt')
        git(other, 'push', '-q', 'origin', 'main')
        self.assert_refused('Not released: main is 0 ahead of and 1 behind origin/main. Push or pull first.')

    def test_signed_out_of_gh_is_refused(self):
        self.github.signed_in = False
        self.assert_refused('Not released: gh is not signed in to GitHub.')
        self.assertIn(['gh', 'auth', 'status', '--hostname', 'github.com'], self.calls)

    def test_an_existing_local_tag_is_refused(self):
        git(self.work, 'tag', 'v0.6.0')
        self.assert_refused('Not released: v0.6.0 already exists.')

    def test_a_tag_only_on_github_is_refused(self):
        # A tag on a commit that no branch holds, so fetching main does not bring it here.
        other = self.root / 'other'
        git(self.root, 'clone', '-q', str(self.origin), str(other))
        commit(other, 'Side', 'side.txt')
        git(other, 'push', '-q', 'origin', 'HEAD:refs/tags/v0.6.0')
        self.assert_refused('Not released: v0.6.0 already exists.')

    def test_a_failing_test_is_refused_with_its_output(self):
        self.failing = ('python3', '-m', 'unittest', 'discover', '-s', 'scripts/tests')
        self.assert_refused('Not released: tests failed in python3 -m unittest discover -s scripts/tests.\n'
                            'FAILED (failures=1)')
        ran = [tuple(argv) for argv in self.calls if tuple(argv) in release.TESTS]
        self.assertEqual(ran, list(release.TESTS[:3]))

    def test_the_tests_are_the_workflows_in_its_order(self):
        tag, latest = release.checks(self.work, None, None)
        self.assertEqual((tag, latest), ('v0.6.0', 'v0.5.0'))
        self.assertEqual([tuple(argv) for argv in self.calls if tuple(argv) in release.TESTS], list(release.TESTS))
        self.assertEqual(release.TESTS, (
            ('cargo', 'test', '--workspace', '--locked'),
            ('bash', 'scripts/check_features.sh'),
            ('python3', '-m', 'unittest', 'discover', '-s', 'scripts/tests'),
            ('python3', '-m', 'unittest', 'hooks.shared.harold_turn_complete_test'),
            ('python3', '-m', 'unittest', 'hooks.providers.test_turn_complete')))

    def test_every_failing_check_is_printed_and_no_test_runs(self):
        git(self.work, 'checkout', '-q', '-b', 'topic')
        (self.work / 'README.md').write_text('changed\n')
        self.github.signed_in = False
        self.assert_refused('Not released: this checkout is on topic, not main.\n'
                            'Not released: uncommitted changes in README.md.\n'
                            'Not released: gh is not signed in to GitHub.')
        self.assertFalse([argv for argv in self.calls if tuple(argv) in release.TESTS])

    def test_a_missing_tool_is_refused(self):
        self.missing = ('gh',)
        self.assert_refused('Not released: gh is not installed.')
        self.assertEqual(self.calls, [])

    def test_an_untracked_folder_is_not_a_change(self):
        (self.work / 'tasks').mkdir()
        (self.work / 'tasks/todo.md').write_text('later\n')
        self.assertEqual(release.checks(self.work, None, None), ('v0.6.0', 'v0.5.0'))

    def add_events_submodule(self):
        events = self.root / 'events.git'
        git(self.root, 'init', '-q', '--bare', '-b', 'main', str(events))
        source = self.root / 'events-source'
        git(self.root, 'init', '-q', '-b', 'main', str(source))
        commit(source, 'Events', 'Cargo.toml')
        git(source, 'push', '-q', str(events), 'main')
        git(self.work, '-c', 'protocol.file.allow=always', 'submodule', 'add', '-q', str(events), 'events')
        git(self.work, 'commit', '-q', '-m', 'Add events')
        git(self.work, 'push', '-q', 'origin', 'main')

    def test_a_change_inside_the_events_submodule_is_refused(self):
        self.add_events_submodule()
        (self.work / 'events/Cargo.toml').write_text('changed\n')
        self.assert_refused('Not released: uncommitted changes in events.')

    def test_an_untracked_file_in_the_submodule_is_not_a_change(self):
        self.add_events_submodule()
        (self.work / 'events/scratch.txt').write_text('mine\n')
        self.assertEqual(release.checks(self.work, None, None), ('v0.6.0', 'v0.5.0'))

    def test_a_version_not_higher_than_the_latest_is_refused(self):
        for version in ('0.5.0', '0.4.9', 'abc', '0.6', '0.6.0.1', ''):
            with self.subTest(version=version):
                code, _, err = self.main('release', '--version', version, '--prefix', str(self.prefix))
                self.assertEqual((code, err), (1, 'Not released: VERSION must be higher than v0.5.0.\n'))
                self.assertEqual(self.github.events, [])

    def test_a_given_version_is_used(self):
        self.assertEqual(release.checks(self.work, '0.5.1', None), ('v0.5.1', 'v0.5.0'))
        self.assertEqual(release.checks(self.work, 'v0.7.0', None), ('v0.7.0', 'v0.5.0'))

    def test_with_no_release_the_version_is_v0_1_0(self):
        self.github.releases = {}
        self.assertEqual(release.checks(self.work, None, None), ('v0.1.0', None))

    def test_an_unreadable_notes_file_is_refused(self):
        code, _, err = self.main('release', '--notes', 'missing.md', '--prefix', str(self.prefix))
        self.assertEqual((code, err), (1, 'Not released: NOTES file missing.md cannot be read.\n'))
        self.assertEqual(self.github.events, [])

    def test_a_notes_file_that_is_not_utf_8_is_refused(self):
        (self.work / 'notes.md').write_bytes(b'\xff\xfe')
        git(self.work, 'add', 'notes.md')
        git(self.work, 'commit', '-q', '-m', 'Notes')
        git(self.work, 'push', '-q', 'origin', 'main')
        code, _, err = self.main('release', '--notes', 'notes.md', '--prefix', str(self.prefix))
        self.assertEqual((code, err), (1, 'Not released: NOTES file notes.md cannot be read.\n'))
        self.assertEqual(self.github.events, [])


class VersionTests(unittest.TestCase):
    def test_the_middle_number_is_raised_and_the_last_set_to_zero(self):
        self.assertEqual(release.new_tag('v0.5.0', None), 'v0.6.0')
        self.assertEqual(release.new_tag('v0.9.3', None), 'v0.10.0')
        self.assertEqual(release.new_tag('v1.2.3', None), 'v1.3.0')

    def test_no_release_means_v0_1_0(self):
        self.assertEqual(release.new_tag(None, None), 'v0.1.0')

    def test_a_requested_version_must_be_three_numbers_and_higher(self):
        self.assertEqual(release.new_tag('v0.5.0', '0.5.1'), 'v0.5.1')
        self.assertEqual(release.new_tag('v0.5.0', 'v1.0.0'), 'v1.0.0')
        self.assertEqual(release.new_tag(None, '0.0.1'), 'v0.0.1')
        for requested in ('0.5.0', '0.4.10', 'abc', '1.2', '1.2.3.4', 'v', ' 0.6.0'):
            with self.subTest(requested=requested):
                self.assertIsNone(release.new_tag('v0.5.0', requested))


class NotesTests(ReleaseTestCase):
    def test_the_notes_are_the_subjects_since_the_previous_tag_oldest_first(self):
        tag, latest = release.checks(self.work, None, None)
        self.assertEqual(release.release_notes(self.work, latest, None), 'Second\nThird\n')

    def test_the_previous_tag_is_fetched_when_this_checkout_lacks_it(self):
        self.assertEqual(release.release_notes(self.work, 'v0.5.0', None), 'Second\nThird\n')

    def test_with_no_previous_release_every_subject_is_listed(self):
        self.assertEqual(release.release_notes(self.work, None, None), 'First\nSecond\nThird\n')

    def test_a_notes_file_is_used_as_it_is(self):
        (self.work / 'notes.md').write_text('# Harold v0.6.0\n\nFaster agent monitor.\n')
        self.assertEqual(release.release_notes(self.work, 'v0.5.0', 'notes.md'),
                         '# Harold v0.6.0\n\nFaster agent monitor.\n')


class BuildTests(ReleaseTestCase):
    def publish(self):
        sha = git(self.work, 'rev-parse', 'HEAD').strip()
        with contextlib.redirect_stdout(io.StringIO()):
            url, published = release.publish(self.work, 'v0.6.0', sha, 'Second\nThird\n')
        return sha, url, published

    def wait(self):
        _, url, published = self.publish()
        printed = io.StringIO()
        with contextlib.redirect_stdout(printed):
            release.wait_for_build(self.work, 'v0.6.0', url, published)
        return printed.getvalue()

    def assert_failed_build(self, what, url):
        with self.assertRaises(release.Stopped) as failed:
            self.wait()
        self.assertEqual(str(failed.exception), '\n'.join([
            'The build {}: {}'.format(what, url),
            'v0.6.0 stays a pre-release with no build, and nothing was installed. Either:',
            '  withdraw it: gh release delete v0.6.0 --cleanup-tag --yes',
            '  or re-run the workflow on GitHub; when it succeeds, mark it latest'
            ' (gh release edit v0.6.0 --prerelease=false --latest)',
            '    and install it: make install-release VERSION=0.6.0']))
        self.assertTrue(self.github.releases['v0.6.0']['isPrerelease'])
        self.assertNotIn('edit v0.6.0', self.github.events)

    def test_the_release_is_a_pre_release_of_head_titled_by_its_tag(self):
        sha, url, published = self.publish()
        self.assertEqual(self.github.created, {'tag': 'v0.6.0', 'target': sha, 'title': 'Harold v0.6.0',
                                               'prerelease': True, 'notes': 'Second\nThird\n'})
        self.assertEqual((url, published), ('https://github.com/kahgeh/harold/releases/tag/v0.6.0', PUBLISHED))
        self.assertTrue(self.github.releases['v0.5.0']['isLatest'])
        self.assertFalse(self.github.releases['v0.6.0']['isLatest'])

    def test_a_failed_view_after_creating_says_the_release_is_public(self):
        self.github.failing_release_views = ('url,publishedAt',)
        with self.assertRaises(release.Stopped) as failed:
            self.publish()
        self.assertEqual(str(failed.exception), '\n'.join([
            'v0.6.0 was created as a pre-release, but gh release view failed: HTTP 502: Bad Gateway',
            '  withdraw it: gh release delete v0.6.0 --cleanup-tag --yes',
            '  or, when its build on GitHub succeeds, mark it latest'
            ' (gh release edit v0.6.0 --prerelease=false --latest)',
            '    and install it: make install-release VERSION=0.6.0']))
        self.assertTrue(self.github.releases['v0.6.0']['isPrerelease'])

    def test_each_step_is_printed_once_as_it_finishes(self):
        output = self.wait()
        steps = [line for line in output.splitlines() if line.startswith('  build: ')]
        self.assertEqual(steps, ['  build: {} (success)'.format(name) for name in STEPS])
        self.assertIn('Following the build: https://github.com/kahgeh/harold/actions/runs/2\n', output)

    def test_an_older_run_on_the_same_tag_is_not_followed(self):
        # FakeGitHub fails the test if run 1, from the withdrawn v0.6.0, is viewed.
        self.github.appears_after = 3
        self.wait()
        self.assertEqual(self.github.listings, 4)

    def test_a_failed_poll_is_retried(self):
        self.github.failing_views = 2
        self.wait()
        self.assertEqual(self.github.events[-1], 'build success')

    def test_a_failed_build_names_the_run_and_both_ways_on(self):
        self.github.outcome = 'failure'
        self.assert_failed_build('ended in failure', 'https://github.com/kahgeh/harold/actions/runs/2')

    def test_a_build_past_45_minutes_is_a_failed_build(self):
        self.github.polls = 10 ** 6
        self.assert_failed_build('did not finish within 45 minutes',
                                 'https://github.com/kahgeh/harold/actions/runs/2')
        self.assertGreaterEqual(self.clock.now, release.BUILD_LIMIT)
        self.assertLess(self.clock.now, release.BUILD_LIMIT + 2 * release.POLL)

    def test_a_run_not_found_within_2_minutes_names_the_release(self):
        self.github.appears_after = 10 ** 6
        self.assert_failed_build('was not found within 2 minutes',
                                 'https://github.com/kahgeh/harold/releases/tag/v0.6.0')
        self.assertGreaterEqual(self.clock.now, release.FIND_LIMIT)
        self.assertLess(self.clock.now, release.FIND_LIMIT + 2 * release.POLL)

    def test_a_build_without_both_files_is_a_failed_build(self):
        self.github.attach = False
        self.assert_failed_build('finished without both files attached',
                                 'https://github.com/kahgeh/harold/actions/runs/2')

    def test_files_that_cannot_be_read_after_the_build_are_a_failed_build(self):
        self.github.failing_release_views = ('assets',)
        self.assert_failed_build('finished, but its files could not be read',
                                 'https://github.com/kahgeh/harold/actions/runs/2')

    def test_marking_latest_clears_the_pre_release_mark(self):
        self.wait()
        with contextlib.redirect_stdout(io.StringIO()):
            release.mark_latest(self.work, 'v0.6.0')
        self.assertEqual(self.github.events, ['create v0.6.0', 'build success', 'edit v0.6.0'])
        self.assertFalse(self.github.releases['v0.6.0']['isPrerelease'])
        self.assertTrue(self.github.releases['v0.6.0']['isLatest'])
        self.assertFalse(self.github.releases['v0.5.0']['isLatest'])


class CommandTests(ReleaseTestCase):
    def setUp(self):
        super().setUp()
        self.pack()

    def last_line(self, out):
        return out.rstrip('\n').splitlines()[-1]

    def test_a_release_is_published_built_marked_latest_and_installed(self):
        code, out, err = self.main('release', '--prefix', str(self.prefix))
        self.assertEqual(code, 0, err)
        self.assertEqual(self.github.events, ['create v0.6.0', 'build success', 'edit v0.6.0', 'download v0.6.0'])
        self.assertEqual(self.github.created['notes'], 'Second\nThird\n')
        self.assertEqual(self.last_line(out), 'Harold v0.6.0 (31323b9) is running, PID 16897.')

    def test_the_installer_is_the_archives_own_run_with_this_python(self):
        self.main('release', '--prefix', str(self.prefix))
        package = Path(self.installer[1]).parents[1]
        self.assertEqual(self.installer, [sys.executable, str(package / 'scripts/install.py'),
                                          '--prebuilt-dir', str(package), '--prefix', str(self.prefix)])
        self.assertEqual(package.name, release.PACKAGE)

    def test_a_signing_identity_reaches_the_installer_from_both_commands(self):
        # The release command creates v0.6.0 on the fake GitHub; the install command then installs it.
        code, _, err = self.main('release', '--prefix', str(self.prefix), '--signing-identity', 'Some Cert')
        self.assertEqual(code, 0, err)
        self.assertEqual(self.installer[-2:], ['--signing-identity', 'Some Cert'])
        self.installer = None
        code, _, err = self.main('install', '--version', '0.6.0', '--prefix', str(self.prefix),
                                 '--signing-identity', 'Some Cert')
        self.assertEqual(code, 0, err)
        self.assertEqual(self.installer[-2:], ['--signing-identity', 'Some Cert'])

    def test_without_a_signing_identity_none_is_passed(self):
        self.main('release', '--prefix', str(self.prefix))
        self.assertNotIn('--signing-identity', self.installer)

    def test_with_no_prefix_the_default_prefix_is_used(self):
        code, out, err = self.main('release')
        self.assertEqual(code, 0, err)
        self.assertEqual(option(self.installer, '--prefix'), str(self.root / 'default prefix'))

    def test_a_notes_file_becomes_the_release_notes(self):
        (self.work / 'notes.md').write_text('Faster agent monitor.\n')
        git(self.work, 'add', 'notes.md')
        git(self.work, 'commit', '-q', '-m', 'Notes')
        git(self.work, 'push', '-q', 'origin', 'main')
        code, _, err = self.main('release', '--notes', 'notes.md', '--prefix', str(self.prefix))
        self.assertEqual(code, 0, err)
        self.assertEqual(self.github.created['notes'], 'Faster agent monitor.\n')

    def test_a_failed_build_downloads_and_installs_nothing(self):
        self.github.outcome = 'failure'
        code, _, err = self.main('release', '--prefix', str(self.prefix))
        self.assertEqual(code, 1)
        self.assertEqual(self.github.events, ['create v0.6.0', 'build failure'])
        self.assertTrue(self.github.releases['v0.6.0']['isPrerelease'])
        self.assertIsNone(self.installer)
        self.assertIn('https://github.com/kahgeh/harold/actions/runs/2', err)
        self.assertIn('gh release delete v0.6.0 --cleanup-tag --yes', err)
        self.assertIn('gh release edit v0.6.0 --prerelease=false --latest', err)
        self.assertIn('make install-release VERSION=0.6.0', err)

    def test_a_checksum_mismatch_runs_no_installer(self):
        self.pack(checksum='0' * 64)
        code, _, err = self.main('release', '--prefix', str(self.prefix))
        self.assertEqual(code, 1)
        self.assertIsNone(self.installer)
        self.assertEqual(err, 'Not installed: the archive of v0.6.0 does not match its checksum.\n'
                              'v0.6.0 is on GitHub; try again with: make install-release VERSION=0.6.0\n')
        self.assertTrue(self.github.releases['v0.6.0']['isLatest'])

    def test_a_checksum_file_that_is_not_text_runs_no_installer(self):
        self.github.files[release.CHECKSUM] = b'\xff\xfe'
        code, _, err = self.main('release', '--prefix', str(self.prefix))
        self.assertEqual(code, 1)
        self.assertIsNone(self.installer)
        self.assertEqual(err, 'Not installed: the checksum file of v0.6.0 is not valid.\n'
                              'v0.6.0 is on GitHub; try again with: make install-release VERSION=0.6.0\n')

    def test_an_archive_entry_outside_the_package_runs_no_installer(self):
        self.github.releases['v0.6.0'] = self.github.release('v0.6.0', assets=(release.ARCHIVE, release.CHECKSUM))
        for entry in ('../escape', '/etc/escape', 'other/file'):
            with self.subTest(entry=entry):
                self.pack(entry=entry)
                code, _, err = self.main('install', '--version', '0.6.0', '--prefix', str(self.prefix))
                self.assertEqual(code, 1)
                self.assertIsNone(self.installer)
                self.assertIn('unsafe entry in the archive of v0.6.0', err)

    def test_a_failed_installer_exits_1_and_says_how_to_retry(self):
        self.installer_code = 1
        code, out, err = self.main('release', '--prefix', str(self.prefix))
        self.assertEqual((code, err), (1, 'v0.6.0 is on GitHub; try again with: make install-release VERSION=0.6.0\n'))
        self.assertNotIn('is running', out)

    def test_an_installed_version_other_than_the_tag_is_a_failure(self):
        self.installed_version = 'v0.5.0'
        code, _, err = self.main('release', '--prefix', str(self.prefix))
        self.assertEqual((code, err), (1, 'Installed VERSION is v0.5.0, not v0.6.0.\n'))

    def test_harold_not_ready_is_a_failure(self):
        self.ready = False
        code, _, err = self.main('release', '--prefix', str(self.prefix))
        self.assertEqual(code, 1)
        self.assertTrue(err.startswith('Harold is not ready: haroldctl status said {"ready": false'), err)

    def test_install_release_makes_no_check_and_publishes_nothing(self):
        self.github.releases['v0.6.0'] = self.github.release('v0.6.0', prerelease=True,
                                                             assets=(release.ARCHIVE, release.CHECKSUM))
        code, out, err = self.main('install', '--version', '0.6.0', '--prefix', str(self.prefix))
        self.assertEqual(code, 0, err)
        self.assertEqual(self.github.events, ['download v0.6.0'])
        self.assertFalse([argv for argv in self.calls if argv[0] == 'git'])
        self.assertFalse([argv for argv in self.calls if argv[:2] == ['gh', 'auth']])
        self.assertFalse([argv for argv in self.calls if tuple(argv) in release.TESTS])
        self.assertEqual(self.last_line(out), 'Harold v0.6.0 (31323b9) is running, PID 16897.')

    def test_install_release_with_no_version_installs_the_latest(self):
        self.pack(version=None)
        code, out, err = self.main('install', '--prefix', str(self.prefix))
        self.assertEqual(code, 0, err)
        self.assertEqual(self.github.events, ['download v0.5.0'])
        self.assertEqual(self.last_line(out), 'Harold (version not recorded) (31323b9) is running, PID 16897.')

    def test_install_release_refuses_a_version_that_is_not_three_numbers(self):
        code, _, err = self.main('install', '--version', 'abc', '--prefix', str(self.prefix))
        self.assertEqual((code, err), (1, 'Not installed: VERSION must be three numbers, such as 0.6.0.\n'))
        self.assertEqual(self.github.events, [])

    def test_ctrl_c_while_waiting_leaves_the_pre_release(self):
        def interrupt(seconds):
            raise KeyboardInterrupt
        self.github.appears_after = 1
        with patch.object(release, 'sleep', interrupt):
            code, _, err = self.main('release', '--prefix', str(self.prefix))
        self.assertEqual(code, 130)
        self.assertEqual(self.github.events, ['create v0.6.0'])
        self.assertTrue(self.github.releases['v0.6.0']['isPrerelease'])
        self.assertIsNone(self.installer)
        self.assertIn('v0.6.0 stays a pre-release and its build continues on GitHub', err)
        self.assertIn('make install-release VERSION=0.6.0', err)
        self.assertIn('gh release delete v0.6.0 --cleanup-tag --yes', err)


OPTIONS = ('VERSION', 'NOTES', 'PREFIX', 'SIGNING_IDENTITY')


@unittest.skipUnless(shutil.which('make'), 'make is not installed')
class MakefileTests(unittest.TestCase):
    """The make targets pass their options to release.py, each as one argument.

    make exports variables given on its command line to the recipe's shell; the recipe turns
    each one that is set into an option."""

    def make(self, *arguments):
        with tempfile.TemporaryDirectory() as folder:
            stub = Path(folder) / 'python'
            stub.write_text('#!/bin/sh\nprintf \'%s\\n\' "$@" > "$ARGUMENTS"\n')
            stub.chmod(0o755)
            env = {key: value for key, value in os.environ.items()
                   if key not in OPTIONS + ('MAKEFLAGS', 'MFLAGS', 'MAKELEVEL')}
            env['ARGUMENTS'] = str(Path(folder) / 'arguments')
            result = subprocess.run(['make', '-s', '-C', str(release.REPO_ROOT), 'PYTHON=' + str(stub)]
                                    + list(arguments), env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            return (Path(folder) / 'arguments').read_text().splitlines()

    def test_release_passes_every_option_given(self):
        self.assertEqual(
            self.make('release', 'VERSION=0.5.1', 'NOTES=notes.md', 'PREFIX=/tmp/a prefix',
                      'SIGNING_IDENTITY=Some Cert'),
            ['scripts/release.py', 'release', '--version', '0.5.1', '--notes', 'notes.md',
             '--prefix', '/tmp/a prefix', '--signing-identity', 'Some Cert'])

    def test_release_with_no_option_passes_none(self):
        self.assertEqual(self.make('release'), ['scripts/release.py', 'release'])

    def test_install_release_passes_its_options_and_not_notes(self):
        self.assertEqual(
            self.make('install-release', 'VERSION=0.6.0', 'NOTES=notes.md', 'SIGNING_IDENTITY=Some Cert'),
            ['scripts/release.py', 'install', '--version', '0.6.0', '--signing-identity', 'Some Cert'])

    def test_an_empty_signing_identity_passes_none(self):
        self.assertEqual(self.make('install-release', 'SIGNING_IDENTITY='), ['scripts/release.py', 'install'])


class NothingRunsItTests(unittest.TestCase):
    """Only a person runs make release and make install-release."""

    PATTERN = re.compile(r'make release|install-release|release\.py')

    def test_no_hook_workflow_or_script_mentions_them(self):
        own = {SCRIPTS / 'release.py', SCRIPTS / 'tests/test_release.py'}
        found = []
        for folder in ('hooks', '.github', 'scripts'):
            for path in sorted((release.REPO_ROOT / folder).rglob('*')):
                if (not path.is_file() or path in own
                        or {'__pycache__', 'node_modules'} & set(path.parts)):
                    continue
                for number, line in enumerate(path.read_text(errors='ignore').splitlines(), 1):
                    if self.PATTERN.search(line):
                        found.append('{}:{}'.format(path.relative_to(release.REPO_ROOT), number))
        self.assertEqual(found, [])

    def test_no_other_make_target_runs_them(self):
        targets, found = [], []
        lines = (release.REPO_ROOT / 'Makefile').read_text().splitlines()
        for number, line in enumerate(lines, 1):
            rule = re.match(r'([^\s:=#][^:=]*):(?!=)(.*)', line)
            if rule:
                targets = rule.group(1).split()
                body = rule.group(2)
            elif line.startswith('\t'):
                body = line
            else:
                continue
            if set(targets) - {'release', 'install-release', '.PHONY'} and self.PATTERN.search(body):
                found.append('Makefile:{}'.format(number))
        self.assertEqual(found, [])


if __name__ == '__main__':
    unittest.main()
