#!/usr/bin/env python3
"""Install and configure an on-demand, per-user Harold service on macOS."""

import argparse
from datetime import datetime, timezone
import getpass
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import shlex
import stat
import subprocess
import sys
import tempfile
import uuid

import harold_service as service

CLAUDE_SETTINGS = Path.home() / '.claude/settings.json'
PLUGIN_DIRS = 'CLAUDE_CODE_PLUGIN_DIRS'
# What the mod loads at runtime; a release or checkout missing any of these loads nothing.
CLAUDE_MOD_FILES = tuple('hooks/claude-mod/' + name for name in (
    '.claude-plugin/plugin.json', 'hooks/hooks.json', 'hooks/register.ts', 'hooks/accept.ts'))
SKILL_NAME = 'send-message-to-away-device'
SKILL_FILE = 'skills/{}/SKILL.md'.format(SKILL_NAME)
# What the skill source writes where the installed harold binary's path goes.
BIN_PLACEHOLDER = '{{HAROLD_BIN}}'
# Each agent that reads SKILL.md, and the folder it reads skills from.
AGENT_SKILL_FOLDERS = (('Claude Code', Path.home() / '.claude/skills'),
                       ('Codex', Path.home() / '.codex/skills'))


def reject_symlinks(path):
    for candidate in [path] + list(path.parents):
        if candidate.is_symlink():
            raise RuntimeError('Unsafe symlink in install path: {}'.format(candidate))


def validate_destination(prefix):
    if prefix in (Path('/'), Path.home(), Path('/usr'), Path('/bin'), Path('/usr/bin')):
        raise RuntimeError('Choose a dedicated executable prefix, such as ~/bin')
    reject_symlinks(prefix)
    if prefix.exists() and not prefix.is_dir():
        raise RuntimeError('Install prefix must be a directory')
    for name in ('harold', 'haroldctl', 'tmx-agent-dash', '.harold-install.lock'):
        candidate = prefix / name
        reject_symlinks(candidate)
        if candidate.is_dir():
            for root, dirs, files in os.walk(candidate, followlinks=False):
                for child in dirs + files:
                    if (Path(root) / child).is_symlink():
                        raise RuntimeError('Unsafe symlink in installed bundle: {}'.format(Path(root) / child))
    if (prefix / 'harold').exists() and not (prefix / 'harold').is_dir():
        raise RuntimeError('Installed harold bundle must be a directory')
    for name in ('haroldctl', 'tmx-agent-dash', '.harold-install.lock'):
        path = prefix / name
        if path.exists() and not path.is_file():
            raise RuntimeError('Expected regular installed file: {}'.format(path))
    for name in ('config', 'hooks', 'data', 'data/events'):
        path = prefix / 'harold' / name
        if path.exists() and not path.is_dir():
            raise RuntimeError('Expected managed directory: {}'.format(path))


def prerequisites(repo, prebuilt=False):
    if sys.version_info < (3, 9):
        raise RuntimeError('Python 3.9+ is required')
    if sys.platform != 'darwin':
        raise RuntimeError('This installer requires macOS and a logged-in desktop user')
    if os.getuid() == 0:
        raise RuntimeError('Run as your normal desktop user, without sudo')
    required = ('tmux', 'grpcurl', 'codesign', 'launchctl', 'plutil', 'lsof')
    if not prebuilt:
        required += ('cargo', 'rustc', 'protoc')
    missing = [name for name in required if not shutil.which(name)]
    if missing:
        instructions = ('With Homebrew run `brew install tmux grpcurl`; Apple tools require '
                        '`xcode-select --install`.') if prebuilt else (
                            'Install Rust via rustup; with Homebrew run '
                            '`brew install tmux grpcurl protobuf`; Apple tools require '
                            '`xcode-select --install`.')
        raise RuntimeError('Missing tools: {}. {} Then rerun.'.format(', '.join(missing), instructions))
    if not prebuilt:
        if not (repo / 'events/Cargo.toml').is_file():
            raise RuntimeError('Missing events submodule. Run git submodule update --init --recursive')
        if not (repo / 'Cargo.lock').is_file():
            raise RuntimeError('Missing Cargo.lock; restore the checked-in lockfile before installing')


def prebuilt_files(directory):
    required = ('harold', 'tmx-agent-dash', 'harold.proto',
                'hooks/harold_turn_complete.py', 'hooks/claude_turn_complete.py',
                'hooks/codex_turn_complete.py', 'scripts/harold_service.py',
                'config/default.toml', 'config/local.template.toml') + CLAUDE_MOD_FILES + (SKILL_FILE,)
    for name in required:
        path = directory / name
        if not path.is_file() or path.is_symlink():
            raise RuntimeError('Missing regular release file: {}'.format(path))
    for name in ('harold', 'tmx-agent-dash'):
        if not os.access(directory / name, os.X_OK):
            raise RuntimeError('Release binary is not executable: {}'.format(directory / name))
    return {name: directory / name for name in ('harold', 'tmx-agent-dash')}


def link_claude_mod(settings_path, mod_dir):
    """Name the mod's folder in CLAUDE_CODE_PLUGIN_DIRS in Claude Code's settings.

    Returns 'added', 'appended' or 'unchanged', or the text of the line for the person to add
    when the file is missing or not safe to edit; then nothing is written. A symlinked file is
    edited through the link, and its backup sits beside the real file.
    """
    def manual(reason):
        return '{}. Add to the "env" block of {}: "{}": "{}"'.format(
            reason, settings_path, PLUGIN_DIRS, mod_dir)

    if ':' in str(mod_dir):
        return 'Not added: the mod folder path contains \':\', which a \':\'-separated list cannot hold. ' \
               'Move the install or load it with `claude --plugin-dir {}`'.format(mod_dir)
    target = settings_path.resolve()
    try:
        data = json.loads(target.read_text(encoding='utf-8'))
    except (OSError, ValueError):
        return manual('The settings file is missing or is not valid JSON')
    env = data.get('env', {}) if isinstance(data, dict) else None
    if not isinstance(env, dict) or not isinstance(env.get(PLUGIN_DIRS, ''), str):
        return manual('The settings file has an unexpected shape')
    folders = [folder for folder in env.get(PLUGIN_DIRS, '').split(':') if folder]
    if str(mod_dir) in [os.path.expanduser(folder).rstrip('/') for folder in folders]:
        return 'unchanged'
    env[PLUGIN_DIRS] = ':'.join(folders + [str(mod_dir)])
    data['env'] = env
    backup = target.with_name(target.name + '.before-harold')
    temporary = None
    try:
        text = json.dumps(data, indent=2, ensure_ascii=False, allow_nan=False) + '\n'
        text.encode('utf-8')
        # lexists: an existing symlink there counts as a copy, and is never written through.
        if not os.path.lexists(backup):
            shutil.copy2(target, backup)
        # Owner-only from the first byte: the file can hold tokens in its env block.
        descriptor, name = tempfile.mkstemp(dir=target.parent, prefix=target.name + '.', suffix='.harold-tmp')
        temporary = Path(name)
        with os.fdopen(descriptor, 'w', encoding='utf-8') as handle:
            handle.write(text)
        temporary.chmod(stat.S_IMODE(target.stat().st_mode))
        os.replace(temporary, target)
    except (OSError, ValueError) as error:
        try:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
        except OSError:
            pass
        return manual('The settings file could not be written ({})'.format(getattr(error, 'strerror', None) or error))
    return 'appended' if folders else 'added'


def write_skill(source, destination, binary):
    """Write the skill at `destination`, naming the installed harold binary where the source has
    the placeholder. The path is quoted for the shell, which leaves a path without spaces as it is."""
    text = source.read_text(encoding='utf-8').replace(BIN_PLACEHOLDER, shlex.quote(str(binary)))
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(text, encoding='utf-8')


def shown(path):
    """`path` as printed to the person: with `~` for their home folder."""
    try:
        return '~/' + str(path.relative_to(Path.home()))
    except ValueError:
        return str(path)


def link_skill(skills_folder, target):
    """Link `skills_folder/send-message-to-away-device` to the installed skill folder `target`.

    Returns the outcome to print: 'linked', 'unchanged', or 'not linked' with the reason. Nothing
    that already exists is replaced, and a failure is reported, not raised.
    """
    path = skills_folder / SKILL_NAME
    try:
        if not skills_folder.is_dir():
            return 'not linked ({} not found)'.format(shown(skills_folder))
        if os.path.islink(path) and os.readlink(path) == str(target):
            return 'unchanged'
        # lexists: a link whose target is gone is still something there.
        if os.path.lexists(path):
            return 'not linked, {} already exists. To link it: ln -s {} {}'.format(
                shown(path), shlex.quote(str(target)), shlex.quote(str(path)))
        os.symlink(target, path)
    except OSError as error:
        return 'not linked ({})'.format(error.strerror or error)
    return 'linked'


def choose_config(supplied, bundle):
    source = Path(supplied).expanduser() if supplied else bundle / 'config/local.toml'
    if supplied or source.exists():
        if not source.is_file():
            raise RuntimeError('Config is not a readable TOML file: {}'.format(source))
        return source.read_bytes()
    if not sys.stdin.isatty():
        raise RuntimeError('No existing configuration and no input terminal. Supply --config PATH')
    channel = input('Notification channel [imessage/telegram] (imessage): ').strip().lower() or 'imessage'
    if channel == 'imessage':
        recipient = input('iMessage recipient (phone number or email): ').strip()
        values = input('Messages handle IDs (comma separated, e.g. 1,36): ').strip()
        try:
            handles = [int(value.strip()) for value in values.split(',')]
        except ValueError:
            raise RuntimeError('Handle IDs must be comma-separated integers') from None
        if not recipient or not handles or any(value <= 0 for value in handles):
            raise RuntimeError('A recipient and positive handle IDs are required')
        config = '[imessage]\nrecipient = {}\nhandle_ids = {}\n'.format(
            json.dumps(recipient, ensure_ascii=False), json.dumps(handles))
    elif channel == 'telegram':
        token = getpass.getpass('Telegram bot token (hidden): ').strip()
        try:
            chat_id = int(input('Telegram chat ID: ').strip())
        except ValueError:
            raise RuntimeError('Telegram chat ID must be an integer') from None
        if not token or not chat_id:
            raise RuntimeError('Telegram bot token and nonzero chat ID are required')
        config = '[telegram]\nbot_token = {}\nchat_id = {}\n'.format(
            json.dumps(token, ensure_ascii=False), chat_id)
    else:
        raise RuntimeError('Choose imessage or telegram')
    return (config + '\n[notify]\naway_channel = "{}"\n\n[activity_summary]\nenabled = false\n'.format(channel)).encode()


def checked(args, **kwargs):
    subprocess.run(args, check=True, **kwargs)


def build_binaries(repo, offline):
    names = ('harold', 'tmx-agent-dash')
    command = ['cargo', 'build', '--locked', '--release',
               '--message-format=json-render-diagnostics', '-p', 'harold', '-p', 'tmx-agent-dash']
    if offline:
        command.append('--offline')
    print('Building Harold and dashboard…', flush=True)
    result = subprocess.run(command, cwd=repo, check=True, stdout=subprocess.PIPE, text=True)
    artifacts = {}
    for line in result.stdout.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if message.get('reason') != 'compiler-artifact':
            continue
        target = message['target']
        name = target['name']
        if (name not in names or 'bin' not in target['kind']
                or message['profile']['test'] or not message.get('executable')
                or Path(message['manifest_path']).resolve() != (repo / name / 'Cargo.toml').resolve()):
            continue
        path = Path(message['executable'])
        if not path.is_absolute() or not path.is_file():
            raise RuntimeError('Cargo reported an unavailable executable for {}'.format(name))
        if name in artifacts and artifacts[name] != path:
            raise RuntimeError('Cargo produced multiple target executables for {}; select one native build target'.format(name))
        artifacts[name] = path
    missing = set(names) - artifacts.keys()
    if missing:
        raise RuntimeError('Cargo did not report executable artifacts for {}'.format(', '.join(sorted(missing))))
    return artifacts


def make_metadata(prefix, grpc_addr):
    bundle = prefix / 'harold'
    label = 'com.harold.agent.' + hashlib.sha256(str(prefix).encode()).hexdigest()[:16]
    paths = [str(prefix)]
    for name in ('tmux', 'grpcurl', 'python3', 'claude'):
        location = shutil.which(name)
        if location:
            paths.append(str(Path(location).parent))
    paths.extend(entry for entry in os.environ.get('PATH', '').split(':') if os.path.isabs(entry))
    paths.extend([str(Path.home() / '.local/bin'), '/opt/homebrew/bin', '/usr/local/bin', '/usr/bin', '/bin', '/usr/sbin', '/sbin'])
    return {'bundle': str(bundle), 'label': label, 'grpc_addr': grpc_addr,
            'plist': str(bundle / 'service.plist'),
            'env': {'HAROLD_CONFIG_DIR': str(bundle / 'config'), 'HAROLD_ENV': 'local',
                    'HAROLD__STORE__PATH': str(bundle / 'data/events'),
                    'PATH': ':'.join(dict.fromkeys(paths)), 'LANG': 'en_US.UTF-8', 'LC_ALL': 'en_US.UTF-8'}}


def write_plist(path, metadata):
    bundle = Path(metadata['bundle'])
    value = {'Label': metadata['label'],
             'ProgramArguments': [sys.executable, str(bundle / 'service.py'), 'run'],
             'WorkingDirectory': str(bundle), 'RunAtLoad': False,
             'ProcessType': 'Background', 'ExitTimeOut': 10,
             'EnvironmentVariables': metadata['env'],
             'StandardOutPath': str(bundle / 'harold.log'),
             'StandardErrorPath': str(bundle / 'harold.log')}
    path.write_bytes(plistlib.dumps(value))


def replace_bundle(stage, bundle, reinstall):
    backup = None
    moved = []
    if bundle.exists():
        suffix = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ') + '-' + uuid.uuid4().hex[:8]
        backup = bundle.with_name('harold.backup-' + suffix)
        bundle.rename(backup)
    try:
        if backup and not reinstall:
            for name in ('data', 'harold.log'):
                source = backup / name
                if source.exists():
                    destination = stage / name
                    if destination.is_dir():
                        shutil.rmtree(destination)
                    source.rename(destination)
                    moved.append(name)
        stage.rename(bundle)
    except OSError:
        if backup:
            for name in reversed(moved):
                (stage / name).rename(backup / name)
            backup.rename(bundle)
        raise
    return backup


def install(args, repo):
    prebuilt = Path(args.prebuilt_dir).expanduser().resolve() if args.prebuilt_dir else None
    prerequisites(repo, prebuilt=prebuilt is not None)
    artifacts = prebuilt_files(prebuilt) if prebuilt is not None else None
    assets = prebuilt if prebuilt is not None else repo
    raw_prefix = Path(os.path.abspath(os.path.expanduser(args.prefix)))
    validate_destination(raw_prefix)
    prefix = raw_prefix
    bundle = prefix / 'harold'
    local = choose_config(args.config, bundle)
    if artifacts is None:
        artifacts = build_binaries(repo, args.offline)
    prefix.mkdir(parents=True, exist_ok=True)
    with service.service_lock(prefix):
        validate_destination(prefix)
        with tempfile.TemporaryDirectory(prefix='.harold-stage-', dir=prefix) as temporary:
            staging = Path(temporary)
            stage = staging / 'harold'
            (stage / 'config').mkdir(parents=True)
            (stage / 'hooks').mkdir()
            (stage / 'data/events').mkdir(parents=True)
            shutil.copy2(artifacts['harold'], stage / 'harold')
            shutil.copy2(artifacts['tmx-agent-dash'], staging / 'tmx-agent-dash')
            shutil.copy2(assets / ('harold.proto' if prebuilt is not None else 'harold-api/proto/harold.proto'),
                         stage / 'harold.proto')
            shutil.copy2(assets / ('hooks/harold_turn_complete.py' if prebuilt is not None else 'hooks/shared/harold_turn_complete.py'),
                         stage / 'hooks/harold_turn_complete.py')
            for name in ('claude_turn_complete.py', 'codex_turn_complete.py'):
                source = assets / ('hooks' if prebuilt is not None else 'hooks/providers') / name
                shutil.copy2(source, stage / 'hooks' / name)
            shutil.copytree(assets / 'hooks/claude-mod', stage / 'hooks/claude-mod',
                            ignore=shutil.ignore_patterns('*.test.ts'))
            shutil.copy2(assets / 'scripts/harold_service.py', stage / 'service.py')
            for name in ('default.toml', 'local.template.toml'):
                shutil.copy2(assets / ('config' if prebuilt is not None else 'harold/config') / name, stage / 'config' / name)
            write_skill(assets / SKILL_FILE, stage / SKILL_FILE, bundle / 'harold')
            (stage / 'config/local.toml').write_bytes(local)
            (stage / 'config/local.toml').chmod(0o600)
            metadata = make_metadata(prefix, '')
            reject_symlinks(Path(metadata['plist']))
            for binary in (stage / 'harold', staging / 'tmx-agent-dash'):
                checked(['codesign', '--force', '--sign', args.signing_identity, str(binary)])
                checked(['codesign', '--verify', '--strict', str(binary)])
            env = service.managed_environment(metadata['env'])
            env['HAROLD_CONFIG_DIR'] = str(stage / 'config')
            probe = subprocess.run([str(stage / 'harold'), '--check-config'], cwd=stage, env=env,
                                   capture_output=True, text=True, timeout=10)
            if probe.returncode:
                # Typed config errors can contain credential values; never relay them.
                raise RuntimeError('Configuration validation failed; check the supplied TOML and required channel fields')
            config = json.loads(probe.stdout)
            metadata['grpc_addr'] = config['grpc_addr']
            if config['store_path'] != str(bundle / 'data/events'):
                raise RuntimeError('Configuration probe did not use the managed store path')
            (stage / 'service.json').write_text(json.dumps(metadata, indent=2) + '\n')
            write_plist(stage / 'service.plist', metadata)
            checked(['plutil', '-lint', str(stage / 'service.plist')])
            launcher = '#!/bin/sh\nexec {} "$(dirname "$0")/harold/service.py" "$@"\n'.format(shlex.quote(sys.executable))
            (staging / 'haroldctl').write_text(launcher)
            (staging / 'haroldctl').chmod(0o755)
            old_metadata = metadata
            if (bundle / 'service.json').is_file():
                old_metadata = json.loads((bundle / 'service.json').read_text())
                if old_metadata['bundle'] != str(bundle) or old_metadata['label'] != metadata['label']:
                    raise RuntimeError('Existing service metadata does not belong to this prefix')
            # Attribute listeners before stopping any currently running service.
            for current in (old_metadata, metadata):
                for pid in service.listener_pids(current):
                    if not service.executable_matches(pid, bundle / 'harold'):
                        raise RuntimeError('Port {} belongs to unrelated PID {}; leaving it running'.format(current['grpc_addr'], pid))
            service.stop(old_metadata)
            service.stop_unmanaged(old_metadata)
            backup = replace_bundle(stage, bundle, args.reinstall)
            if backup:
                print('Previous installation archived at {}'.format(backup), flush=True)
            os.replace(staging / 'tmx-agent-dash', prefix / 'tmx-agent-dash')
            os.replace(staging / 'haroldctl', prefix / 'haroldctl')
            pid = service.start(metadata)
            print('Harold ready at {} (LaunchAgent PID {}).\nControl: {}\nConfig: {}\nLog: {}'.format(
                metadata['grpc_addr'], pid, prefix / 'haroldctl', bundle / 'config/local.toml', bundle / 'harold.log'))
            outcome = link_claude_mod(CLAUDE_SETTINGS, bundle / 'hooks/claude-mod')
            print('Claude Code mod: {}'.format(outcome), flush=True)
            for agent, skills_folder in AGENT_SKILL_FOLDERS:
                print('{} skill: {}'.format(agent, link_skill(skills_folder, bundle / 'skills' / SKILL_NAME)),
                      flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reinstall', action='store_true', help='archive the old bundle and start with fresh managed data')
    parser.add_argument('--config', help='import a TOML local configuration')
    parser.add_argument('--prefix', default=str(Path.home() / 'bin'), help='executable directory (default: ~/bin)')
    parser.add_argument('--signing-identity', default='-', help='codesign identity (default: ad-hoc signing)')
    parser.add_argument('--offline', action='store_true', help='build using cached Cargo dependencies only')
    parser.add_argument('--prebuilt-dir', help='install binaries and assets from an extracted release instead of building')
    args = parser.parse_args()
    if args.prebuilt_dir and args.offline:
        parser.error('--offline only applies to source builds')
    try:
        install(args, Path(__file__).resolve().parents[1])
        return 0
    except (OSError, RuntimeError, ValueError, KeyError, subprocess.SubprocessError) as error:
        prefix = Path(args.prefix).expanduser().absolute()
        print('Install failed: {}\nConfig: {}\nLog: {}\nControl: {}'.format(
            error, prefix / 'harold/config/local.toml', prefix / 'harold/harold.log', prefix / 'haroldctl'), file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
