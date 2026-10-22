#!/usr/bin/env python3
"""Plays a phone call end to end against a stand-in chief and times each step.

It starts a test Harold on a scratch config and a scratch tmux server, runs Claude Code in a
tmux session `harold-call-test` as the chief, and then does what the phone does: it follows
GET /updates, posts a spoken question to /call/audio, fetches the answer, reports it played
and hangs up. The real chief's pane, the installed Harold's calls and the user's thread are
never touched. Python 3 standard library only.
"""

import argparse
import html
import http.client
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
import wave

SESSION = 'harold-call-test'
LOGIN = 'call-test'
QUESTION = 'What is the status of the build today'
NO_RECIPIENT = 'call-test@harold.invalid'
ANSWER_LINE = 'Call test answer.'
SAMPLE_RATE = 16000
PIECE_SAMPLES = 4000
LEAD_S = 1
TAIL_S = 2
WORD_SHARE = 0.8
INSTALL_DIR = Path.home() / 'bin' / 'harold'

# The time limit of each step, in seconds. The ones after the question is posted count from
# when the question ended.
LIMIT_CALL = 20
LIMIT_CUT = 8
LIMIT_HEARD = 30
LIMIT_TYPED = 45
LIMIT_SOUND = 60
LIMIT_FETCH = 5
LIMIT_HANGUP = 10
LIMIT_READY = 90


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
        '[web_app]\nlisten = "127.0.0.1:{}"\nallowed_login = "{}"\n'.format(web_port, LOGIN),
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


def speak_question(path):
    """Makes the question as 16 kHz mono i16 samples with `say`."""
    subprocess.run(
        ['say', '-o', str(path), '--data-format=LEI16@16000', QUESTION], check=True, timeout=60)
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


def call_ended(log, reason):
    return re.search(r'call ended.*?reason="?{}\b'.format(re.escape(reason)), clean_log(log))


# --- what #call shows -----------------------------------------------------------------

CALL_SECTION = re.compile(r'<section id="call"[^>]*>', re.S)


def is_answer(shown):
    """Whether `shown` is the chief's answer sound, not a tick or one of Harold's phrases."""
    return bool(shown['sound']) and shown['line'] == ANSWER_LINE


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
            headers['Content-Type'] = content_type
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

    def __init__(self, install_dir, keep, log_level=None):
        self.install_dir = install_dir
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
        shutil.copy(self.install_dir / 'config' / 'default.toml', self.config_dir / 'default.toml')
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
            [str(self.install_dir / 'harold')], env=harold_env, stdout=self.log_file,
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
        speak = '{} speak --src-id {}'.format(self.install_dir / 'harold', self.pane)
        (self.chief_dir / 'CLAUDE.md').write_text(
            'Each line that starts with 🎙 is a spoken question. Answer it by running exactly\n'
            '`{} "{}"` and do nothing else: no other command, no other text.\n'
            'Answer any other line with the one word: ok.\n'.format(speak, ANSWER_LINE))
        # No hooks: the user's own Stop hook reports to the installed Harold's address, not to
        # HAROLD_CONFIG_DIR's, so a turn here would reach the real Harold as the pane %0.
        (self.chief_dir / '.claude' / 'settings.json').write_text(
            '{{"disableAllHooks": true, "permissions": {{"allow": ["Bash({}:*)"]}}}}\n'.format(speak))
        # One first turn, so the input box has no suggestion in it, which Harold reads as text.
        tmux(self.env, 'respawn-pane', '-k', '-t', self.pane, 'claude --model haiku "Hello"')

    def pane_text(self):
        return tmux(self.env, 'capture-pane', '-p', '-t', self.pane, check=False).stdout

    def wait_for_chief_idle(self):
        """Waits for Claude Code's empty input box, trusting its scratch folder when it asks."""
        def idle():
            text = self.pane_text()
            # The first choice of the folder question is "No, exit": pick the second.
            if 'Yes, I trust this folder' in text:
                tmux(self.env, 'send-keys', '-t', self.pane, 'Down', check=False)
                time.sleep(0.3)
                tmux(self.env, 'send-keys', '-t', self.pane, 'Enter', check=False)
                return False
            return '⏺' in text and '❯' in text and 'esc to interrupt' not in text

        wait_until(idle, LIMIT_READY, 'the stand-in chief shows its input box', every=0.5)
        # Harold reads the panes every 1.2 s.
        time.sleep(3)

    def log(self):
        try:
            return clean_log(self.log_path.read_text(errors='replace'))
        except OSError:
            return ''

    # teardown

    def tear_down(self):
        try:
            if self.pane is not None:
                self.pane_snapshot = self.pane_text().rstrip()
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
        if passed and not self.keep:
            shutil.rmtree(self.scratch, ignore_errors=True)
            return
        print('scratch folder kept: {} (the test Harold\'s log is harold.log)'.format(self.scratch))


def play(run):
    """The call itself: every step, in order. Answers the results."""
    results = []
    phone = run.phone

    def add(result):
        results.append(result)
        return result.ok

    question = speak_question(run.scratch / 'q.wav')
    audio = phone_audio(question)
    phone.follow_updates()
    wait_until(lambda: phone.events, 10, 'the call screen on /updates')
    mark = len(phone.events)

    # 1. POST /call, retried while the test Harold has not yet seen the stand-in chief.
    call_id = []

    def post_call():
        started = time.monotonic()
        while True:
            status, _, body = phone.request('POST', '/call')
            if status == 200:
                call_id.append(body.decode().strip())
                return 'call {}'.format(call_id[0][:8])
            if status != 409 or time.monotonic() - started > LIMIT_CALL:
                raise StepFailed('POST /call answered {} {}'.format(status, body.decode(errors='replace')))
            time.sleep(1)

    call_sent = time.monotonic()
    if not add(run_step('POST /call answers 200', LIMIT_CALL, post_call)):
        return results

    # 2. The question, in pieces stamped from the moment POST /call was sent, in real time.
    posted_at = {}

    def post_question():
        first = None
        for at_ms, piece in pieces(audio):
            # The phone sends a piece once it has recorded it.
            due = call_sent + at_ms / 1000 + PIECE_SAMPLES / SAMPLE_RATE
            time.sleep(max(0.0, due - time.monotonic()))
            if first is None:
                first = time.monotonic()
            status, _, _ = phone.request(
                'POST', '/call/audio?call={}&at={}'.format(call_id[0], at_ms), piece,
                'application/octet-stream')
            if status != 204:
                raise StepFailed('POST /call/audio answered {}'.format(status))
        posted_at['end'] = call_sent + (LEAD_S + len(question) / 2 / SAMPLE_RATE)
        return '{} pieces'.format(len(pieces(audio)))

    audio_s = len(audio) / 2 / SAMPLE_RATE
    if not add(run_step('post the question', audio_s + 5, post_question, since=call_sent)):
        return results
    spoke_until = posted_at['end']

    # 3. The turn is cut.
    def cut():
        found = wait_until(
            lambda: phone.since(mark, lambda s: s['state'] not in ('listening', 'off', '')),
            LIMIT_CUT, 'the turn is cut (#call leaves listening)', since=spoke_until)
        return 'state {}'.format(found[2]['state'])

    if not add(run_step('the turn is cut', LIMIT_CUT, cut, since=spoke_until)):
        return results

    # 4. "You said".
    def heard():
        found = wait_until(
            lambda: phone.since(mark, lambda s: s['heard']),
            LIMIT_HEARD, '"You said" line', since=spoke_until)
        said = found[2]['heard']
        share = word_match(said, QUESTION)
        if not heard_enough(said, QUESTION):
            raise StepFailed('"You said" holds {:.0%} of the question, under {:.0%}: "{}"'.format(
                share, WORD_SHARE, said))
        return '{:.0%} of the words'.format(share)

    if not add(run_step('"You said" holds the question', LIMIT_HEARD, heard, since=spoke_until)):
        return results

    # 5. The line typed into the stand-in's pane.
    def typed():
        wait_until(lambda: 'spoken prompt typed' in run.log(), LIMIT_TYPED,
                   'the log line "spoken prompt typed"', since=spoke_until)
        wait_until(lambda: '🎙' in run.pane_text(), LIMIT_TYPED,
                   'the 🎙 line in the stand-in\'s pane', since=spoke_until)
        return 'pane {}'.format(run.pane)

    if not add(run_step('typed into the stand-in\'s pane', LIMIT_TYPED, typed, since=spoke_until)):
        return results

    # 6. The answer's sound. Harold also offers a tick every 3 s while the chief thinks; like the
    # phone, the script fetches and reports each of those, and waits for the one that carries
    # the chief's line.
    ticks = set()

    def answered():
        with phone.changed:
            offered = [shown for _, shown in phone.events[mark:] if shown['sound']]
        for shown in offered:
            if is_answer(shown):
                return shown
            if shown['sound'] not in ticks:
                ticks.add(shown['sound'])
                try:
                    phone.request('GET', '/call/answer/' + shown['sound'])
                    phone.request('POST', '/call/played?id={}&start_ms=0'.format(shown['sound']))
                except (OSError, http.client.HTTPException):
                    pass
        return None

    def sound():
        found = wait_until(answered, LIMIT_SOUND, "the chief's answer in #call", since=spoke_until)
        answer_id.append(found['sound'])
        return 'sound {}'.format(found['sound'][:8])

    answer_id = []
    if not add(run_step("#call names the chief's answer", LIMIT_SOUND, sound, since=spoke_until)):
        return results
    sound_id = answer_id[0]

    # 7. The answer's audio.
    def answer():
        status, kind, body = phone.request('GET', '/call/answer/' + sound_id)
        if status != 200 or not kind.startswith('audio/wav') or body[:4] != b'RIFF' or body[8:12] != b'WAVE':
            raise StepFailed('GET /call/answer answered {} {} ({} bytes)'.format(status, kind, len(body)))
        scratch_wav = run.scratch / 'answer.wav'
        scratch_wav.write_bytes(body)
        with wave.open(str(scratch_wav), 'rb') as wav:
            return 'WAV of {:.2f} s'.format(wav.getnframes() / wav.getframerate())

    if not add(run_step('GET /call/answer returns WAV', LIMIT_FETCH, answer)):
        return results

    # 8. Played, then hang up.
    def hang_up():
        status, _, _ = phone.request('POST', '/call/played?id={}&start_ms=0'.format(sound_id))
        if status != 204:
            raise StepFailed('POST /call/played answered {}'.format(status))
        status, _, _ = phone.request('DELETE', '/call')
        if status != 204:
            raise StepFailed('DELETE /call answered {}'.format(status))
        wait_until(lambda: call_ended(run.log(), 'stopped'), LIMIT_HANGUP,
                   'the log line call ended reason="stopped"')
        return 'call ended, reason stopped'

    add(run_step('played, DELETE /call, call ended', LIMIT_HANGUP, hang_up))
    return results


def isolation(run):
    """Checks the test Harold's log: no iMessage listener, no away channel, and nothing typed
    outside the stand-in's pane."""
    log = run.log()
    problems = []
    problems.extend(line.strip() for line in log.splitlines() if 'iMessage listener started' in line)
    outside = [pane for pane in typed_panes(log) if pane != run.pane]
    if outside:
        problems.append('typed into {}'.format(', '.join(outside)))
    if problems:
        raise StepFailed('the test Harold reached beyond its scratch pane: ' + ' | '.join(problems[:3]))
    return 'typed only into {}'.format(run.pane)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
    parser.add_argument('--log-level', help="the test Harold's log level, such as debug")
    parser.add_argument('--install-dir', type=Path, default=INSTALL_DIR)
    args = parser.parse_args(argv)

    for tool in ('tmux', 'say', 'claude'):
        if not shutil.which(tool):
            print('FAIL {} is not on PATH'.format(tool))
            return 1
    if not (args.install_dir / 'harold').exists():
        print('FAIL no Harold at {}'.format(args.install_dir / 'harold'))
        return 1

    run = Run(args.install_dir, args.keep, args.log_level)
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
                    results.extend(play(run))
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
