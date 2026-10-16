#!/usr/bin/env python3
"""Per-user launchd service control, copied into the installed Harold bundle."""

import argparse
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time


def managed_environment(controlled, inherited=None):
    source = os.environ if inherited is None else inherited
    env = {key: value for key, value in source.items()
           if not key.startswith('HAROLD') and not key.startswith('LC_')}
    env.update(controlled)
    return env


def run(args, **kwargs):
    return subprocess.run(args, text=True, capture_output=True, timeout=10, **kwargs)


def target(metadata):
    return 'gui/{}/{}'.format(os.getuid(), metadata['label'])


def launch_pid(metadata):
    result = run(['launchctl', 'print', target(metadata)])
    if result.returncode:
        return None
    match = re.search(r'^\s*pid = (\d+)\s*$', result.stdout, re.MULTILINE)
    return int(match.group(1)) if match else None


def listener_pids(metadata):
    port = int(metadata['grpc_addr'].rsplit(':', 1)[1])
    result = run(['lsof', '-nP', '-a', '-iTCP:{}'.format(port), '-sTCP:LISTEN', '-Fp'])
    if result.returncode not in (0, 1) or result.stderr.strip():
        raise RuntimeError('Cannot inspect listening socket: ' + result.stderr.strip())
    return {int(line[1:]) for line in result.stdout.splitlines() if line.startswith('p')}


def executable_matches(pid, executable):
    result = run(['lsof', '-a', '-p', str(pid), '-d', 'txt', '-Fn'])
    names = [line[1:] for line in result.stdout.splitlines() if line.startswith('n')]
    return bool(names) and names[0] == str(executable)


def pid_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def executable_pids(executable):
    if not executable.is_file():
        return set()
    result = run(['lsof', '-t', str(executable)])
    if result.returncode not in (0, 1) or result.stderr.strip():
        raise RuntimeError('Cannot inspect installed executable: ' + result.stderr.strip())
    return {int(line) for line in result.stdout.splitlines() if line.isdigit()
            and executable_matches(int(line), executable)}


def stop_unmanaged(metadata):
    executable = Path(metadata['bundle']) / 'harold'
    pids = listener_pids(metadata)
    for pid in pids:
        if not executable_matches(pid, executable):
            raise RuntimeError('Port {} belongs to unrelated PID {}; leaving it running'.format(
                metadata['grpc_addr'], pid))
    pids.update(executable_pids(executable))
    for pid in pids:
        if executable_matches(pid, executable):
            os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 10
    while any(pid_alive(pid) for pid in pids):
        if time.monotonic() >= deadline:
            raise RuntimeError('Installed daemon did not stop within 10 seconds')
        time.sleep(0.1)


def unload(metadata):
    """Boots the job out of launchd and waits for its process to end. It stays enabled, so
    launchd loads it again at the next login."""
    pid = launch_pid(metadata)
    loaded = run(['launchctl', 'print', target(metadata)]).returncode == 0
    if loaded:
        result = run(['launchctl', 'bootout', target(metadata)])
        if result.returncode:
            raise RuntimeError('Cannot unload service: ' + result.stderr.strip())
    deadline = time.monotonic() + 10
    while pid and pid_alive(pid):
        if time.monotonic() >= deadline:
            raise RuntimeError('Service did not stop within 10 seconds')
        time.sleep(0.1)


def stop(metadata):
    """Disables the job, so launchd leaves it off across logins until `start`, then unloads it."""
    result = run(['launchctl', 'disable', target(metadata)])
    if result.returncode:
        raise RuntimeError('Cannot disable service: ' + result.stderr.strip())
    unload(metadata)


def crash_file(metadata):
    """Harold's record of starts after unclean exits; its first line is `gave-up` after a crash loop."""
    return Path(metadata['bundle']) / 'data/crashes'


def gave_up(metadata):
    # Read as leniently as Harold does: a file that cannot be read counts as no crash loop.
    try:
        with crash_file(metadata).open(errors='replace') as crashes:
            return crashes.readline().strip() == 'gave-up'
    except OSError:
        return False


def disabled(metadata):
    """Whether `haroldctl stop` disabled the job. macOS prints `=> disabled` or, before
    macOS 14, `=> true`."""
    result = run(['launchctl', 'print-disabled', 'gui/{}'.format(os.getuid())])
    if result.returncode:
        raise RuntimeError('Cannot read disabled services: ' + result.stderr.strip())
    pattern = r'^\s*"{}"\s*=>\s*(disabled|true)\s*$'.format(re.escape(metadata['label']))
    return re.search(pattern, result.stdout, re.MULTILINE) is not None


def autostart(metadata):
    if disabled(metadata):
        return 'off (stopped with haroldctl stop)'
    if gave_up(metadata):
        return 'off (gave up after a crash loop)'
    return 'on'


class Off(RuntimeError):
    """`ensure` left Harold off: it was stopped, or gave up after a crash loop."""


def probe_ready(metadata):
    result = run([str(Path(metadata['bundle']) / 'harold'), '--check-ready'],
                 cwd=metadata['bundle'], env=managed_environment(metadata['env']))
    if result.returncode:
        return False
    try:
        return json.loads(result.stdout).get('ready') is True
    except (ValueError, AttributeError):
        return False


def validated_metadata(metadata):
    bundle = Path(metadata['bundle'])
    result = run([str(bundle / 'harold'), '--check-config'], cwd=bundle,
                 env=managed_environment(metadata['env']))
    if result.returncode:
        raise RuntimeError('Configuration validation failed; check config/local.toml')
    config = json.loads(result.stdout)
    if config['store_path'] != str(bundle / 'data/store'):
        raise RuntimeError('Configuration does not use the managed store path')
    return dict(metadata, grpc_addr=config['grpc_addr'])


def launch(metadata, timeout=30):
    """Loads the job if needed, starts it, and waits until it is ready. It loads the LaunchAgents
    copy, or the bundle's plist when that copy was deleted to turn off start at login."""
    pid = launch_pid(metadata)
    listeners = listener_pids(metadata)
    if listeners and listeners != {pid}:
        raise RuntimeError('Port {} has an unrelated listener; use the installer to migrate an old daemon'.format(
            metadata['grpc_addr']))
    if not pid:
        loaded = run(['launchctl', 'print', target(metadata)]).returncode == 0
        if not loaded:
            plist = metadata['launch_agent'] if Path(metadata['launch_agent']).is_file() else metadata['plist']
            result = run(['launchctl', 'bootstrap', 'gui/{}'.format(os.getuid()), plist])
            if result.returncode:
                raise RuntimeError('Cannot load service: ' + result.stderr.strip())
        result = run(['launchctl', 'kickstart', target(metadata)])
        if result.returncode:
            unload(metadata)
            raise RuntimeError('Cannot start service: ' + result.stderr.strip())
    deadline = time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            pid = launch_pid(metadata)
            if pid and listener_pids(metadata) == {pid} and probe_ready(metadata):
                # Do not attribute a probe to a daemon that exited during the RPC.
                if launch_pid(metadata) == pid and listener_pids(metadata) == {pid}:
                    return pid
            time.sleep(0.2)
        raise RuntimeError('Service failed readiness within {} seconds'.format(timeout))
    except Exception:
        # Unload only: a failed start must not leave the job disabled.
        unload(metadata)
        raise


def start(metadata, timeout=30):
    """Enables the job, forgets any crash loop, and launches it."""
    result = run(['launchctl', 'enable', target(metadata)])
    if result.returncode:
        raise RuntimeError('Cannot enable service: ' + result.stderr.strip())
    crash_file(metadata).unlink(missing_ok=True)
    return launch(metadata, timeout)


def ensure(metadata, timeout=30):
    """Launches the job unless `haroldctl stop` disabled it or Harold gave up after a crash loop."""
    if disabled(metadata):
        raise Off('stopped with haroldctl stop')
    if gave_up(metadata):
        raise Off('gave up after a crash loop; haroldctl start clears it')
    return launch(metadata, timeout)


@contextmanager
def service_lock(prefix, timeout=40):
    path = Path(prefix) / '.harold-install.lock'
    fd = os.open(str(path), os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        deadline = time.monotonic() + timeout
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise RuntimeError('Another Harold install/control operation is still running')
                time.sleep(0.1)
        yield
    finally:
        os.close(fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['start', 'stop', 'restart', 'status', 'ensure'])
    args = parser.parse_args()
    bundle = Path(__file__).resolve().parent
    try:
        metadata = json.loads((bundle / 'service.json').read_text())
        environment = managed_environment(metadata['env'])
        os.environ.clear()
        os.environ.update(environment)
        with service_lock(bundle.parent):
            if args.command in ('start', 'restart', 'ensure'):
                updated = validated_metadata(metadata)
                if updated != metadata:
                    import tempfile
                    with tempfile.NamedTemporaryFile(mode='w', dir=bundle, prefix='.service-', delete=False) as output:
                        pending = Path(output.name)
                        output.write(json.dumps(updated, indent=2) + '\n')
                    try:
                        os.replace(pending, bundle / 'service.json')
                    finally:
                        pending.unlink(missing_ok=True)
                metadata = updated
            if args.command == 'stop':
                stop(metadata)
            if args.command == 'restart':
                # Unload only: if the restart fails here, the job must stay enabled for the next login.
                unload(metadata)
            if args.command in ('start', 'restart'):
                pid = start(metadata)
                print('Harold ready (PID {})'.format(pid))
            if args.command == 'ensure':
                pid = ensure(metadata)
                print('Harold ready (PID {})'.format(pid))
            if args.command == 'status':
                pid = launch_pid(metadata)
                ready = bool(pid and listener_pids(metadata) == {pid} and probe_ready(metadata))
                print(json.dumps({'ready': ready, 'pid': pid, 'grpc_addr': metadata['grpc_addr'],
                                  'autostart': autostart(metadata)}))
                return 0 if ready else 1
        return 0
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print('Harold: {}\nLog: {}\nConfig: {}\nControl: {}'.format(
            error, bundle / 'harold.log', bundle / 'config/local.toml', bundle.parent / 'haroldctl'),
            file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
