#!/usr/bin/env python3
"""Plays a phone call end to end against a stand-in chief and times each step.

It starts a test Harold on a scratch config and a scratch tmux server, runs Claude Code in a
tmux session `harold-call-test` as the chief, and then does what the phone does: it follows
GET /updates, posts a spoken question to /call/audio, fetches the answer, reports it played
and hangs up. The real chief's pane, the installed Harold's calls and the user's thread are
never touched. Python 3 standard library only.

For each call it reports the time from POST /call to the screen showing "listening", the
sounds the phone was offered before then in order (ringback cycles, then the pick-up; a main
that rings nothing offers none), whether any "One moment." line appeared in #call, and for
each turn `first_sound_ms` and `answer_ms` as the phone sees them, from the end of the speech
(Harold cuts the turn about 1.2 s later). Run it with `--harold` on a build of main and on a
build of a branch to compare them. Like the phone, it tells Harold a web app page is on
screen (POST /page/seen) from the start, so a Harold that loads its voice worker for an open
page does so before the call; `--wait-for-page-load` waits for that.

The test Harold never downloads a speech model and never changes the Hugging Face cache: it
is given a record of the models already in the cache, so it has nothing to check or fetch.
"""

import argparse
import html
import http.client
import io
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import wave

SESSION = 'harold-call-test'
LOGIN = 'call-test'
# The call answerer answers the first question from the todo seeded in the scratch store; the
# second is a request for work, which it hands over to the stand-in chief.
QUESTION = 'How many to do items are waiting'
REQUEST = 'Ask the chief to start the build again'
FROM_THE_CHIEF = 'From the chief.'
ONE_MOMENT = 'One moment.'
NO_CHIEF = "There's no chief running."
TODO_ID = 'QCALLTEST'
NO_RECIPIENT = 'call-test@harold.invalid'
ANSWER_LINE = 'Call test answer.'
SAMPLE_RATE = 16000
PIECE_SAMPLES = 4000
LEAD_S = 1
TAIL_S = 2
WORD_SHARE = 0.8
INSTALL_DIR = Path.home() / 'bin' / 'harold'
# The speech models Harold pins, as `docs/references/voice-input/README.md` lists them.
WHISPER_MODEL = 'mlx-community/whisper-large-v3-turbo'
WHISPER_REVISION = 'a4aaeec0636e6fef84abdcbe3544cb2bf7e9f6fb'
KOKORO_MODEL = 'mlx-community/Kokoro-82M-bf16'
KOKORO_REVISION = 'a71e4d38b236d968966a2002c4c895dbd12b1c3c'
# How often the phone says a page is on screen (Harold counts a page for 75 s).
SEEN_EVERY = 20
# `--tools=Bash` has an equals sign because `--tools Bash "Hello"` reads "Hello" as a second tool.
STAND_IN = 'claude --model haiku --permission-mode dontAsk --tools=Bash "Hello"'
# How long the test sends no other key after Enter on the folder question, and after a move key.
LIMIT_FOLDER_QUESTION = 5
FOLDER_KEY_GAP = 1

# The time limit of each step, in seconds. The ones after the question is posted count from
# when the question ended.
LIMIT_CALL = 20
LIMIT_CUT = 8
LIMIT_HEARD = 30
LIMIT_TYPED = 45
LIMIT_SOUND = 60
LIMIT_ANSWER = 25
LIMIT_FETCH = 5
LIMIT_HANGUP = 10
LIMIT_READY = 90
LIMIT_PAGE_LOAD = 90
LIMIT_LISTENING = 90


class StepFailed(Exception):
    """A step's check did not hold; the message says why."""


class StepResult:
    def __init__(self, name, ok, ms, detail=''):
        self.name = name
        self.ok = ok
        self.ms = ms
        self.detail = detail


def run_step(name, limit, step, since=None):
    """Runs `step`, which answers its detail or raises StepFailed, and times it from `since`
    (monotonic seconds; the step's start when omitted). A step over `limit` seconds fails."""
    started = time.monotonic() if since is None else since
    try:
        detail = step() or ''
    except StepFailed as failure:
        return StepResult(name, False, _ms_since(started), str(failure))
    ms = _ms_since(started)
    if ms > limit * 1000:
        return StepResult(
            name, False, ms, '{} took {} ms, past its limit of {} ms'.format(name, ms, int(limit * 1000)))
    return StepResult(name, True, ms, detail)


def _ms_since(started):
    return int((time.monotonic() - started) * 1000)


def wait_until(check, limit, what, every=0.05, since=None):
    """Calls `check` until it answers something truthy, and answers that. Raises StepFailed
    naming `what` once `limit` seconds have passed since `since` (now when omitted)."""
    started = time.monotonic() if since is None else since
    while True:
        value = check()
        if value:
            return value
        if time.monotonic() - started >= limit:
            raise StepFailed('{}: not seen within {} ms'.format(what, int(limit * 1000)))
        time.sleep(every)


def report(results):
    """The lines to print, one per step, and the exit code."""
    lines = []
    for result in results:
        mark = 'ok  ' if result.ok else 'FAIL'
        line = '{} {:<44} {:>6} ms'.format(mark, result.name, result.ms)
        if result.detail:
            line += '  ' + result.detail
        lines.append(line)
    return lines, 0 if all(result.ok for result in results) else 1


# --- the scratch config ---------------------------------------------------------------

def table_text(config, name):
    """The `[name]` table of the TOML `config`, header included; '' when it has none."""
    lines = config.splitlines()
    header = '[{}]'.format(name)
    if header not in (line.strip() for line in lines):
        return ''
    start = next(i for i, line in enumerate(lines) if line.strip() == header)
    end = len(lines)
    for i in range(start + 1, len(lines)):
        if lines[i].lstrip().startswith('['):
            end = i
            break
    return '\n'.join(lines[start:end]).rstrip() + '\n'


def scratch_config(installed, chief_dir, grpc_port, web_port, store_path, chat_db_path, log_level=None):
    """The test Harold's local.toml. [voice_input] and [tts] come from the installed config.
    Nothing reaches the phone or another pane. Harold refuses to start without an iMessage
    recipient and a handle, so those are placeholders that name nobody (a `.invalid` address
    and handle 0); no timed prompts are set, the iMessage and machine-audio lists are empty,
    and the Messages database is a scratch file that has no messages."""
    parts = [
        '[grpc]\nport = {}\n'.format(grpc_port),
        # The test Harold's phone is a script, not a paired device.
        '[web_app]\nlisten = "127.0.0.1:{}"\nallowed_login = "{}"\nrequire_pairing = false\n'.format(web_port, LOGIN),
        '[store]\npath = "{}"\n'.format(store_path),
        '[imessage]\nrecipient = "{}"\nhandle_ids = [0]\n'.format(NO_RECIPIENT),
        '[chat_db]\npath = "{}"\n'.format(chat_db_path),
        '[telemetry]\nexport = false\n',
        '[notify]\nchief = "{0}"\nchief_session = "{1}"\nweb_app = ["{0}"]\n'
        'imessage = []\nmachine_audio_output = []\n'.format(chief_dir, SESSION),
        table_text(installed, 'voice_input'),
        table_text(installed, 'tts'),
        '[log]\nlevel = "{}"\n'.format(log_level) if log_level else '',
    ]
    return '\n'.join(part for part in parts if part)


# --- the speech models already on the Mac ----------------------------------------------

def hub_cache(env):
    """The Hugging Face cache a program with `env` reads: HF_HUB_CACHE, else HF_HOME/hub,
    else ~/.cache/huggingface/hub."""
    if env.get('HF_HUB_CACHE'):
        return Path(env['HF_HUB_CACHE'])
    if env.get('HF_HOME'):
        return Path(env['HF_HOME']) / 'hub'
    return Path(env.get('HOME', str(Path.home()))) / '.cache' / 'huggingface' / 'hub'


def configured_models(installed):
    """The (model, commit) of Whisper and Kokoro that the installed config names, with the
    commits Harold pins by default."""
    config = tomllib.loads(installed)
    args = config.get('voice_input', {}).get('args', [])
    whisper = args[args.index('--model') + 1] if '--model' in args else WHISPER_MODEL
    tts_args = config.get('tts', {}).get('args') or []
    kokoro = tts_args[tts_args.index('--model') + 1] if '--model' in tts_args else KOKORO_MODEL
    return [
        (whisper, config.get('voice_input', {}).get('model_revision', WHISPER_REVISION)),
        (kokoro, config.get('tts', {}).get('model_revision', KOKORO_REVISION)),
    ]


def snapshot_files(snapshot):
    """Each file in a cached snapshot as {path, size}, read from the folder alone."""
    files = []
    for folder, _, names in os.walk(snapshot, followlinks=True):
        for name in sorted(names):
            path = Path(folder) / name
            if path.is_file():
                files.append({
                    'path': str(path.relative_to(snapshot)),
                    'size': path.stat().st_size,
                    'sha256': '',
                })
    return files


def speech_model_records(installed, cache):
    """The records of the last check, by file name, for the models the installed config names
    that the cache holds, so Harold finds nothing to check or fetch. Nothing is read from the
    network and nothing in the cache is changed."""
    records = {}
    for model, commit in configured_models(installed):
        snapshot = cache / ('models--' + model.replace('/', '--')) / 'snapshots' / commit
        if not snapshot.is_dir():
            continue
        name = '{}@{}.json'.format(model.replace('/', '--'), commit)
        records[name] = json.dumps(
            {'model': model, 'commit': commit, 'files': snapshot_files(snapshot)})
    return records


# --- the question ---------------------------------------------------------------------

def pieces(pcm, first_at_ms=0, size=PIECE_SAMPLES, rate=SAMPLE_RATE):
    """Splits little-endian i16 `pcm` into pieces of `size` samples, each with `at`, the
    milliseconds from the call's start to where it was recorded."""
    step = size * 2
    return [
        (first_at_ms + (offset // 2) * 1000 // rate, pcm[offset:offset + step])
        for offset in range(0, len(pcm), step)
    ]


def phone_audio(question_pcm):
    """The question with 1 s of silence before it and 2 s after."""
    return b'\0' * (2 * SAMPLE_RATE * LEAD_S) + question_pcm + b'\0' * (2 * SAMPLE_RATE * TAIL_S)


def words(text):
    return re.findall(r"[\w']+", text.lower())


def word_match(heard, question):
    """The share of the question's words that `heard` holds."""
    asked = words(question)
    if not asked:
        return 0.0
    heard_words = words(heard)
    found = 0
    for word in asked:
        if word in heard_words:
            heard_words.remove(word)
            found += 1
    return found / len(asked)


def heard_enough(heard, question, share=WORD_SHARE):
    return word_match(heard, question) >= share - 1e-9


def speak_question(path, text=QUESTION):
    """Makes `text` as 16 kHz mono i16 samples with `say`."""
    subprocess.run(
        ['say', '-o', str(path), '--data-format=LEI16@16000', text], check=True, timeout=60)
    with wave.open(str(path), 'rb') as wav:
        if (wav.getframerate(), wav.getnchannels(), wav.getsampwidth()) != (SAMPLE_RATE, 1, 2):
            raise StepFailed('say did not write 16 kHz mono 16-bit audio')
        return wav.readframes(wav.getnframes())


# --- the test Harold's log ------------------------------------------------------------

ANSI = re.compile(r'\x1b\[[0-9;]*m')


def clean_log(text):
    return ANSI.sub('', text)


def typed_panes(log):
    """The pane id of each `spoken prompt typed` line."""
    panes = []
    for line in clean_log(log).splitlines():
        found = re.search(r'pane_id=(%\d+)', line)
        if 'spoken prompt typed' in line and found:
            panes.append(found.group(1))
    return panes


def hand_over_panes(log):
    """The pane id of each `hand-over typed` line."""
    panes = []
    for line in clean_log(log).splitlines():
        found = re.search(r'pane_id=(%\d+)', line)
        if 'hand-over typed' in line and found:
            panes.append(found.group(1))
    return panes


def call_ended(log, reason):
    return re.search(r'call ended.*?reason="?{}\b'.format(re.escape(reason)), clean_log(log))


# --- what #call shows -----------------------------------------------------------------

CALL_SECTION = re.compile(r'<section id="call"[^>]*>', re.S)


def is_answer(shown):
    """Whether `shown` is the chief's answer sound, not a tick or one of Harold's phrases."""
    return bool(shown['sound']) and shown['line'] == ANSWER_LINE


def sound_name(body):
    """What a sound Harold offered is, from the length of its WAV: the ringback is one 3 s
    cycle, the pick-up 0.15 s, main's connecting tone 0.6 s and the tick 0.04 s."""
    try:
        with wave.open(io.BytesIO(body)) as sound:
            seconds = sound.getnframes() / sound.getframerate()
    except (wave.Error, EOFError, ZeroDivisionError):
        return 'sound'
    if seconds >= 2.5:
        return 'ringback'
    if abs(seconds - 0.6) < 0.05:
        return 'connecting'
    if abs(seconds - 0.15) < 0.03:
        return 'pickup'
    if seconds < 0.1:
        return 'tick'
    return 'sound'


def sound_summary(names):
    """The sounds in order, a run of the same one counted: ["ringback", "ringback", "pickup"]
    is "ringback x2, pickup"; none is "none"."""
    parts = []
    for name in names:
        if parts and parts[-1][0] == name:
            parts[-1][1] += 1
        else:
            parts.append([name, 1])
    if not parts:
        return 'none'
    return ', '.join(name if count == 1 else '{} x{}'.format(name, count) for name, count in parts)


def one_moment_sounds(events):
    """How many sounds in #call's events said "One moment."."""
    ids = set()
    for _, shown in events:
        if shown['sound'] and shown['line'] == ONE_MOMENT:
            ids.add(shown['sound'])
    return len(ids)


def parse_call(patch):
    """What the `#call` section in an event-stream patch shows, or None when it has none."""
    opening = CALL_SECTION.search(patch)
    if not opening:
        return None

    def attribute(name):
        found = re.search(r'{}="([^"]*)"'.format(name), opening.group(0))
        return html.unescape(found.group(1)) if found else ''

    heard = re.search(r'class="call-heard">You said:<br><span>"🎙 (.*?)"</span>', patch, re.S)
    line = re.search(r'class="call-line">"(.*?)"</p>', patch, re.S)
    return {
        'state': attribute('data-harold-state'),
        'sound': attribute('data-harold-sound'),
        'heard': html.unescape(heard.group(1)) if heard else '',
        'line': html.unescape(line.group(1)) if line else '',
    }


class Phone:
    """What the phone does: requests with its headers, and the event stream it follows."""

    def __init__(self, port):
        self.port = port
        self.host = '127.0.0.1:{}'.format(port)
        self.events = []
        self.changed = threading.Condition()
        self.stream = None
        self.closing = False

    def request(self, method, path, body=None, content_type='application/json', timeout=30):
        headers = {'Tailscale-User-Login': LOGIN}
        if method != 'GET':
            headers['Origin'] = 'http://' + self.host
            if path.split('?')[0] == '/call/audio':
                # `call.js` posts the audio with `fetch`, not Datastar (ADR 0018).
                headers['Content-Type'] = content_type
            else:
                # What Datastar's `@post` and `@delete` send.
                headers['Datastar-Request'] = 'true'
                headers['Content-Type'] = 'application/json'
                if body is None:
                    body = '{}'
        connection = http.client.HTTPConnection('127.0.0.1', self.port, timeout=timeout)
        try:
            connection.request(method, path, body=body, headers=headers)
            response = connection.getresponse()
            return response.status, response.getheader('Content-Type', ''), response.read()
        finally:
            connection.close()

    def follow_updates(self):
        """Follows GET /updates on its own thread, recording each change of #call."""
        thread = threading.Thread(target=self._read_updates, daemon=True)
        thread.start()

    def _read_updates(self):
        try:
            connection = http.client.HTTPConnection('127.0.0.1', self.port, timeout=300)
            connection.request('GET', '/updates', headers={'Tailscale-User-Login': LOGIN})
            response = connection.getresponse()
            self.stream = connection
            data = []
            while not self.closing:
                line = response.readline()
                if not line:
                    return
                line = line.decode('utf-8', 'replace').rstrip('\r\n')
                if line.startswith('data:'):
                    data.append(line[5:].lstrip())
                elif not line and data:
                    self._record('\n'.join(data))
                    data = []
        except (OSError, ValueError, AttributeError, http.client.HTTPException):
            return

    def _record(self, patch):
        shown = parse_call(patch)
        if shown is None:
            return
        with self.changed:
            self.events.append((time.monotonic(), shown))
            self.changed.notify_all()

    def since(self, mark, test):
        """The first recorded change at or after index `mark` that `test` accepts, as
        (index, time, shown), or None."""
        with self.changed:
            for index in range(mark, len(self.events)):
                at, shown = self.events[index]
                if test(shown):
                    return index, at, shown
        return None

    def keep_seen(self, stop):
        """Says a page is on screen now and every `SEEN_EVERY` s, as the app does while it is
        in front, until `stop` is set."""
        def beat():
            while True:
                try:
                    self.request('POST', '/page/seen', timeout=10)
                except (OSError, http.client.HTTPException):
                    pass
                if stop.wait(SEEN_EVERY):
                    return

        threading.Thread(target=beat, daemon=True).start()

    def close(self):
        self.closing = True
        if self.stream is not None:
            try:
                self.stream.close()
            except OSError:
                pass


# --- the run --------------------------------------------------------------------------

def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


def tmux(env, *args, check=True):
    return subprocess.run(['tmux', '-f', '/dev/null', *args], env=env, text=True, capture_output=True, check=check, timeout=15)


class Run:
    """One call test: the scratch folder, the test Harold and the stand-in chief."""

    def __init__(self, install_dir, keep, log_level=None, binary=None, default_toml=None,
                 wait_for_page_load=False):
        self.install_dir = install_dir
        self.binary = binary or install_dir / 'harold'
        self.default_toml = default_toml or install_dir / 'config' / 'default.toml'
        self.wait_for_page_load = wait_for_page_load
        self.seen_stop = threading.Event()
        self.log_level = log_level
        self.keep = keep
        self.scratch = Path(os.path.realpath(tempfile.mkdtemp(prefix='hct-')))
        self.config_dir = self.scratch / 'config'
        self.chief_dir = self.scratch / 'chief'
        self.log_path = self.scratch / 'harold.log'
        self.harold = None
        self.log_file = None
        self.pane = None
        self.pane_snapshot = ''
        self.stand_in_text = ''
        self.stand_in_started = None
        self.stand_in_screens = []
        self.stand_in_keys = []
        self.stand_in_to_print = False
        self.folder_key_at = None
        self.folder_enter = False
        self.phone = None
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith('HAROLD') and key != 'TMUX' and key != 'TMUX_PANE'
        }
        self.env['TMUX_TMPDIR'] = str(self.scratch / 't')
        self.env['HAROLD_CONFIG_DIR'] = str(self.config_dir)
        self.env['NO_COLOR'] = '1'

    # setup

    def write_config(self):
        installed = (self.install_dir / 'config' / 'local.toml').read_text()
        for folder in (self.config_dir, self.chief_dir / '.claude', self.scratch / 't'):
            folder.mkdir(parents=True)
        shutil.copy(self.default_toml, self.config_dir / 'default.toml')
        # The speech models are in the cache already: a record of them in the test Harold's own
        # data folder (beside its store) leaves it nothing to check or download.
        records = self.scratch / 'speech-models'
        records.mkdir(mode=0o700)
        for name, record in speech_model_records(installed, hub_cache(os.environ)).items():
            (records / name).write_text(record)
        self.grpc_port = free_port()
        self.web_port = free_port()
        (self.config_dir / 'local.toml').write_text(scratch_config(
            installed,
            chief_dir=str(self.chief_dir),
            grpc_port=self.grpc_port,
            web_port=self.web_port,
            store_path=str(self.scratch / 'store'),
            chat_db_path=str(self.scratch / 'no-messages.db'),
            log_level=self.log_level,
        ))

    def start_harold(self):
        self.log_file = open(self.log_path, 'wb')
        # Its own TMPDIR: a starting Harold sweeps leftover call-* folders of
        # $TMPDIR/harold-voice-input, which must not be the installed Harold's.
        (self.scratch / 'tmp').mkdir()
        harold_env = dict(self.env, TMPDIR=str(self.scratch / 'tmp'))
        self.harold = subprocess.Popen(
            [str(self.binary)], env=harold_env, stdout=self.log_file,
            stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, start_new_session=True)
        self.phone = Phone(self.web_port)

        def up():
            if self.harold.poll() is not None:
                raise StepFailed('the test Harold exited with {}'.format(self.harold.returncode))
            try:
                return self.phone.request('GET', '/', timeout=3)[0] == 200
            except OSError:
                return False

        wait_until(up, LIMIT_READY, 'the test Harold answers on its web port', every=0.2)

    def start_chief(self):
        """A tmux session whose pane runs Claude Code in the chief folder, told to answer each
        spoken line with `harold speak`. The pane id is known once the session exists, so the
        pane holds `cat` until the folder's files name it, and then runs Claude Code."""
        created = tmux(
            self.env, 'new-session', '-d', '-s', SESSION, '-c', str(self.chief_dir),
            '-x', '160', '-y', '50', '-P', '-F', '#{pane_id}', 'cat')
        self.pane = created.stdout.strip()
        # harold speak takes a tmux pane id as --src-id, so the stand-in's own pane id goes in.
        # The command runs through a link in the stand-in's folder, so no file names a path outside it.
        link = self.chief_dir / 'bin' / 'harold'
        link.parent.mkdir(exist_ok=True)
        link.symlink_to(self.binary)
        speak = '{} speak --src-id {}'.format(link, self.pane)
        (self.chief_dir / 'CLAUDE.md').write_text(
            'Each line that starts with 🎙 is a spoken request or question. Answer it by running exactly\n'
            '`{} "{}"` and do nothing else: no other command, no other text.\n'
            'Answer any other line with the one word: ok.\n'.format(speak, ANSWER_LINE))
        # No hooks: the user's own Stop hook reports to the installed Harold's address, not to
        # HAROLD_CONFIG_DIR's, so a turn here would reach the real Harold as the pane %0.
        (self.chief_dir / '.claude' / 'settings.json').write_text(
            '{{"disableAllHooks": true, "permissions": {{"defaultMode": "dontAsk", '
            '"allow": ["Bash({}:*)"]}}}}\n'.format(speak))
        self.run_stand_in()

    def run_stand_in(self):
        """Runs Claude Code in the stand-in's pane, with one first turn, so the input box has no
        suggestion in it, which Harold reads as text. CALL_TEST_STAND_IN runs another command
        there (for the forced-stop test). The pane stays when its program ends, so the test can
        read its last screen."""
        tmux(self.env, 'set-option', '-w', '-t', self.pane, 'remain-on-exit', 'on')
        self.stand_in_started = time.monotonic()
        tmux(self.env, 'respawn-pane', '-k', '-t', self.pane, os.environ.get('CALL_TEST_STAND_IN') or STAND_IN)

    def pane_text(self):
        return tmux(self.env, 'capture-pane', '-p', '-t', self.pane, check=False).stdout

    def pane_state(self):
        """Answers (pane_dead, pane_dead_status) as tmux words, or None when tmux does not answer."""
        shown = tmux(self.env, 'display-message', '-p', '-t', self.pane,
                     '#{pane_dead} #{pane_dead_status}', check=False)
        if shown.returncode != 0:
            return None
        words = shown.stdout.split()
        return (words[0], words[1] if len(words) > 1 else '') if words else None

    def stand_in_ms(self):
        return _ms_since(self.stand_in_started) if self.stand_in_started is not None else 0

    def see_screen(self):
        """Keeps the pane's history and screen when it differs from the one kept last."""
        shown = tmux(self.env, 'capture-pane', '-p', '-S', '-200', '-t', self.pane, check=False).stdout.rstrip()
        if not self.stand_in_screens or self.stand_in_screens[-1][1] != shown:
            self.stand_in_screens.append((self.stand_in_ms(), shown))

    def send_key(self, key):
        self.stand_in_keys.append((self.stand_in_ms(), key))
        tmux(self.env, 'send-keys', '-t', self.pane, key, check=False)

    def stand_in_report(self):
        """What the test knows of the stand-in: whether its program ended, the time since its
        start, the last 3 different screens and every key sent, each with its time."""
        if self.pane is None:
            return 'the stand-in chief was not started'
        try:
            self.see_screen()
            state = self.pane_state()
        except subprocess.SubprocessError:
            state = None
        if state is None:
            ended = 'pane_dead and pane_dead_status: tmux did not answer'
        else:
            ended = 'pane_dead={} pane_dead_status={}'.format(state[0], state[1])
        lines = ["the stand-in chief's report, {} ms after its start".format(self.stand_in_ms()), ended]
        for at, shown in self.stand_in_screens[-3:]:
            lines.append('--- screen at {} ms ---'.format(at))
            lines.append(shown)
        lines.append('--- keys sent ---')
        lines.extend('{} ms  {}'.format(at, key) for at, key in self.stand_in_keys)
        if not self.stand_in_keys:
            lines.append('none')
        return '\n'.join(lines)

    def stand_in_stop(self):
        """The report, also written to stand-in.log in the scratch folder."""
        text = self.stand_in_report()
        self.stand_in_to_print = False
        try:
            (self.scratch / 'stand-in.log').write_text(text + '\n')
        except OSError:
            pass
        return text

    def wait_for_chief_idle(self):
        """Waits for Claude Code's empty input box, trusting its scratch folder when it asks. It
        fails at once when the stand-in's program has ended."""
        def idle():
            text = self.pane_text()
            self.see_screen()
            state = self.pane_state()
            if state is None:
                raise StepFailed('tmux does not answer: the stand-in chief is gone after {} ms'.format(self.stand_in_ms()))
            if state[0] == '1':
                raise StepFailed('the stand-in chief exited with {} after {} ms'.format(
                    state[1] or 'no status', self.stand_in_ms()))
            if 'Yes, I trust this folder' in text:
                self.answer_folder_question(text)
                return False
            return '⏺' in text and '❯' in text and 'esc to interrupt' not in text

        try:
            wait_until(idle, LIMIT_READY, 'the stand-in chief shows its input box', every=0.5)
        except StepFailed:
            self.stand_in_stop()
            self.stand_in_to_print = True
            raise
        # Harold reads the panes every 1.2 s.
        time.sleep(3)

    def answer_folder_question(self, text):
        """Moves the highlight to the "Yes" choice, found by its text, and sends Enter when the
        screen shows the highlight there. Claude Code can drop a key it gets while it starts, so
        Enter waits for the screen to show the move. It sends one key at a time, and no key
        for FOLDER_KEY_GAP seconds after a key."""
        choices = []
        chosen = 0
        for line in text.splitlines():
            found = re.match(r'\s*(❯|>)?\s*(?:\d+\.\s*)?((?:Yes|No)\b.*?)\s*$', line)
            if found:
                if found.group(1):
                    chosen = len(choices)
                choices.append(found.group(2))
        wanted = [number for number, name in enumerate(choices) if name.startswith('Yes')]
        if not wanted:
            raise StepFailed('the folder question shows no "Yes" choice:\n' + text.rstrip())
        gap = LIMIT_FOLDER_QUESTION if self.folder_enter else FOLDER_KEY_GAP
        if self.folder_key_at is not None and time.monotonic() - self.folder_key_at < gap:
            return
        self.folder_key_at = time.monotonic()
        if wanted[0] == chosen:
            self.folder_enter = True
            self.send_key('Enter')
        else:
            self.send_key('Down' if wanted[0] > chosen else 'Up')

    def seed_todo(self):
        """One todo in the scratch store, through the test Harold's question tool."""
        todo = (
            '{{"id": "{}", "product": "harold", "spec": "call-test", "sha": "abc123", '
            '"link": "https://example.com/call-test.md", "intro": "Which way?", "decisions": '
            '[{{"ask": "way", "options": [{{"key": "a", "label": "left", "impact": "", '
            '"suggested": true}}, {{"key": "b", "label": "right", "impact": ""}}]}}]}}'
        ).format(TODO_ID)
        put = subprocess.run(
            [str(self.binary), 'tool', 'questions', 'put'], input=todo, text=True,
            env=self.env, capture_output=True, timeout=30)
        if put.returncode != 0:
            raise StepFailed('seeding the todo failed: ' + (put.stdout + put.stderr).strip())
        return 'todo ' + TODO_ID

    def log(self):
        try:
            return clean_log(self.log_path.read_text(errors='replace'))
        except OSError:
            return ''

    # teardown

    def tear_down(self):
        self.seen_stop.set()
        try:
            if self.pane is not None:
                self.pane_snapshot = self.pane_text().rstrip()
                self.stand_in_text = self.stand_in_report()
        except subprocess.SubprocessError:
            pass
        if self.phone is not None:
            try:
                self.phone.request('DELETE', '/call', timeout=10)
            except (OSError, http.client.HTTPException):
                pass
            self.phone.close()
        for kill in (('kill-session', '-t', SESSION), ('kill-server',)):
            try:
                tmux(self.env, *kill, check=False)
            except subprocess.SubprocessError:
                pass
        if self.harold is not None and self.harold.poll() is None:
            self.harold.send_signal(signal.SIGTERM)
            try:
                self.harold.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(self.harold.pid, signal.SIGKILL)
                self.harold.wait()
        if self.log_file is not None:
            self.log_file.close()

    def finish(self, passed):
        if not passed and self.stand_in_to_print:
            print(self.stand_in_text)
        if passed and not self.keep:
            shutil.rmtree(self.scratch, ignore_errors=True)
            return
        print('scratch folder kept: {} (the test Harold\'s log is harold.log)'.format(self.scratch))


class Call:
    """The phone's side of one call: it posts spoken questions, follows what #call shows, and
    fetches and reports every sound Harold offers, as the phone does."""

    def __init__(self, run):
        self.run = run
        self.phone = run.phone
        self.mark = 0
        self.call_id = None
        self.call_sent = 0.0
        self.handled = set()
        self.wavs = {}
        self.sound_names = []
        self.last_heard = ''

    def serve(self, shown):
        """Fetches and reports a sound once; keeps what it fetched."""
        sound = shown['sound']
        if not sound or sound in self.handled:
            return
        self.handled.add(sound)
        try:
            status, kind, body = self.phone.request('GET', '/call/answer/' + sound)
            if status == 200:
                self.wavs[sound] = (kind, body)
                self.sound_names.append(sound_name(body))
            self.phone.request('POST', '/call/played?id={}&start_ms=0'.format(sound))
        except (OSError, http.client.HTTPException):
            pass

    def sounds_since(self, mark):
        with self.phone.changed:
            return [shown for _, shown in self.phone.events[mark:] if shown['sound']]

    def wait_for_line(self, test, limit, what, since, mark):
        """Waits for a sound at or after event `mark` whose line `test` accepts, serving every
        sound meanwhile."""
        def found():
            for shown in self.sounds_since(mark):
                self.serve(shown)
                if shown['line'] and test(shown['line']):
                    return shown
            return None

        return wait_until(found, limit, what, since=since)

    def post_question(self, audio):
        """Posts `audio` in pieces stamped from the call's start, in real time. Answers when
        the speech has ended, as a monotonic time."""
        base_ms = int((time.monotonic() - self.call_sent) * 1000)
        for at_ms, piece in pieces(audio, first_at_ms=base_ms):
            due = self.call_sent + at_ms / 1000 + PIECE_SAMPLES / SAMPLE_RATE
            time.sleep(max(0.0, due - time.monotonic()))
            status, _, _ = self.phone.request(
                'POST', '/call/audio?call={}&at={}'.format(self.call_id, at_ms), piece,
                'application/octet-stream')
            if status != 204:
                raise StepFailed('POST /call/audio answered {}'.format(status))
        return time.monotonic()


def play(run):
    """The call itself: every step, in order. Answers the results."""
    results = []
    phone = run.phone
    call = Call(run)

    def add(result):
        results.append(result)
        return result.ok

    question = speak_question(run.scratch / 'q.wav')
    request = speak_question(run.scratch / 'r.wav', REQUEST)
    phone.follow_updates()
    wait_until(lambda: phone.events, 10, 'the call screen on /updates')
    call.mark = len(phone.events)

    # 0. The page is on screen, as it is when the user taps Call. A Harold that loads its voice
    # worker for an open page does so now.
    phone.keep_seen(run.seen_stop)
    if run.wait_for_page_load:
        def page_load():
            wait_until(lambda: 'voice worker ready' in run.log(), LIMIT_PAGE_LOAD,
                       'the log line "voice worker ready" for the open page')
            return worker_ready_line(run.log())

        if not add(run_step('the voice worker loads for the open page', LIMIT_PAGE_LOAD, page_load)):
            return results

    # 1. POST /call answers a Datastar event that patches `_callId`, or `_callWhy` when no call
    # starts. Retried while the test Harold has not yet seen the stand-in chief.
    def post_call():
        started = time.monotonic()
        while True:
            status, _, body = phone.request('POST', '/call')
            if status != 200:
                raise StepFailed('POST /call answered {} {}'.format(status, body.decode(errors='replace')))
            signals = call_signals(body.decode(errors='replace'))
            if signals.get('_callId'):
                call.call_id = signals['_callId']
                return 'call {}'.format(call.call_id[:8])
            why = signals.get('_callWhy', '')
            if why != NO_CHIEF or time.monotonic() - started > LIMIT_CALL:
                raise StepFailed('POST /call started no call: {}'.format(why or body.decode(errors='replace')))
            time.sleep(1)

    call.call_sent = time.monotonic()
    if not add(run_step('POST /call answers 200', LIMIT_CALL, post_call)):
        return results

    def listening():
        def shown_listening():
            # The phone fetches and reports each sound of the ringing as it is offered.
            for shown in call.sounds_since(call.mark):
                call.serve(shown)
            return phone.since(call.mark, lambda s: s['state'] == 'listening')

        found = wait_until(shown_listening, LIMIT_LISTENING, 'the call screen shows listening',
                           since=call.call_sent)
        connecting = phone.since(call.mark, lambda s: s['state'] == 'connecting')
        return 'connecting first' if connecting and connecting[1] < found[1] else 'listening at once'

    if not add(run_step('tap_to_listening: POST /call to listening', LIMIT_LISTENING, listening,
                        since=call.call_sent)):
        return results

    add(StepResult('the sounds before listening, in order', True, 0, sound_summary(call.sound_names)))

    def leg(label, text, audio_pcm):
        """Posts one spoken turn and checks it is cut and heard. Answers (ok, spoke_until, mark)."""
        mark = len(phone.events)
        audio = phone_audio(audio_pcm)
        spoke = {}

        def post():
            spoke['end'] = call.post_question(audio) - TAIL_S
            return '{} pieces'.format(len(pieces(audio)))

        if not add(run_step(label + ': post the question', len(audio) / 2 / SAMPLE_RATE + 5, post)):
            return False, None, mark
        until = spoke['end']

        def cut():
            found = wait_until(
                lambda: phone.since(
                    mark, lambda s: s['state'] not in ('listening', 'connecting', 'off', '') or s['sound']
                    or (s['heard'] and s['heard'] != call.last_heard)),
                LIMIT_CUT, 'the turn is cut (#call leaves listening, or shows a sound or the words)', since=until)
            return 'state {}'.format(found[2]['state'])

        if not add(run_step(label + ': the turn is cut', LIMIT_CUT, cut, since=until)):
            return False, until, mark

        def heard():
            # #call keeps showing the last turn's words until the next turn replaces them.
            def shown():
                # The phone fetches and reports each sound meanwhile, such as "One moment.".
                for sound in call.sounds_since(mark):
                    call.serve(sound)
                return phone.since(mark, lambda s: s['heard'] and s['heard'] != call.last_heard)

            found = wait_until(shown, LIMIT_HEARD, '"You said" line', since=until)
            said = found[2]['heard']
            call.last_heard = said
            share = word_match(said, text)
            if not heard_enough(said, text):
                raise StepFailed('"You said" holds {:.0%} of the question, under {:.0%}: "{}"'.format(
                    share, WORD_SHARE, said))
            return '{:.0%} of the words'.format(share)

        ok = add(run_step(label + ': "You said" holds the question', LIMIT_HEARD, heard, since=until))
        return ok, until, mark

    # 2. The question the call answerer answers from the todo, with nothing typed.
    ok, until, leg_mark = leg('todos', QUESTION, question)
    if not ok:
        return results
    answer_marks = {}

    def todos_answer():
        shown = call.wait_for_line(
            lambda line: line != ONE_MOMENT and bool(re.search(r'\b(1|one)\b', line.lower())), LIMIT_ANSWER,
            "the call answerer's answer naming one todo", until, call.mark)
        answer_marks['todos'] = shown['sound']
        log = run.log()
        if 'spoken prompt typed' in log or 'hand-over typed' in log:
            raise StepFailed('a line was typed into a pane for a question the facts answer')
        return '"{}"'.format(shown['line'])

    if not add(run_step("the call answerer answers 'how many todos'", LIMIT_ANSWER, todos_answer, since=until)):
        return results
    results.extend(turn_times('todos', phone, leg_mark, until, lambda line: line != ONE_MOMENT and bool(re.search(r'\b(1|one)\b', line.lower()))))
    with phone.changed:
        one_moments = one_moment_sounds(phone.events)
    results.append(StepResult('"One moment." in #call', True, 0,
                              '{} sound{}'.format(one_moments, '' if one_moments == 1 else 's') if one_moments else 'none'))
    wav = call.wavs.get(answer_marks['todos'])
    if not add(run_step('the answerer\'s answer is a WAV', LIMIT_FETCH,
                        lambda: wav_check(wav))):
        return results

    # 3. A request for work: the call answerer says it was passed on, and Harold types the
    # hand-over into the stand-in chief's pane, which answers with `harold speak`.
    time.sleep(1)
    ok, until, leg_mark = leg('hand-over', REQUEST, request)
    if not ok:
        return results

    def passed_on():
        shown = call.wait_for_line(lambda line: 'chief' in line.lower() and line != FROM_THE_CHIEF,
                                   LIMIT_ANSWER, 'the answerer says it passed the request on', until, leg_mark)
        return '"{}"'.format(shown['line'])

    if not add(run_step("the answerer says it passed the request on", LIMIT_ANSWER, passed_on, since=until)):
        return results
    results.extend(turn_times(
        'hand-over', phone, leg_mark, until,
        lambda line: 'chief' in line.lower() and line != FROM_THE_CHIEF))

    def handed_over():
        wait_until(lambda: 'hand-over typed' in run.log(), LIMIT_TYPED,
                   'the log line "hand-over typed"', since=until)
        wait_until(lambda: 'hand-over H1' in run.pane_text(), LIMIT_TYPED,
                   "the 🎙 hand-over H1 line in the stand-in's pane", since=until)
        return 'pane {}'.format(run.pane)

    if not add(run_step("the hand-over is typed into the stand-in's pane", LIMIT_TYPED, handed_over, since=until)):
        return results

    # 4. The chief's report, after "From the chief.".
    def chief_line():
        call.wait_for_line(lambda line: line == ANSWER_LINE, LIMIT_SOUND, "the chief's answer in #call", until, leg_mark)
        lines = [shown['line'] for shown in call.sounds_since(leg_mark) if shown['line']]
        if FROM_THE_CHIEF not in lines or lines.index(FROM_THE_CHIEF) > lines.index(ANSWER_LINE):
            raise StepFailed('"From the chief." did not come before the answer: {}'.format(lines))
        return 'after "From the chief."'

    if not add(run_step("the chief's answer plays after 'From the chief.'", LIMIT_SOUND, chief_line, since=until)):
        return results

    # 5. Hang up.
    def hang_up():
        status, _, _ = phone.request('DELETE', '/call')
        if status != 204:
            raise StepFailed('DELETE /call answered {}'.format(status))
        wait_until(lambda: call_ended(run.log(), 'stopped'), LIMIT_HANGUP,
                   'the log line call ended reason="stopped"')
        return 'call ended, reason stopped'

    add(run_step('DELETE /call, call ended', LIMIT_HANGUP, hang_up))
    ready = worker_ready_line(run.log())
    if ready:
        results.append(StepResult('the log says how the voice worker loaded', True, 0, ready))
    return results


def call_signals(events):
    """The signals a Datastar event patches, from its `data: signals` line."""
    for line in events.splitlines():
        if line.startswith('data: signals '):
            try:
                signals = json.loads(line[len('data: signals '):])
            except ValueError:
                return {}
            return signals if isinstance(signals, dict) else {}
    return {}


def no_post_refused(log):
    """Fails when the test Harold's log has an `app post refused` line, naming each route."""
    routes = []
    for line in clean_log(log).splitlines():
        if 'app post refused' in line:
            found = re.search(r'route="([^"]*)"', line)
            routes.append(found.group(1) if found else line.strip())
    if routes:
        raise StepFailed('the test Harold refused posts to: {}'.format(', '.join(sorted(set(routes)))))
    return 'none refused'


def worker_ready_line(log):
    """The detail of the last `voice worker ready` line: its load time and who started it."""
    ready = [line for line in clean_log(log).splitlines() if 'voice worker ready' in line]
    if not ready:
        return ''
    load = re.search(r'load_ms=(\d+)', ready[-1])
    by = re.search(r'started_by="?(\w+)', ready[-1])
    return 'load_ms={} started_by={}'.format(load.group(1) if load else '?', by.group(1) if by else '?')


def turn_times(label, phone, mark, until, is_answer_line):
    """`first_sound_ms` and `answer_ms` of the turn whose speech ended at `until` (monotonic
    seconds), as the phone saw them: the first sound with words (a phrase such as "One
    moment." on main counts; a tick, the ringback or the pick-up has none), and the first sound whose words
    `is_answer_line` accepts. Both count from the end of the speech; Harold cuts the turn
    about 1.2 s later."""
    def first(test):
        with phone.changed:
            for at, shown in phone.events[mark:]:
                if at >= until and shown['sound'] and shown['line'] and test(shown['line']):
                    return at, shown['line']
        return None

    results = []
    for name, found in (('first_sound_ms', first(lambda line: True)), ('answer_ms', first(is_answer_line))):
        if found is None:
            results.append(StepResult('{}: {}'.format(label, name), False, 0, 'no such sound'))
        else:
            results.append(StepResult('{}: {}'.format(label, name), True, int((found[0] - until) * 1000), '"{}"'.format(found[1])))
    return results


def wav_check(wav):
    if wav is None:
        raise StepFailed('the answer was not fetched')
    kind, body = wav
    if not kind.startswith('audio/wav') or body[:4] != b'RIFF' or body[8:12] != b'WAVE':
        raise StepFailed('GET /call/answer answered {} ({} bytes)'.format(kind, len(body)))
    return '{} bytes'.format(len(body))


def isolation(run):
    """Checks the test Harold's log: no iMessage listener, no away channel, and nothing typed
    outside the stand-in's pane."""
    log = run.log()
    problems = []
    problems.extend(line.strip() for line in log.splitlines() if 'iMessage listener started' in line)
    outside = [pane for pane in typed_panes(log) + hand_over_panes(log) if pane != run.pane]
    if outside:
        problems.append('typed into {}'.format(', '.join(outside)))
    if problems:
        raise StepFailed('the test Harold reached beyond its scratch pane: ' + ' | '.join(problems[:3]))
    return 'typed only into {}'.format(run.pane)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
    parser.add_argument('--log-level', help="the test Harold's log level, such as debug")
    parser.add_argument('--install-dir', type=Path, default=INSTALL_DIR,
                        help="where the installed Harold's config is read from")
    parser.add_argument('--harold', type=Path,
                        help='the Harold to test (default: harold in --install-dir)')
    parser.add_argument('--default-config', type=Path,
                        help="the default.toml that goes with --harold (default: the installed one)")
    parser.add_argument('--wait-for-page-load', action='store_true',
                        help='wait for the voice worker to load for the open page before the call')
    args = parser.parse_args(argv)

    for tool in ('tmux', 'say', 'claude'):
        if not shutil.which(tool):
            print('FAIL {} is not on PATH'.format(tool))
            return 1
    binary = (args.harold or args.install_dir / 'harold').resolve()
    if not binary.exists():
        print('FAIL no Harold at {}'.format(binary))
        return 1

    run = Run(args.install_dir, args.keep, args.log_level, binary=binary,
              default_toml=args.default_config and args.default_config.resolve(), wait_for_page_load=args.wait_for_page_load)
    # A signal ends the run through the teardown below, which leaves nothing running.
    for name in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(name, lambda number, frame: sys.exit(128 + number))
    results = []
    try:
        run.write_config()
        socket_path = run.scratch / 't' / 'tmux-{}'.format(os.getuid()) / 'default'
        if len(str(socket_path)) > 100:
            results.append(StepResult('scratch tmux socket', False, 0, 'path too long: ' + str(socket_path)))
        else:
            ready = run_step('the test Harold starts', LIMIT_READY, run.start_harold)
            results.append(ready)
            if ready.ok:
                chief = run_step('the stand-in chief is ready', LIMIT_READY,
                                 lambda: (run.start_chief(), run.wait_for_chief_idle()) and '')
                results.append(chief)
                if chief.ok:
                    seeded = run_step('a todo is seeded in the scratch store', 30, run.seed_todo)
                    results.append(seeded)
                    if seeded.ok:
                        results.extend(play(run))
                        results.append(run_step('no post refused', 5, lambda: no_post_refused(run.log())))
                    results.append(run_step('log shows nothing beyond the scratch pane', 5,
                                            lambda: isolation(run)))
    except Exception as error:  # a crash is a failed run, and the teardown still happens
        results.append(StepResult('run', False, 0, '{}: {}'.format(type(error).__name__, error)))
    finally:
        run.tear_down()
    lines, code = report(results)
    print('\n'.join(lines))
    if code and run.pane_snapshot:
        print("the stand-in's pane at the failure:")
        print(run.pane_snapshot)
    if code and run.phone is not None:
        print('what #call showed:')
        for at, shown in run.phone.events:
            print('  {:>8.1f} s  {}'.format(at - run.phone.events[0][0], shown))
    run.finish(code == 0)
    return code


if __name__ == '__main__':
    sys.exit(main())
