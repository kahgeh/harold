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
    if notes and not (Path(cwd) / notes).is_file():
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
    created = gh_json(cwd, 'release', 'view', tag, '--json', 'url,publishedAt')
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
    assets = gh_json(cwd, 'release', 'view', tag, '--json', 'assets')['assets']
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
