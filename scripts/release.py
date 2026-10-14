#!/usr/bin/env python3
"""Cut a Harold release on GitHub and install it on this Mac, or install a release that exists.

`release` checks this checkout, publishes HEAD as a pre-release, follows the build that starts,
marks the release latest and installs it. `install` installs a release that already exists.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time

REPO = 'kahgeh/harold'
REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PREFIX = Path.home() / 'bin'
WORKFLOW = 'build.yml'
PACKAGE = 'harold-aarch64-apple-darwin'
ARCHIVE = PACKAGE + '.tar.gz'
CHECKSUM = ARCHIVE + '.sha256'
# The tests the build workflow runs, in its order.
TESTS = (('cargo', 'test', '--workspace', '--locked'),
         ('python3', '-m', 'unittest', 'discover', '-s', 'scripts/tests'),
         ('python3', '-m', 'unittest', 'hooks.shared.harold_turn_complete_test'),
         ('python3', '-m', 'unittest', 'hooks.providers.test_turn_complete'))
BUILD_LIMIT = 45 * 60  # the workflow's timeout-minutes
FIND_LIMIT = 2 * 60
POLL = 10
VERSION_PATTERN = re.compile(r'v?(\d+)\.(\d+)\.(\d+)')
# The tests replace these, so that no test waits.
clock = time.monotonic
sleep = time.sleep


class Stopped(Exception):
    """A step failed. The message, when there is one, says which step and why."""


class Interrupted(Exception):
    """Ctrl-C while waiting for the build; the message says what is left on GitHub."""


def run(argv, cwd, inherit=False, merge=False):
    """Run `argv` in `cwd` and return the finished process; a missing program exits 127.

    `inherit` leaves the terminal connected; otherwise output is captured, with stderr in
    stdout when `merge` is set.
    """
    argv = [str(item) for item in argv]
    streams = {} if inherit else {'stdout': subprocess.PIPE,
                                  'stderr': subprocess.STDOUT if merge else subprocess.PIPE}
    try:
        return subprocess.run(argv, cwd=str(cwd), text=True, **streams)
    except FileNotFoundError:
        text = '{} is not installed\n'.format(argv[0])
        return subprocess.CompletedProcess(argv, 127, text if merge else '', None if merge else text)


def git(cwd, *args):
    """What `git args` prints. git runs only before anything is published, so a failure is a refusal."""
    result = run(('git',) + args, cwd)
    if result.returncode:
        raise Stopped('Not released: git {} failed: {}'.format(' '.join(args), result.stderr.strip()))
    return result.stdout


def gh_json(cwd, *args):
    """The JSON that `gh args --repo kahgeh/harold` prints."""
    result = run(('gh',) + args + ('--repo', REPO), cwd)
    if result.returncode:
        raise Stopped('gh {} failed: {}'.format(' '.join(args[:2]), result.stderr.strip()))
    try:
        return json.loads(result.stdout)
    except ValueError:
        raise Stopped('gh {} printed something that is not JSON'.format(' '.join(args[:2]))) from None


def parse_version(text):
    """(major, minor, patch) for `0.6.0` or `v0.6.0`; None for anything else."""
    match = VERSION_PATTERN.fullmatch(text or '')
    return tuple(int(part) for part in match.groups()) if match else None


def tag_of(numbers):
    return 'v{}.{}.{}'.format(*numbers)


def new_tag(latest, requested):
    """The tag to release, or None when `requested` is not three numbers higher than `latest`.

    With nothing requested it is `latest` with its middle number raised and its last set to 0.
    """
    current = parse_version(latest)
    if requested is None:
        return tag_of((current[0], current[1] + 1, 0)) if current else 'v0.1.0'
    wanted = parse_version(requested)
    if wanted is None or (current is not None and wanted <= current):
        return None
    return tag_of(wanted)


def latest_release(cwd):
    """The tag of the release GitHub marks latest, or None when no release is."""
    releases = gh_json(cwd, 'release', 'list', '--limit', '100', '--json', 'tagName,isLatest')
    return next((item['tagName'] for item in releases if item.get('isLatest')), None)


def branch_check(cwd):
    branch = git(cwd, 'rev-parse', '--abbrev-ref', 'HEAD').strip()
    if branch != 'main':
        return 'Not released: this checkout is on {}, not main.'.format(branch)
    return None


def changes_check(cwd):
    """Tracked files modified or staged, the events submodule's included. Untracked files are
    not in the pushed commit the release is built from, so they do not count."""
    output = git(cwd, 'status', '--porcelain', '--untracked-files=no', '--ignore-submodules=untracked')
    paths = [line[3:] for line in output.splitlines() if line]
    if paths:
        return 'Not released: uncommitted changes in {}.'.format(', '.join(paths[:3]))
    return None


def origin_check(cwd):
    git(cwd, 'fetch', '--quiet', 'origin', 'main')
    ahead, behind = git(cwd, 'rev-list', '--left-right', '--count', 'HEAD...origin/main').split()
    if (ahead, behind) != ('0', '0'):
        return 'Not released: main is {} ahead of and {} behind origin/main. Push or pull first.'.format(
            ahead, behind)
    return None


def signed_in_check(cwd):
    if run(('gh', 'auth', 'status'), cwd).returncode:
        return 'Not released: gh is not signed in to GitHub.'
    return None


def tag_check(cwd, tag):
    """Refuses a tag that exists in this checkout or on GitHub."""
    local = run(('git', 'rev-parse', '--quiet', '--verify', 'refs/tags/' + tag), cwd).returncode == 0
    if local or git(cwd, 'ls-remote', '--tags', 'origin', 'refs/tags/' + tag).strip():
        return 'Not released: {} already exists.'.format(tag)
    return None


def notes_check(cwd, notes):
    """Refuses a NOTES file that is missing, unreadable or not UTF-8."""
    if not notes:
        return None
    try:
        (Path(cwd) / notes).read_text(encoding='utf-8')
    except (OSError, ValueError):
        return 'Not released: NOTES file {} cannot be read.'.format(notes)
    return None


def checks(cwd, version, notes):
    """Step 1, before anything is public. Returns the new tag and the latest release's tag.

    Raises Stopped with one line for each failing check. The tests take minutes, so they run
    only when every other check passes, and the first failing test command stops them.
    """
    missing = [tool for tool in ('git', 'gh') if shutil.which(tool) is None]
    if missing:
        raise Stopped('\n'.join('Not released: {} is not installed.'.format(tool) for tool in missing))
    failures = []

    def passes(check, *args):
        try:
            message = check(cwd, *args)
        except Stopped as error:
            message = str(error)
        if message:
            failures.append(message)
        return not message

    on_main = passes(branch_check)
    passes(changes_check)
    if on_main:
        passes(origin_check)
    tag = latest = None
    if passes(signed_in_check):
        try:
            latest = latest_release(cwd)
        except Stopped as error:
            failures.append('Not released: {}'.format(error))
        else:
            tag = new_tag(latest, version)
            if tag is None:
                failures.append('Not released: VERSION must be higher than {}.'.format(latest or 'v0.0.0'))
            else:
                passes(tag_check, tag)
    passes(notes_check, notes)
    if failures:
        raise Stopped('\n'.join(failures))
    for command in TESTS:
        print('Testing: ' + ' '.join(command), flush=True)
        result = run(command, cwd, merge=True)
        if result.returncode:
            raise Stopped('Not released: tests failed in {}.\n{}'.format(' '.join(command), result.stdout.rstrip()))
    return tag, latest


def release_notes(cwd, latest, notes):
    """The text of the NOTES file, or the subject of every commit since `latest`, oldest first."""
    if notes:
        return (Path(cwd) / notes).read_text(encoding='utf-8')
    if latest is None:
        return git(cwd, 'log', '--reverse', '--format=%s', 'HEAD')
    if run(('git', 'rev-parse', '--quiet', '--verify', 'refs/tags/' + latest), cwd).returncode:
        git(cwd, 'fetch', '--quiet', 'origin', 'refs/tags/{0}:refs/tags/{0}'.format(latest))
    return git(cwd, 'log', '--reverse', '--format=%s', latest + '..HEAD')


def publish(cwd, tag, sha, notes):
    """Step 4: create the release of `sha` as a pre-release, which GitHub's latest-release link
    skips while it has no build. Returns the release's URL and when it was published."""
    with tempfile.TemporaryDirectory(prefix='harold-notes-') as folder:
        path = Path(folder) / 'notes.md'
        path.write_text(notes, encoding='utf-8')
        result = run(('gh', 'release', 'create', tag, '--repo', REPO, '--target', sha,
                      '--title', 'Harold ' + tag, '--prerelease', '--notes-file', path), cwd)
    if result.returncode:
        raise Stopped('Not released: gh release create failed: {}'.format(result.stderr.strip()))
    try:
        created = gh_json(cwd, 'release', 'view', tag, '--json', 'url,publishedAt')
    except Stopped as error:
        raise Stopped('\n'.join([
            '{} was created as a pre-release, but {}'.format(tag, error),
            '  withdraw it: gh release delete {} --cleanup-tag --yes'.format(tag),
            '  or, when its build on GitHub succeeds: make install-release VERSION={}'.format(tag[1:])])) from None
    print('Published {} as a pre-release: {}'.format(tag, created['url']), flush=True)
    return created['url'], created['publishedAt']


def build_failed(tag, url, what):
    return '\n'.join([
        'The build {}: {}'.format(what, url),
        '{} stays a pre-release with no build, and nothing was installed. Either:'.format(tag),
        '  withdraw it: gh release delete {} --cleanup-tag --yes'.format(tag),
        '  or re-run the workflow on GitHub, then: make install-release VERSION={}'.format(tag[1:])])


def poll(cwd, *args):
    """gh_json while waiting: a failed call, such as a network blip, returns None and the
    next poll tries again."""
    try:
        return gh_json(cwd, *args)
    except Stopped:
        return None


def find_run(cwd, tag, published, deadline):
    """The run this release started: a release run of the workflow on `tag`, created at or after
    `published`. An earlier run on the same tag, from a withdrawn release, is older. None by
    `deadline`."""
    while True:
        runs = poll(cwd, 'run', 'list', '--workflow', WORKFLOW, '--event', 'release', '--limit', '20',
                    '--json', 'databaseId,headBranch,createdAt,url') or []
        started = [item for item in runs if item['headBranch'] == tag and item['createdAt'] >= published]
        if started:
            return max(started, key=lambda item: item['createdAt'])
        if clock() >= deadline:
            return None
        sleep(POLL)


def follow(cwd, run_id, deadline):
    """Print each job step once, as it finishes. Returns the run's conclusion, or None when it
    has not finished by `deadline`."""
    printed = set()
    while True:
        view = poll(cwd, 'run', 'view', str(run_id), '--json', 'status,conclusion,jobs')
        for job in (view or {}).get('jobs', []):
            for step in job.get('steps', []):
                key = (job['name'], step['number'])
                if step.get('status') == 'completed' and key not in printed:
                    printed.add(key)
                    print('  {}: {} ({})'.format(job['name'], step['name'], step['conclusion']), flush=True)
        if view and view.get('status') == 'completed':
            return view.get('conclusion')
        if clock() >= deadline:
            return None
        sleep(POLL)


def wait_for_build(cwd, tag, release_url, published):
    """Step 5. Returns when the run succeeded and both files are attached to the release."""
    start = clock()
    found = find_run(cwd, tag, published, start + FIND_LIMIT)
    if found is None:
        raise Stopped(build_failed(tag, release_url, 'was not found within 2 minutes'))
    print('Following the build: ' + found['url'], flush=True)
    conclusion = follow(cwd, found['databaseId'], start + BUILD_LIMIT)
    if conclusion is None:
        raise Stopped(build_failed(tag, found['url'], 'did not finish within 45 minutes'))
    if conclusion != 'success':
        raise Stopped(build_failed(tag, found['url'], 'ended in {}'.format(conclusion)))
    try:
        assets = gh_json(cwd, 'release', 'view', tag, '--json', 'assets')['assets']
    except Stopped:
        raise Stopped(build_failed(tag, found['url'], 'finished, but its files could not be read')) from None
    if not {ARCHIVE, CHECKSUM} <= {asset['name'] for asset in assets}:
        raise Stopped(build_failed(tag, found['url'], 'finished without both files attached'))


def mark_latest(cwd, tag):
    """Step 6: clear the pre-release mark and make this the latest release."""
    result = run(('gh', 'release', 'edit', tag, '--repo', REPO, '--prerelease=false', '--latest'), cwd)
    if result.returncode:
        raise Stopped('{} is built, but gh release edit failed: {}\n'
                      'Mark it latest on GitHub, then: make install-release VERSION={}'.format(
                          tag, result.stderr.strip(), tag[1:]))
    print('{} is the latest release.'.format(tag), flush=True)


def retry(tag):
    return '{} is on GitHub; try again with: make install-release VERSION={}'.format(tag, tag[1:])


def download(cwd, tag, folder):
    """Step 7: fetch the archive and its checksum into `folder`, check the archive against it,
    and extract it with the path checks bootstrap.sh applies. Returns the extracted package."""
    result = run(('gh', 'release', 'download', tag, '--repo', REPO, '--pattern', ARCHIVE,
                  '--pattern', CHECKSUM, '--dir', folder), cwd)
    if result.returncode:
        raise Stopped('Not installed: gh release download failed: {}\n{}'.format(result.stderr.strip(), retry(tag)))
    try:
        # Bytes that are not text fail the format check below, not the read.
        text = (folder / CHECKSUM).read_text(errors='replace')
        data = (folder / ARCHIVE).read_bytes()
    except OSError as error:
        raise Stopped('Not installed: {} has no {}.\n{}'.format(tag, Path(error.filename).name, retry(tag))) from None
    match = re.fullmatch(r'([0-9a-fA-F]{64})  ' + re.escape(ARCHIVE) + r'\n?', text)
    if match is None:
        raise Stopped('Not installed: the checksum file of {} is not valid.\n{}'.format(tag, retry(tag)))
    if hashlib.sha256(data).hexdigest() != match.group(1).lower():
        raise Stopped('Not installed: the archive of {} does not match its checksum.\n{}'.format(tag, retry(tag)))
    try:
        with tarfile.open(folder / ARCHIVE) as archive:
            for item in archive.getmembers():
                path = PurePosixPath(item.name)
                if (path.is_absolute() or '..' in path.parts or not path.parts
                        or path.parts[0] != PACKAGE or not (item.isfile() or item.isdir())):
                    raise Stopped('Not installed: unsafe entry in the archive of {}: {}'.format(tag, item.name))
            # Python 3.12 and later warn without a filter; every entry is checked above.
            if hasattr(tarfile, 'data_filter'):
                archive.extractall(folder, filter='data')
            else:
                archive.extractall(folder)
    except tarfile.TarError as error:
        raise Stopped('Not installed: the archive of {} cannot be read: {}\n{}'.format(tag, error, retry(tag))) from None
    return folder / PACKAGE


def run_installer(cwd, package, prefix, identity):
    """Run the release's own installer with the Python running this command. The installer
    prints its own failure, so a failure here adds nothing."""
    command = [sys.executable, package / 'scripts/install.py', '--prebuilt-dir', package, '--prefix', prefix]
    if identity:
        command += ['--signing-identity', identity]
    if run(command, cwd, inherit=True).returncode:
        raise Stopped('')


def installed(bundle, name):
    """The text of the installed bundle's VERSION or REVISION file, or None."""
    try:
        return (bundle / name).read_text().strip() or None
    except OSError:
        return None


def report(cwd, prefix, tag):
    """Step 8: the line naming what runs, or Stopped naming what is wrong.

    A bundle without VERSION (releases up to v0.5.0) is reported as not recorded, not as wrong.
    """
    bundle = prefix / 'harold'
    version, revision = installed(bundle, 'VERSION'), installed(bundle, 'REVISION')
    result = run((prefix / 'haroldctl', 'status'), cwd)
    try:
        status = json.loads(result.stdout)
    except ValueError:
        status = None
    problems = []
    if version is not None and version != tag:
        problems.append('Installed VERSION is {}, not {}.'.format(version, tag))
    if not (isinstance(status, dict) and status.get('ready') is True):
        said = ((result.stdout or '') + (result.stderr or '')).strip() or 'nothing'
        problems.append('Harold is not ready: haroldctl status said {}'.format(said))
    if problems:
        raise Stopped('\n'.join(problems))
    name = 'Harold ' + (version or '(version not recorded)')
    if revision:
        name += ' ({})'.format(revision[:7])
    return '{} is running, PID {}.'.format(name, status['pid'])


def prefix_of(cwd, prefix):
    """The folder to install under: PREFIX, relative to the checkout, or ~/bin."""
    if not prefix:
        return DEFAULT_PREFIX
    return Path(os.path.abspath(str(Path(cwd) / Path(prefix).expanduser())))


def install_tag(cwd, tag, prefix, identity):
    """Steps 7 and 8 for `tag`."""
    with tempfile.TemporaryDirectory(prefix='harold-release-') as folder:
        print('Downloading {}'.format(tag), flush=True)
        package = download(cwd, tag, Path(folder))
        run_installer(cwd, package, prefix, identity)
    print(report(cwd, prefix, tag), flush=True)
    return 0


def cut(cwd, args):
    """`make release`: steps 1 to 8."""
    tag, latest = checks(cwd, args.version, args.notes)
    sha = git(cwd, 'rev-parse', 'HEAD').strip()
    notes = release_notes(cwd, latest, args.notes)
    url, published = publish(cwd, tag, sha, notes)
    try:
        wait_for_build(cwd, tag, url, published)
    except KeyboardInterrupt:
        raise Interrupted('\n'.join([
            'Interrupted: {} stays a pre-release and its build continues on GitHub: {}'.format(tag, url),
            'When the build succeeds, mark it latest (gh release edit {} --prerelease=false --latest)'.format(tag),
            '  and install it: make install-release VERSION={}'.format(tag[1:]),
            'Or withdraw it: gh release delete {} --cleanup-tag --yes'.format(tag)])) from None
    mark_latest(cwd, tag)
    return install_tag(cwd, tag, prefix_of(cwd, args.prefix), args.signing_identity)


def install_existing(cwd, args):
    """`make install-release`: steps 7 and 8 for the latest release or VERSION, with no checks."""
    if args.version is None:
        try:
            tag = latest_release(cwd)
        except Stopped as error:
            raise Stopped('Not installed: {}'.format(error)) from None
        if tag is None:
            raise Stopped('Not installed: no release is marked latest.')
    else:
        numbers = parse_version(args.version)
        if numbers is None:
            raise Stopped('Not installed: VERSION must be three numbers, such as 0.6.0.')
        tag = tag_of(numbers)
    return install_tag(cwd, tag, prefix_of(cwd, args.prefix), args.signing_identity)


def main(argv=None, cwd=REPO_ROOT):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest='command', metavar='{release,install}')
    commands.required = True
    for name, text in (('release', 'check, publish, build, mark latest and install'),
                       ('install', 'install a release that exists, with no checks')):
        command = commands.add_parser(name, help=text)
        command.add_argument('--version', help='three numbers, such as 0.6.0 (default: '
                             + ('the latest release with its middle number raised)' if name == 'release'
                                else 'the latest release)'))
        if name == 'release':
            command.add_argument('--notes', help='file holding the release notes '
                                 '(default: the subject of every commit since the previous release)')
        command.add_argument('--prefix', help='install under this folder instead of ~/bin')
        command.add_argument('--signing-identity', help='certificate the installer signs with and saves')
    args = parser.parse_args(argv)
    try:
        return cut(cwd, args) if args.command == 'release' else install_existing(cwd, args)
    except Stopped as error:
        if str(error):
            print(error, file=sys.stderr)
        return 1
    except Interrupted as error:
        print(error, file=sys.stderr)
        return 130
    except KeyboardInterrupt:
        print('Interrupted.', file=sys.stderr)
        return 130
    except OSError as error:
        print('Stopped: {}'.format(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
