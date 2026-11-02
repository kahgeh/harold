#!/usr/bin/env python3
"""Install and configure an on-demand, per-user Harold service on macOS (launchd) or Linux (systemd)."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
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
# The sentences `harold --check-config` writes to stderr for a setting Harold no longer reads, or
# for a setting that needs a feature the build lacks. Keep them identical to the constants in
# harold/src/settings.rs (TELEGRAM_REMOVED, PEERS_REMOVED, NOTIFY_TELEGRAM_REMOVED,
# AWAY_CHANNEL_REMOVED, GRPC_HOST_NOT_LOOPBACK) and to `needs_feature_sentence` there.
REFUSAL_SENTENCES = (
    '[telegram] is no longer read: Harold reaches the phone by iMessage only. Remove it from config/local.toml or the HAROLD__ environment variables.',
    '[peers] is no longer read: two Macs share replies through iMessage, with no hub or member. Remove it from config/local.toml or the HAROLD__ environment variables.',
    'notify.telegram is no longer read: the lists are imessage, web_app and machine_audio_output. Remove it from config/local.toml or the HAROLD__ environment variables.',
    'notify.away_channel is no longer read: Harold reaches the phone by iMessage only. Remove it from config/local.toml or the HAROLD__ environment variables.',
    'grpc.host must be a loopback address (127.0.0.0/8, ::1 or localhost): Harold no longer serves other Macs.',
    'notify.imessage is set, but this Harold is built without iMessage: remove it from config/local.toml or the HAROLD__ environment variables.',
    'imessage is set, but this Harold is built without iMessage: remove it from config/local.toml or the HAROLD__ environment variables.',
    'chat_db is set, but this Harold is built without iMessage: remove it from config/local.toml or the HAROLD__ environment variables.',
    'voice_input is set, but this Harold is built without calls: remove it from config/local.toml or the HAROLD__ environment variables.',
)
SKILL_NAME = 'send-message-to-away-device'
SKILL_FILE = 'skills/{}/SKILL.md'.format(SKILL_NAME)
# The program that moved into the bundle as control-tower.
OLD_PROGRAM = 'tmx-agent-dash'
# What the skill source writes where the installed harold binary's path goes.
BIN_PLACEHOLDER = '{{HAROLD_BIN}}'
# Each agent that reads SKILL.md, and the folder it reads skills from.
AGENT_SKILL_FOLDERS = (('Claude Code', Path.home() / '.claude/skills'),
                       ('Codex', Path.home() / '.codex/skills'))
# Printed after an iMessage install. macOS keys Full Disk Access and Messages control to the
# designated requirement of harold's signature, so a matching requirement keeps both.
CARRY_OVER = ('macOS permissions: this harold is signed like the one it replaced; '
              'Full Disk Access and Messages control carry over.')
GRANT_STEPS = '\n'.join([
    'macOS permissions for this build of harold:',
    '  Full Disk Access (needed to read replies from the phone):',
    '    System Settings > Privacy & Security > Full Disk Access.',
    '    If "harold" is in the list, select it and click "−".',
    '    Click "+", press Cmd+Shift+G, enter {binary}, press Enter, click Open.',
    '    Check that the switch next to "harold" is on. No restart is needed.',
    '  Messages: the first time Harold sends a message, macOS asks whether "harold" may control',
    '    "Messages". Click Allow. Until then Harold cannot send.',
    '  If you switched on python3.13 for Harold before, you can switch it off now.'])
AD_HOC_NOTE = ('Signed with no certificate: these steps are needed after every install of a new build. '
               'See "Keep permissions across installs" in docs/how-tos/setup.md.')


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
    # An old install can still hold tmx-agent-dash, so it stays in both lists.
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
    # An old install can still hold tmx-agent-dash.
    for name in ('haroldctl', 'tmx-agent-dash', '.harold-install.lock'):
        path = prefix / name
        if path.exists() and not path.is_file():
            raise RuntimeError('Expected regular installed file: {}'.format(path))
    for name in ('config', 'hooks', 'data', 'data/events', 'data/store'):
        path = prefix / 'harold' / name
        if path.exists() and not path.is_dir():
            raise RuntimeError('Expected managed directory: {}'.format(path))


PACMAN_LINE = 'sudo pacman -S --needed python curl lsof inetutils base-devel pipewire'
GRPCURL_LINE = ('Install grpcurl from the AUR package grpcurl-bin at its checked commit, '
                'with makepkg -si (see docs/prerequisites.md).')


def linux_prerequisites(prebuilt):
    """The Linux twin of the Mac checks in `prerequisites`: no codesign, launchctl or plutil."""
    if sys.version_info < (3, 9):
        raise RuntimeError('Python 3.9+ is required')
    if os.getuid() == 0:
        raise RuntimeError('Run as your normal desktop user, without sudo')
    required = ('rex', 'grpcurl', 'lsof', 'systemctl', 'curl')
    if not prebuilt:
        required += ('cargo', 'rustc', 'protoc')
    missing = [name for name in required if not shutil.which(name)]
    if missing:
        lines = ['Missing tools: {}.'.format(', '.join(missing)), 'Run `{}`.'.format(PACMAN_LINE), GRPCURL_LINE]
        if 'rex' in missing:
            lines.append('Rex comes with omarchy: check that `rex` is on your PATH.')
        if not prebuilt:
            lines.append('For a source build, install Rust via rustup and run `sudo pacman -S --needed protobuf`.')
        raise RuntimeError(' '.join(lines) + ' Then rerun.')


def prerequisites(repo, prebuilt=False):
    if sys.version_info < (3, 9):
        raise RuntimeError('Python 3.9+ is required')
    if sys.platform == 'linux':
        linux_prerequisites(prebuilt)
        if not prebuilt and not (repo / 'Cargo.lock').is_file():
            raise RuntimeError('Missing Cargo.lock; restore the checked-in lockfile before installing')
        return
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
        if not (repo / 'Cargo.lock').is_file():
            raise RuntimeError('Missing Cargo.lock; restore the checked-in lockfile before installing')


def prebuilt_files(directory):
    required = ('harold', 'control-tower', 'harold.proto',
                'hooks/harold_turn_complete.py', 'hooks/claude_turn_complete.py',
                'hooks/codex_turn_complete.py', 'scripts/harold_service.py',
                'config/default.toml', 'config/local.template.toml') + CLAUDE_MOD_FILES + (SKILL_FILE,)
    for name in required:
        path = directory / name
        if not path.is_file() or path.is_symlink():
            raise RuntimeError('Missing regular release file: {}'.format(path))
    for name in ('harold', 'control-tower'):
        if not os.access(directory / name, os.X_OK):
            raise RuntimeError('Release binary is not executable: {}'.format(directory / name))
    return {name: directory / name for name in ('harold', 'control-tower')}


def write_settings(target, data):
    """Write `data` as the settings file `target` (already resolved), keeping the first version
    beside it as `<name>.before-harold`. The write is atomic and keeps the file's mode. Raises
    OSError or ValueError, with the file as it was."""
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
    except (OSError, ValueError):
        try:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
        except OSError:
            pass
        raise


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
    try:
        write_settings(target, data)
    except (OSError, ValueError) as error:
        return manual('The settings file could not be written ({})'.format(getattr(error, 'strerror', None) or error))
    return 'appended' if folders else 'added'


TURN_HOOK = 'claude_turn_complete.py'
# A script that imports Harold's shared notifier is a Harold hook, whatever it is called.
NOTIFIER_IMPORT = re.compile(r'^\s*(?:import|from)\s+harold_turn_complete\b', re.MULTILINE)


def turn_hook_command(hook_path):
    return 'python3 {}'.format(shlex.quote(str(hook_path)))


def stop_hook_script(command):
    """The script a Stop hook command runs: its last argument that ends in .py, else None."""
    try:
        words = shlex.split(command)
    except ValueError:
        return None
    scripts = [word for word in words if word.endswith('.py')]
    return Path(os.path.expanduser(scripts[-1])) if scripts else None


def is_harold_stop_hook(script):
    """Harold's own claude_turn_complete.py, or a script that imports harold_turn_complete."""
    if script.name == TURN_HOOK:
        return True
    try:
        with open(script, encoding='utf-8', errors='replace') as handle:
            return bool(NOTIFIER_IMPORT.search(handle.read(1_000_000)))
    except OSError:
        return False


def register_turn_hook(settings_path, hook_path):
    """Make Harold's claude_turn_complete.py the Claude Code Stop hook in the settings.

    Returns 'unchanged' (a Stop command already runs `hook_path`), 'replaced' (the one Harold
    Stop hook that ran another script now runs it) or 'added' (there was none), or the text
    of the JSON for the person to add when the file is missing, not valid JSON, has a Stop
    list this does not know the shape of, or has two Harold hooks; then nothing is written.
    No other tool's hook is touched. A symlinked file is edited through the link, and its
    backup sits beside the real file.
    """
    command = turn_hook_command(hook_path)

    def manual(reason):
        entry = {'matcher': '', 'hooks': [{'type': 'command', 'command': command}]}
        return '{}. Add this entry to "hooks": "Stop" in {}, in place of any Harold Stop hook: {}'.format(
            reason, settings_path, json.dumps(entry))

    target = settings_path.resolve()
    try:
        data = json.loads(target.read_text(encoding='utf-8'))
    except (OSError, ValueError):
        return manual('The settings file is missing or is not valid JSON')
    hooks = data.get('hooks', {}) if isinstance(data, dict) else None
    stop = hooks.get('Stop', []) if isinstance(hooks, dict) else None
    if not isinstance(stop, list) or not all(
            isinstance(entry, dict) and isinstance(entry.get('hooks', []), list)
            and all(isinstance(hook, dict) for hook in entry.get('hooks', []))
            for entry in stop):
        return manual('The settings file has a Stop hook list of a shape this does not know')
    harold = []
    for entry in stop:
        for hook in entry.get('hooks', []):
            script = stop_hook_script(hook['command']) \
                if hook.get('type') == 'command' and isinstance(hook.get('command'), str) else None
            if script is not None and is_harold_stop_hook(script):
                harold.append((hook, script))
    if len(harold) > 1:
        return manual('The settings file has {} Harold Stop hooks'.format(len(harold)))
    if harold and harold[0][1] == Path(hook_path):
        return 'unchanged'
    if harold:
        harold[0][0]['command'] = command
        outcome = 'replaced'
    else:
        hooks = data.setdefault('hooks', {})
        hooks.setdefault('Stop', []).append({'matcher': '', 'hooks': [{'type': 'command', 'command': command}]})
        outcome = 'added'
    try:
        write_settings(target, data)
    except (OSError, ValueError) as error:
        return manual('The settings file could not be written ({})'.format(getattr(error, 'strerror', None) or error))
    return outcome


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
    if sys.platform == 'linux':
        # No iMessage on Linux, so there is no question to ask: linux_config adds the settings.
        return b''
    if not sys.stdin.isatty():
        raise RuntimeError('No existing configuration and no input terminal. Supply --config PATH')
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
    return (config + '\n[activity_summary]\nenabled = false\n').encode()


TOML_HEADER = re.compile(r'^[ \t]*\[(?!\[)[ \t]*([A-Za-z0-9_-]+)[ \t]*[.\]]', re.MULTILINE)
TOML_TOP_KEY = re.compile(r'^[ \t]*([A-Za-z0-9_-]+)[ \t]*[.=]', re.MULTILINE)


def has_table(text, name):
    """Whether the TOML `text` already defines the table `name`: a `[name]` or `[name.sub]`
    header, or a top-level `name = ...` or `name.key = ...` before the first header. A line check
    only (Python 3.9 has no tomllib); a comment line never matches."""
    if name in TOML_HEADER.findall(text):
        return True
    first = TOML_HEADER.search(text)
    return name in TOML_TOP_KEY.findall(text[:first.start()] if first else text)


def linux_config(base, rex):
    """`base` with the Linux settings appended: [agent_monitor] for Rex, [tts] with no command and
    [telemetry] with no export. A table already in `base` is left as written, with one line said."""
    try:
        text = base.decode('utf-8')
    except UnicodeDecodeError:
        raise RuntimeError('The configuration is not UTF-8 text') from None
    tables = (('agent_monitor', 'multiplexer = "rex"\nrex = {}\n'.format(json.dumps(rex, ensure_ascii=False))),
              ('tts', 'command = ""\n'),
              ('telemetry', 'export = false\n'))
    added = []
    for name, body in tables:
        if has_table(text, name):
            print('[{}] is already in the configuration; left as written.'.format(name), flush=True)
        else:
            added.append('[{}]\n{}'.format(name, body))
    if added:
        if text and not text.endswith('\n'):
            text += '\n'
        text += ('\n' if text else '') + '\n'.join(added)
    return text.encode('utf-8')


def package_features(binary):
    """The features the package's harold was built with, one per line of `--build-features`."""
    result = subprocess.run([str(binary), '--build-features'], capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError('{} --build-features failed: {}'.format(binary, result.stderr.strip()))
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


def find_rex():
    """The path of `rex` on PATH, as found: a symlink stays a symlink, so a Rex upgrade keeps it."""
    location = shutil.which('rex')
    if not location:
        raise RuntimeError("Rex not found on PATH: omarchy's agents run in Rex. Check that `rex` is installed.")
    return os.path.abspath(location)


def check_rex_server(rex):
    result = subprocess.run([rex, '--autostart=false', 'whoami'], capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise RuntimeError("Install stopped: no Rex server answers ({} --autostart=false whoami: {}). "
                           "omarchy's agents run in Rex, and Harold reads them there. Start Rex, then rerun. "
                           'Nothing was installed.'.format(rex, result.stderr.strip() or 'exit {}'.format(result.returncode)))


def check_service_manager():
    """Stops before any change when the user's systemd manager does not answer (for example, no
    DBUS_SESSION_BUS_ADDRESS or XDG_RUNTIME_DIR). service.run stops a call after 10 seconds."""
    result = service.systemctl('daemon-reload')
    if result.returncode:
        raise RuntimeError('Cannot reach the user service manager: systemctl --user daemon-reload: {}. '
                           'Run the install from a login session of your user.'.format(result.stderr.strip()))


def carried_pairing(bundle):
    """The installed config/pairing.env's bytes, which hold the web app's pairing code, or None
    when there is none. Like local.toml it is carried across installs; a link or a folder is
    not carried."""
    source = bundle / 'config/pairing.env'
    if source.is_symlink() or not source.is_file():
        return None
    return source.read_bytes()


def checked(args, **kwargs):
    subprocess.run(args, check=True, **kwargs)


def record_version(stage, prebuilt, repo):
    """Say in the bundle what was installed: a release's VERSION and REVISION files, copied, or
    for a source install `source` and the checkout's commit. No commit is recorded without git."""
    if prebuilt is not None:
        for name in ('VERSION', 'REVISION'):
            source = prebuilt / name
            if source.is_file() and not source.is_symlink():
                shutil.copy2(source, stage / name)
        return
    (stage / 'VERSION').write_text('source\n')
    try:
        result = subprocess.run(['git', 'rev-parse', 'HEAD'], cwd=repo, capture_output=True, text=True)
    except OSError:
        return
    revision = result.stdout.strip()
    if result.returncode == 0 and revision:
        (stage / 'REVISION').write_text(revision + '\n')


def saved_signing_identity(bundle):
    """The identity the installed bundle was signed with: `-` (ad-hoc) when none is recorded."""
    path = bundle / 'service.json'
    if not path.is_file():
        return '-'
    identity = json.loads(path.read_text()).get('signing_identity', '-')
    if not isinstance(identity, str):
        raise RuntimeError('service.json signing_identity must be text; pass --signing-identity')
    return identity


def sign(binary, identity):
    """Sign `binary` with `identity` and an identifier that is its file name.

    A fixed identifier keeps a certificate-signed program's designated requirement the same
    across builds, and with it the privacy grants macOS keys to that requirement.
    No timeout: codesign may wait for the person to answer the keychain question.
    """
    result = subprocess.run(['codesign', '--force', '--sign', identity, '--identifier', binary.name,
                             str(binary)], capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError('codesign could not sign {} with "{}": {}'.format(
            binary.name, identity, result.stderr.strip()))
    checked(['codesign', '--verify', '--strict', str(binary)])


def designated_requirement(binary):
    """The rule macOS stores with a privacy grant for `binary`, or None when there is none to read."""
    if not binary.is_file():
        return None
    result = subprocess.run(['codesign', '-dr', '-', str(binary)], capture_output=True, text=True)
    if result.returncode:
        return None
    for line in result.stdout.splitlines():
        text = line.lstrip('# ')
        if text.startswith('designated => '):
            return text[len('designated => '):].strip() or None
    return None


def permissions_message(old, new, identity, binary):
    """What to tell the person about macOS permissions after an install."""
    if old is not None and old == new:
        return CARRY_OVER
    steps = GRANT_STEPS.format(binary=binary)
    return steps + '\n' + AD_HOC_NOTE if identity == '-' else steps


def build_binaries(repo, offline):
    names = ('harold', 'control-tower')
    command = ['cargo', 'build', '--locked', '--release',
               '--message-format=json-render-diagnostics', '-p', 'harold', '-p', 'control-tower']
    if offline:
        command.append('--offline')
    print('Building Harold and the control-tower…', flush=True)
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
            'launch_agent': str(Path.home() / 'Library/LaunchAgents' / (label + '.plist')),
            'env': {'HAROLD_CONFIG_DIR': str(bundle / 'config'), 'HAROLD_ENV': 'local',
                    'HAROLD__STORE__PATH': str(bundle / 'data/store'),
                    'PATH': ':'.join(dict.fromkeys(paths)), 'LANG': 'en_US.UTF-8', 'LC_ALL': 'en_US.UTF-8'}}


def write_plist(path, metadata):
    bundle = Path(metadata['bundle'])
    value = {'Label': metadata['label'],
             'ProgramArguments': [str(bundle / 'harold')],
             'WorkingDirectory': str(bundle),
             # Start at login; after an unclean exit, start again at most every 30 seconds.
             'RunAtLoad': True, 'KeepAlive': {'SuccessfulExit': False}, 'ThrottleInterval': 30,
             'ExitTimeOut': 10,
             # Only the job launchd starts carries HAROLD_LAUNCHD, which turns a refused
             # start into a clean exit; haroldctl's checks run with metadata['env'].
             'EnvironmentVariables': dict(metadata['env'], HAROLD_LAUNCHD='1'),
             'StandardOutPath': str(bundle / 'harold.log'),
             'StandardErrorPath': str(bundle / 'harold.log')}
    path.write_bytes(plistlib.dumps(value))


def write_launch_agent(metadata):
    """Copies the bundle's service.plist to ~/Library/LaunchAgents, where launchd loads it at
    login. The copy is written beside it and moved into place, so a symlink there is replaced,
    not written through. The file written beside it does not end in `.plist`, so launchd never
    loads it, and it is removed if anything fails."""
    destination = Path(metadata['launch_agent'])
    destination.parent.mkdir(parents=True, exist_ok=True)
    output = tempfile.NamedTemporaryFile(dir=destination.parent, prefix='.harold-', suffix='.plist.tmp',
                                         delete=False)
    pending = Path(output.name)
    try:
        with output:
            output.write(Path(metadata['plist']).read_bytes())
        pending.chmod(0o644)
        os.replace(pending, destination)
    finally:
        pending.unlink(missing_ok=True)


def make_linux_metadata(prefix, grpc_addr, rex):
    """The Linux twin of make_metadata: a systemd user unit, and no launchd label."""
    bundle = prefix / 'harold'
    rex_folder = str(Path(rex).parent)
    if ':' in rex_folder or '\n' in rex_folder:
        raise RuntimeError("The rex folder {!r} cannot go in PATH (it holds ':' or a line break). "
                           'Move rex, then rerun.'.format(rex_folder))
    if '\n' in str(prefix):
        raise RuntimeError('The install prefix cannot hold a line break')
    paths = [rex_folder, str(Path.home() / '.local/bin'), '/usr/local/bin', '/usr/bin']
    return {'bundle': str(bundle), 'unit': service.UNIT, 'grpc_addr': grpc_addr,
            'unit_file': str(Path.home() / '.config/systemd/user' / service.UNIT),
            'env': {'HAROLD_CONFIG_DIR': str(bundle / 'config'), 'HAROLD_ENV': 'local',
                    'HAROLD__STORE__PATH': str(bundle / 'data/store'),
                    'PATH': ':'.join(dict.fromkeys(paths)), 'LANG': 'C.UTF-8', 'LC_ALL': 'C.UTF-8'}}


def unit_path(value):
    """`value` for a systemd setting that takes a path as is: `%` doubled, no quotes."""
    return value.replace('%', '%%')


def unit_quoted(value):
    """`value` as one quoted word in a systemd unit, with `\\`, `"` and `%` escaped."""
    return '"{}"'.format(value.replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%'))


def unit_environment(name, value):
    assignment = '{}={}'.format(name, value)
    if re.fullmatch(r'[A-Za-z0-9_@+=:,./-]*', assignment):
        return 'Environment=' + assignment
    return 'Environment=' + unit_quoted(assignment)


def write_unit(path, metadata):
    """Writes the systemd user unit at `path`. It is written beside it and moved into place, so a
    symlink there is replaced, not written through. The file written beside it does not end in
    `.service`, so systemd never loads it, and it is removed if anything fails."""
    bundle = Path(metadata['bundle'])
    log = unit_path(str(bundle / 'harold.log'))
    # Only the process systemd starts carries HAROLD_LAUNCHD, as with launchd on the Mac.
    environment = dict(metadata['env'], HAROLD_LAUNCHD='1')
    lines = ['[Unit]', 'Description=Harold',
             # After 5 unclean exits in 10 minutes, systemd stops starting it again.
             'StartLimitIntervalSec=600', 'StartLimitBurst=5', '',
             '[Service]', 'Type=simple',
             # `$` starts a variable in ExecStart only.
             'ExecStart=' + unit_quoted(str(bundle / 'harold')).replace('$', '$$'),
             'WorkingDirectory=' + unit_path(str(bundle))]
    lines += [unit_environment(name, value) for name, value in environment.items()]
    lines += ['Restart=on-failure', 'RestartSec=30', 'TimeoutStopSec=10',
              'StandardOutput=append:' + log, 'StandardError=append:' + log, '',
              '[Install]', 'WantedBy=default.target', '']
    path.parent.mkdir(parents=True, exist_ok=True)
    output = tempfile.NamedTemporaryFile(mode='w', dir=path.parent, prefix='.harold-', suffix='.unit.tmp',
                                         delete=False)
    pending = Path(output.name)
    try:
        with output:
            output.write('\n'.join(lines))
        pending.chmod(0o644)
        os.replace(pending, path)
    finally:
        pending.unlink(missing_ok=True)


def linger_note():
    """One warning line when systemd stops the user's services at logout, else None. It never
    runs `loginctl enable-linger`: that choice is the person's."""
    try:
        result = service.run(['loginctl', 'show-user', str(os.getuid()), '-p', 'Linger', '--value'])
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode == 0 and result.stdout.strip() == 'no':
        return ('Warning: lingering is off for your user, so systemd stops Harold when you log out. '
                'To keep it running after logout, run: loginctl enable-linger')
    return None


def recorded(bundle, name):
    """The text of a bundle's VERSION or REVISION file, or None when it has none."""
    try:
        return (bundle / name).read_text().strip() or None
    except (OSError, ValueError):
        return None


def plist_python(bundle):
    """The Python a bundle's service.plist starts Harold through (bundles from before
    direct-launch run `python service.py run`), or None."""
    try:
        arguments = plistlib.loads((bundle / 'service.plist').read_bytes()).get('ProgramArguments')
    except Exception:
        # A damaged plist can raise almost anything (ExpatError among them); any of it means no Python.
        return None
    if (isinstance(arguments, list) and len(arguments) >= 2
            and all(isinstance(item, str) for item in arguments[:2])
            and Path(arguments[1]).name == 'service.py'):
        return arguments[0]
    return None


def starts_at_login(bundle):
    """Whether a bundle's service.plist has launchd start it at login."""
    try:
        return plistlib.loads((bundle / 'service.plist').read_bytes()).get('RunAtLoad') is True
    except Exception:
        # A missing or damaged plist can raise almost anything; any of it means no.
        return False


def rollback_note(prefix, backup, copied, reinstall, metadata, data_copied=False):
    """ROLLBACK.md for `backup`: what it holds and the commands that put it back.

    `copied` names the files from `prefix` kept in `backup`. The commands set the new bundle
    aside as harold.replaced-<time> and, unless this was a reinstall, move its data and log back.
    They put back the LaunchAgents copy of `backup`'s plist, or remove the copy when `backup`
    does not start at login, and enable the label, which the new `haroldctl stop` disabled.
    On Linux they enable the systemd unit instead.
    They also delete the counted-tip file, which an older Harold would leave stale.
    When `data_copied` is true, `backup` holds a copy of the store and events, so the commands move back
    only the other entries of the new data folder.
    """
    aside = 'harold.replaced-' + backup.name[len('harold.backup-'):]
    commands = ['cd ' + shlex.quote(str(prefix)), './haroldctl stop',
                'mv harold ' + aside, 'mv {} harold'.format(backup.name)]
    if reinstall:
        holds = ('It holds the programs, configuration, scripts and data from before the reinstall. '
                 'The new data stays in {}.'.format(aside))
    elif data_copied:
        holds = ('It holds the programs, configuration and scripts. It also holds a copy of the data folder, '
                 'taken before the install. The log and the other data entries moved to the new install, and the '
                 'commands move them back. The new store and events stay in {}/data. '
                 'Replies and events from after the install are lost.'.format(aside))
        commands.append('mv {}/harold.log harold/harold.log'.format(aside))
        # The copy holds only the store and events. Every other entry of the live data comes back from the aside folder.
        live = prefix / 'harold' / 'data'
        others = sorted(entry.name for entry in live.iterdir() if entry.name not in ('store', 'events')) if live.is_dir() else []
        commands += ['mv {}/data/{} harold/data/{}'.format(aside, shlex.quote(name), shlex.quote(name)) for name in others]
    else:
        holds = ('It holds the programs, configuration and scripts. The data and log moved to the new '
                 'install, and the commands move them back.')
        commands += ['mv {}/data harold/data'.format(aside), 'mv {}/harold.log harold/harold.log'.format(aside)]
    if copied:
        commands.append('mv {} .'.format(' '.join('harold/' + name for name in copied)))
    # An older Harold counts merges without moving the tips file, so a later upgrade would count them again.
    commands.append('rm -f harold/data/pod/tips')
    if sys.platform == 'linux':
        # The unit names the same bundle path, so it needs no change; the new stop disabled it.
        commands += ['systemctl --user enable harold', './haroldctl start']
    else:
        launch_agent = shlex.quote(metadata['launch_agent'])
        if starts_at_login(backup):
            commands.append('cp harold/service.plist ' + launch_agent)
        else:
            commands.append('rm -f ' + launch_agent)
        commands += ['launchctl enable gui/$(id -u)/' + metadata['label'], './haroldctl start']
    lines = ['# Roll back Harold', '',
             'This folder holds the Harold that was installed in {}.'.format(prefix), '',
             '- Version: {}'.format(recorded(backup, 'VERSION') or 'not recorded'),
             '- Revision: {}'.format(recorded(backup, 'REVISION') or 'not recorded'),
             '- Replaced: {}'.format(datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M UTC')), '',
             holds, '',
             'To roll back, run these commands. They stop the new Harold, set it aside as {}, '
             'put this one back and start it.'.format(aside), '',
             '```sh'] + commands + ['```', '',
             'The commands delete `harold/data/pod/tips`, the counted-tip file. A Harold built before 36a7c94 '
             'counts merges without moving it, so a later upgrade would count them again. Deleting it is safe.']
    python = plist_python(backup)
    if python:
        lines += ['', 'This Harold starts through Python. After rolling back, Full Disk Access and Messages '
                  'control must be on for {} (System Settings > Privacy & Security).'.format(python)]
    return '\n'.join(lines) + '\n'


def keep_rollback_copy(prefix, backup, reinstall, metadata, data_copied=False):
    """Copy haroldctl and, when it is there, tmx-agent-dash into `backup`, which holds the old
    bundle, and write ROLLBACK.md there. Raises OSError when a copy or the note cannot be written."""
    copied = []
    # An install from before that move keeps tmx-agent-dash beside the bundle, not in it. This copy
    # must run before remove_old_program deletes that file.
    for name in ('haroldctl', 'tmx-agent-dash'):
        if (prefix / name).is_file():
            shutil.copy2(prefix / name, backup / name)
            copied.append(name)
    (backup / 'ROLLBACK.md').write_text(rollback_note(prefix, backup, copied, reinstall, metadata, data_copied))


def remove_old_program(prefix):
    """Delete the old tmx-agent-dash from `prefix`. The control-tower now lives in the bundle."""
    old = prefix / OLD_PROGRAM
    if old.is_file():
        try:
            old.unlink()
        except OSError as error:
            reason = error.strerror or error
            print('could not remove ~/bin/tmx-agent-dash ({}); delete it by hand'.format(reason), flush=True)
            return
        print('removed ~/bin/tmx-agent-dash (now harold start-control-tower)', flush=True)


COPY_MARKER = '.harold-data-copy'


def copy_data_to_backup(source_data, target):
    """Copy `source_data`/store and `source_data`/events, whichever exist, into `target`.

    Returns the number of bytes copied. Raises OSError when a copy fails."""
    total = 0
    for name in ('store', 'events'):
        source = source_data / name
        if source.is_dir():
            shutil.copytree(source, target / name)
            total += sum(file.stat().st_size for file in source.rglob('*') if file.is_file())
    return total


def has_copy_marker(backup):
    """Whether backup/data holds the marker of this run: a file that names `backup`.
    A stray marker from an earlier run names another backup and does not count."""
    try:
        return (backup / 'data' / COPY_MARKER).read_text() == backup.name
    except OSError:
        return False


def restore_live_data(stage, backup, copy_folder):
    """Undo a failed replace_bundle step by step, deciding from the folders and not from flags.

    `copy_folder` is backup/data-copy when a copy started, else None. The copy is removed only
    after the live data is back in `backup`, so the live data is never the only thing deleted."""
    if copy_folder is not None:
        if has_copy_marker(backup) and (stage / 'data').exists():
            # The marked copy took the name backup/data and the live data sits in the stage.
            discard = backup / 'data-discard'
            (backup / 'data').rename(discard)
            (stage / 'data').rename(backup / 'data')
            shutil.rmtree(discard, ignore_errors=True)
        else:
            shutil.rmtree(copy_folder, ignore_errors=True)
    for name in ('data', 'harold.log'):
        if not (backup / name).exists() and (stage / name).exists():
            (stage / name).rename(backup / name)


class DataCopyFailed(Exception):
    """The data copy into the backup failed. The old bundle is back in place."""

    def __init__(self, reason):
        super().__init__(reason)
        self.reason = reason


def replace_bundle(stage, bundle, reinstall, data_copy=None):
    """Move `bundle` to a backup folder, carry its data into `stage` and put `stage` in its place.

    Unless this is a reinstall, the data also goes into the backup as a copy. The copy is made
    while the live data is still in the backup, as backup/data-copy, and takes the name backup/data
    after the live data has moved out. `data_copy`, when given a dict, receives 'bytes' when the copy
    was made. A failed copy raises DataCopyFailed. Any exception, including an interrupt and
    DataCopyFailed, removes the partial copy and puts the live data back. The undo reads the folders, not flags, so an interrupt
    between any two steps still leaves the live data in the backup."""
    backup = None
    copy_folder = None
    if bundle.exists():
        suffix = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ') + '-' + uuid.uuid4().hex[:8]
        backup = bundle.with_name('harold.backup-' + suffix)
        bundle.rename(backup)
    try:
        if backup and not reinstall and any((backup / 'data' / name).is_dir() for name in ('store', 'events')):
            copy_folder = backup / 'data-copy'
            try:
                copied_bytes = copy_data_to_backup(backup / 'data', copy_folder)
            except OSError as error:
                # A partial copy must not look like a full one after the backup moves back.
                shutil.rmtree(copy_folder, ignore_errors=True)
                copy_folder = None
                raise DataCopyFailed(error.strerror or str(error)) from error
            else:
                # The marker proves a finished copy. The live data never carries it.
                (copy_folder / COPY_MARKER).write_text(backup.name)
                if data_copy is not None:
                    data_copy['bytes'] = copied_bytes
        if backup and not reinstall:
            for name in ('data', 'harold.log'):
                source = backup / name
                if source.exists():
                    destination = stage / name
                    if destination.is_dir():
                        shutil.rmtree(destination)
                    source.rename(destination)
        if copy_folder is not None:
            copy_folder.rename(backup / 'data')
        stage.rename(bundle)
    except BaseException:
        if backup:
            restore_live_data(stage, backup, copy_folder)
            backup.rename(bundle)
        if data_copy is not None:
            data_copy.clear()
        raise
    if copy_folder is not None:
        if has_copy_marker(backup):
            (backup / 'data' / COPY_MARKER).unlink(missing_ok=True)
    return backup


def stage_bundle(stage, bundle, artifacts, assets, prebuilt, repo, local, pairing):
    """Fill `stage` with the programs, hooks, mod, service script, configuration and skill that
    both the Mac and the Linux install put in the bundle."""
    (stage / 'config').mkdir(parents=True)
    (stage / 'hooks').mkdir()
    (stage / 'data/store').mkdir(parents=True)
    shutil.copy2(artifacts['harold'], stage / 'harold')
    shutil.copy2(artifacts['control-tower'], stage / 'control-tower')
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
    record_version(stage, prebuilt, repo)
    (stage / 'config/local.toml').write_bytes(local)
    (stage / 'config/local.toml').chmod(0o600)
    if pairing is not None:
        # Written at 0600 from the start: Harold refuses a pairing file others can read.
        descriptor = os.open(stage / 'config/pairing.env', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'wb') as handle:
            handle.write(pairing)
        (stage / 'config/pairing.env').chmod(0o600)


def probe_config(stage, bundle, metadata):
    """Run the staged harold's `--check-config` and set metadata['grpc_addr'] from it."""
    env = service.managed_environment(metadata['env'])
    env['HAROLD_CONFIG_DIR'] = str(stage / 'config')
    probe = subprocess.run([str(stage / 'harold'), '--check-config'], cwd=stage, env=env,
                           capture_output=True, text=True, timeout=10)
    if probe.returncode:
        # Typed config errors can contain credential values; relay only the fixed sentences.
        relayed = [line.strip() for line in probe.stderr.splitlines() if line.strip() in REFUSAL_SENTENCES]
        if relayed:
            raise RuntimeError('Configuration validation failed. Remove these settings, then run the install again:\n'
                               + '\n'.join(relayed))
        raise RuntimeError('Configuration validation failed; check the supplied TOML and required iMessage fields')
    config = json.loads(probe.stdout)
    metadata['grpc_addr'] = config['grpc_addr']
    if config['store_path'] != str(bundle / 'data/store'):
        raise RuntimeError('Configuration probe did not use the managed store path')


def install(args, repo):
    linux = sys.platform == 'linux'
    prebuilt = Path(args.prebuilt_dir).expanduser().resolve() if args.prebuilt_dir else None
    prerequisites(repo, prebuilt=prebuilt is not None)
    artifacts = prebuilt_files(prebuilt) if prebuilt is not None else None
    assets = prebuilt if prebuilt is not None else repo
    raw_prefix = Path(os.path.abspath(os.path.expanduser(args.prefix)))
    validate_destination(raw_prefix)
    prefix = raw_prefix
    bundle = prefix / 'harold'
    local = choose_config(args.config, bundle)
    pairing = carried_pairing(bundle)
    if artifacts is None:
        artifacts = build_binaries(repo, args.offline)
    if linux:
        # All before anything is copied: the build's own features, Rex, and the service manager.
        features = package_features(artifacts['harold'])
        print('Harold build features: {}'.format(', '.join(features) or 'none'), flush=True)
        rex = find_rex()
        check_rex_server(rex)
        check_service_manager()
        local = linux_config(local, rex)
    prefix.mkdir(parents=True, exist_ok=True)
    with service.service_lock(prefix):
        validate_destination(prefix)
        with tempfile.TemporaryDirectory(prefix='.harold-stage-', dir=prefix) as temporary:
            staging = Path(temporary)
            stage = staging / 'harold'
            stage_bundle(stage, bundle, artifacts, assets, prebuilt, repo, local, pairing)
            if linux:
                # No codesign, designated requirement, plutil or plist on Linux.
                metadata = make_linux_metadata(prefix, '', rex)
                probe_config(stage, bundle, metadata)
                (stage / 'service.json').write_text(json.dumps(metadata, indent=2) + '\n')
            else:
                metadata = make_metadata(prefix, '')
                reject_symlinks(Path(metadata['plist']))
                identity = args.signing_identity if args.signing_identity is not None else saved_signing_identity(bundle)
                metadata['signing_identity'] = identity
                for binary in (stage / 'harold', stage / 'control-tower'):
                    sign(binary, identity)
                old_requirement = designated_requirement(bundle / 'harold')
                new_requirement = designated_requirement(stage / 'harold')
                probe_config(stage, bundle, metadata)
                (stage / 'service.json').write_text(json.dumps(metadata, indent=2) + '\n')
                write_plist(stage / 'service.plist', metadata)
                checked(['plutil', '-lint', str(stage / 'service.plist')])
            launcher = '#!/bin/sh\nexec {} "$(dirname "$0")/harold/service.py" "$@"\n'.format(shlex.quote(sys.executable))
            (staging / 'haroldctl').write_text(launcher)
            (staging / 'haroldctl').chmod(0o755)
            old_metadata = metadata
            if (bundle / 'service.json').is_file():
                old_metadata = json.loads((bundle / 'service.json').read_text())
                same_service = (old_metadata.get('unit') == metadata['unit'] if linux
                                else old_metadata['label'] == metadata['label'])
                if old_metadata['bundle'] != str(bundle) or not same_service:
                    raise RuntimeError('Existing service metadata does not belong to this prefix')
            fresh_store = not args.reinstall and (bundle / 'data/events').is_dir() and not (bundle / 'data/store').exists()
            # Attribute listeners before stopping any currently running service.
            for current in (old_metadata, metadata):
                for pid in service.listener_pids(current):
                    if not service.executable_matches(pid, bundle / 'harold'):
                        raise RuntimeError('Port {} belongs to unrelated PID {}; leaving it running'.format(current['grpc_addr'], pid))
            # Boot out the old job without disabling it; start enables and loads the new copy.
            # On Linux, with no unit file there is no unit to stop, and `systemctl stop` would fail.
            if not linux or Path(metadata['unit_file']).is_file():
                service.unload(old_metadata)
            service.stop_unmanaged(old_metadata)
            data_copy = {}
            try:
                backup = replace_bundle(stage, bundle, args.reinstall, data_copy)
            except DataCopyFailed as failure:
                print('Install stopped: the data copy failed ({}). Nothing changed: the old Harold runs again '
                      'with its data. Free the space or fix the cause, then install again.'.format(failure.reason),
                      flush=True)
                try:
                    service.start(old_metadata)
                except Exception as error:
                    print('The old Harold did not start: {}. Start it with haroldctl start.'.format(error), flush=True)
                # The staging folder and the service lock release as SystemExit leaves the with blocks.
                raise SystemExit(1) from failure
            copy_kept = True
            if backup:
                if 'bytes' in data_copy:
                    print('copied data to {} ({:.1f} MB)'.format(shown(backup / 'data'), data_copy['bytes'] / 1_000_000),
                          flush=True)
                # Before haroldctl is overwritten below. Harold is stopped and the old bundle
                # has moved, so a failed rollback copy is reported and the install goes on.
                try:
                    keep_rollback_copy(prefix, backup, args.reinstall, metadata, 'bytes' in data_copy)
                except Exception as error:
                    copy_kept = False
                    old_stays = ('the old {} stays in place; '.format(OLD_PROGRAM)
                                 if (prefix / OLD_PROGRAM).is_file() else '')
                    print('Rollback copy incomplete ({}); {}'
                          'the old bundle is still in the folder below.'.format(
                              getattr(error, 'strerror', None) or error, old_stays), flush=True)
                print('Previous installation archived at {}'.format(backup), flush=True)
            if copy_kept:
                remove_old_program(prefix)
            os.replace(staging / 'haroldctl', prefix / 'haroldctl')
            if linux:
                write_unit(Path(metadata['unit_file']), metadata)
                result = service.systemctl('daemon-reload')
                if result.returncode:
                    raise RuntimeError('systemctl --user daemon-reload failed: {}'.format(result.stderr.strip()))
            else:
                write_launch_agent(metadata)
            pid = service.start(metadata)
            print('Harold ready at {} ({} PID {}).\nControl: {}\nConfig: {}\nLog: {}'.format(
                metadata['grpc_addr'], 'systemd' if linux else 'LaunchAgent', pid, prefix / 'haroldctl',
                bundle / 'config/local.toml', bundle / 'harold.log'))
            print('restart the control-tower to see blocked, done and error', flush=True)
            if not linux:
                message = permissions_message(old_requirement,
                                              new_requirement, identity, bundle / 'harold')
                if message:
                    print(message, flush=True)
            outcome = link_claude_mod(CLAUDE_SETTINGS, bundle / 'hooks/claude-mod')
            print('Claude Code mod: {}'.format(outcome), flush=True)
            outcome = register_turn_hook(CLAUDE_SETTINGS, bundle / 'hooks' / TURN_HOOK)
            print('Claude Code turn hook: {}'.format(outcome), flush=True)
            for agent, skills_folder in AGENT_SKILL_FOLDERS:
                print('{} skill: {}'.format(agent, link_skill(skills_folder, bundle / 'skills' / SKILL_NAME)),
                      flush=True)
            if fresh_store:
                print('New store: {}\nNot carried over from {}: messages still waiting to be delivered, '
                      'and the mute and pause settings (voice is on, messaging unpaused). '
                      'That folder is left as it was.'.format(bundle / 'data/store', bundle / 'data/events'),
                      flush=True)
            if linux:
                note = linger_note()
                if note:
                    print(note, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--reinstall', action='store_true', help='archive the old bundle and start with fresh managed data')
    parser.add_argument('--config', help='import a TOML local configuration')
    parser.add_argument('--prefix', default=str(Path.home() / 'bin'), help='executable directory (default: ~/bin)')
    parser.add_argument('--signing-identity',
                        help='codesign identity, saved for later installs (default: the saved one, else ad-hoc -)')
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
