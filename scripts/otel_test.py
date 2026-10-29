#!/usr/bin/env python3
"""Follows one reply, and one todo answer, from a tap in a headless Chrome to the answer drawn,
and prints each as a trace tree with times.

It reuses scripts/call_test.py's set-up: a test Harold on a scratch config (a temporary
HAROLD_CONFIG_DIR, TMPDIR and TMUX_TMPDIR, free ports, no iMessage listener or sender) and a
stand-in chief, Claude Code in a tmux session of its own. Besides that it starts:

- a collector stand-in (scripts/otel_collector.mjs) on scratch ports, which takes the test
  Harold's spans over OTLP gRPC and the browser's spans over OTLP HTTP, as Harold forwards them;
- a headless Chrome at 390 px (scripts/otel_browser.mjs), which opens the test Harold's web app,
  types a reply and taps Send, checks that Shift+Enter adds a line and Enter sends, then follows a todo to its Send. It adds the login header to
  every request, as Tailscale Serve does for the phone.

The stand-in chief answers each line it is typed by running `harold send-message-to-away-device`,
which is how the chief sends a message mid-turn; the script then reports the turn's end to the
test Harold with grpcurl, as the Stop hook does. The real chief's pane, the installed Harold, the
user's thread and any real tmux session are never touched, and everything started is stopped.
Needs node, tmux, claude, grpcurl and Google Chrome. Python 3 standard library only.
"""

import argparse
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import threading
import time

import call_test as ct

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
ANSWER = 'Chief answer.'
LIMIT_SCENARIO = 240


class OtelRun(ct.Run):
    """The call test's run, with export on to the collector stand-in and a chief that answers
    through the away-message command."""

    def __init__(self, binary, default_toml, keep, ports):
        super().__init__(ct.INSTALL_DIR, keep, binary=binary, default_toml=default_toml)
        self.ports = ports
        self.sampler = None

    def write_config(self):
        for folder in (self.config_dir, self.chief_dir / '.claude', self.scratch / 't'):
            folder.mkdir(parents=True)
        shutil.copy(self.default_toml, self.config_dir / 'default.toml')
        self.grpc_port = ct.free_port()
        self.web_port = ct.free_port()
        config = ct.scratch_config(
            '',
            chief_dir=str(self.chief_dir),
            grpc_port=self.grpc_port,
            web_port=self.web_port,
            store_path=str(self.scratch / 'store'),
            chat_db_path=str(self.scratch / 'no-messages.db'),
        )
        config = config.replace(
            '[telemetry]\nexport = false\n',
            '[telemetry]\nexport = true\notlp_endpoint = "http://127.0.0.1:{}"\n'
            'otlp_http_endpoint = "http://127.0.0.1:{}"\n'.format(self.ports['grpc'], self.ports['http']))
        (self.config_dir / 'local.toml').write_text(config)

    def start_chief(self):
        created = ct.tmux(
            self.env, 'new-session', '-d', '-s', ct.SESSION, '-c', str(self.chief_dir),
            '-x', '160', '-y', '50', '-P', '-F', '#{pane_id}', 'cat')
        self.pane = created.stdout.strip()
        # The stand-in reaches Harold through a link in its scratch folder, so no path of the
        # worktree is in its files.
        (self.scratch / 'bin').mkdir()
        link = self.scratch / 'bin' / 'harold'
        link.symlink_to(self.binary)
        say = '{} send-message-to-away-device --src-id {} "{}"'.format(link, self.pane, ANSWER)
        (self.chief_dir / 'CLAUDE.md').write_text(
            'Two kinds of line need an answer, and the answer is always the same command.\n'
            '1. A message whose first line starts with 📱 is one reply from the phone, with all its lines.\n'
            '   Run the command once for the whole message.\n'
            '2. A line that starts with the word ANSWERS holds answers to todos. It is not a question for you:\n'
            '   it only needs the acknowledgement.\n'
            'For either kind, run exactly `{}` and do nothing else: no other command, no other text.\n'
            'Answer any other line with the one word: ok.\n'.format(say))
        # No hooks: the user's own Stop hook reports to the installed Harold.
        (self.chief_dir / '.claude' / 'settings.json').write_text(
            '{{"disableAllHooks": true, "promptSuggestionEnabled": false, '
            '"permissions": {{"defaultMode": "dontAsk", "allow": ["Bash({}:*)"]}}}}\n'.format(
                '{} send-message-to-away-device'.format(link)))
        self.run_stand_in()
        self.sampler = LoadSampler(self.env)
        self.sampler.start()

    def run_stand_in(self):
        """As Run.run_stand_in, but Claude Code has the shell tool only, in dontAsk mode: it
        refuses each tool use that no rule allows."""
        ct.tmux(self.env, 'set-option', '-w', '-t', self.pane, 'remain-on-exit', 'on')
        self.stand_in_started = time.monotonic()
        ct.tmux(self.env, 'respawn-pane', '-k', '-t', self.pane, os.environ.get('CALL_TEST_STAND_IN')
                or 'claude --model haiku --permission-mode dontAsk --tools=Bash "Hello"')

    def chief_idle(self):
        text = self.pane_text()
        return '⏺' in text and '❯' in text and 'esc to interrupt' not in text

    def report_turn_end(self, prompt):
        """What the Stop hook sends: the turn's end, over gRPC, for the stand-in's pane."""
        payload = json.dumps({
            'pane_id': self.pane,
            'pane_label': ct.SESSION + ':0.0',
            'last_user_prompt': prompt,
            'assistant_message': ANSWER,
            'main_context': '',
        })
        done = subprocess.run(
            ['grpcurl', '-plaintext', '-import-path', str(REPO / 'harold-api' / 'proto'),
             '-proto', 'harold.proto', '-d', payload,
             '127.0.0.1:{}'.format(self.grpc_port), 'harold.Harold/TurnComplete'],
            capture_output=True, text=True, timeout=20)
        if done.returncode != 0:
            raise ct.StepFailed('TurnComplete failed: ' + (done.stdout + done.stderr).strip()[:300])


LIST_PANES_FORMAT = '#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_pid}\t#{pane_tty}\t#{pane_current_path}'
SAMPLE_EVERY = 5
REPORT_SAMPLES = 300
REPORT_LOG = 300
HAROLD_TROUBLE = ('agent monitor degraded', 'agent monitor recovered', 'event handler cycle failed')


class LoadSampler(threading.Thread):
    """From the stand-in's start until the tear-down, every 5 s: how long Harold's pane listing
    takes on the scratch tmux server, and the 1-minute load average."""

    def __init__(self, env):
        super().__init__(daemon=True)
        self.env = env
        self.done = threading.Event()
        self.samples = []

    def run(self):
        while not self.done.is_set():
            started = time.monotonic()
            try:
                subprocess.run(
                    ['tmux', '-f', '/dev/null', '-u', 'list-panes', '-a', '-F', LIST_PANES_FORMAT],
                    env=self.env, capture_output=True, timeout=10)
                listing = '{} ms'.format(int((time.monotonic() - started) * 1000))
            except subprocess.TimeoutExpired:
                listing = 'over 10000 ms'
            except OSError as error:
                listing = 'failed: {}'.format(error)
            self.samples.append((datetime.now(timezone.utc), listing, load_average()))
            self.done.wait(SAMPLE_EVERY)

    def stop(self):
        self.done.set()
        if self.is_alive():
            self.join(timeout=12)


def load_average():
    try:
        done = subprocess.run(['sysctl', '-n', 'vm.loadavg'], capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        return 'unknown'
    fields = done.stdout.strip('{} \n').split()
    return fields[0] if fields else 'unknown'


def load_report(run):
    """What a stop needs to tell the machine from the test Harold: the sampler's last 300 s, the
    test Harold's monitor lines of the last 300 s, and the busiest processes now."""
    now = datetime.now(timezone.utc)
    lines = ['load report, {}'.format(now.strftime('%H:%M:%SZ'))]
    samples = list(run.sampler.samples) if run.sampler is not None else []
    recent = [sample for sample in samples if now - sample[0] <= timedelta(seconds=REPORT_SAMPLES)]
    lines.append('samples of the last {} s (time, tmux list-panes, load average):'.format(REPORT_SAMPLES))
    lines.extend('  {}  {}  {}'.format(at.strftime('%H:%M:%SZ'), listing, load) for at, listing, load in recent)
    if not recent:
        lines.append('  none')
    lines.append("the test Harold's monitor lines of the last {} s:".format(REPORT_LOG))
    shown = 0
    for line in run.log().splitlines():
        if not any(text in line for text in HAROLD_TROUBLE):
            continue
        try:
            at = datetime.fromisoformat(line.split(' ', 1)[0].replace('Z', '+00:00'))
        except ValueError:
            continue
        if now - at > timedelta(seconds=REPORT_LOG):
            continue
        shown += 1
        lines.append('  {}  {}'.format(at.strftime('%H:%M:%SZ'), line.split(' ', 1)[1].strip()))
    if not shown:
        lines.append('  none')
    lines.append('the 5 processes with the most CPU now (CPU %, name):')
    try:
        listed = subprocess.run(['ps', '-Ao', 'pcpu,comm', '-r'], capture_output=True, text=True, timeout=10).stdout
        for row in listed.splitlines()[1:6]:
            cpu, _, name = row.strip().partition(' ')
            lines.append('  {}  {}'.format(cpu, Path(name.strip()).name))
    except (OSError, subprocess.SubprocessError):
        lines.append('  unknown')
    return '\n'.join(lines)


def write_load_report(run):
    text = load_report(run)
    try:
        (run.scratch / 'load.log').write_text(text + '\n')
    except OSError:
        pass
    return text


def start_collector(scratch, ports):
    out = scratch / 'spans.jsonl'
    out.touch()
    process = subprocess.Popen(
        ['node', str(HERE / 'otel_collector.mjs'), '--grpc', str(ports['grpc']),
         '--http', str(ports['http']), '--out', str(out)],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
    time.sleep(1)
    if process.poll() is not None:
        raise ct.StepFailed('the collector stand-in exited: ' + process.stderr.read().decode()[:300])
    return process, out


def spans_in(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def run_scenario(run, scenario, text, spans_path, screens):
    """Runs the browser through `scenario`, reports the stand-in chief's turn end once it has
    answered, and returns the browser's own result."""
    result_path = run.scratch / '{}-browser.json'.format(scenario)
    sent = run.log().count('away message stored')
    browser = subprocess.Popen(
        ['node', str(HERE / 'otel_browser.mjs'), '--scenario', scenario,
         '--url', 'http://127.0.0.1:{}/'.format(run.web_port), '--login', ct.LOGIN,
         '--chrome', CHROME, '--out', str(result_path), '--text', text,
         '--screens', str(screens), '--limit', str(LIMIT_SCENARIO)],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
    marker = text if scenario in ('reply', 'keys') else 'ANSWERS'
    try:
        # The line reaches the stand-in's pane, the stand-in answers, and its box is empty again.
        ct.wait_until(lambda: marker in run.pane_text(), LIMIT_SCENARIO, 'the line is typed into the stand-in')
        ct.wait_until(lambda: run.log().count('away message stored') > sent, LIMIT_SCENARIO,
                      'the stand-in sends its message')
        # Its box is empty again once it has finished its turn.
        time.sleep(3)
        ct.wait_until(lambda: run.chief_idle(), LIMIT_SCENARIO, 'the stand-in finishes its turn', every=0.5)
        run.report_turn_end(marker)
        browser.wait(timeout=LIMIT_SCENARIO)
    finally:
        if browser.poll() is None:
            os.killpg(browser.pid, signal.SIGKILL)
    if not result_path.exists():
        raise ct.StepFailed('the browser wrote no result: ' + browser.stderr.read().decode()[:300])
    return json.loads(result_path.read_text())


def run_layout_scenario(run, scenario, screens, width=390):
    """Runs a browser scenario that sends no reply, so no chief turn follows, and returns the
    browser's own result."""
    result_path = run.scratch / '{}-browser.json'.format(scenario if width == 390 else '{}-{}'.format(scenario, width))
    browser = subprocess.Popen(
        ['node', str(HERE / 'otel_browser.mjs'), '--scenario', scenario,
         '--url', 'http://127.0.0.1:{}/'.format(run.web_port), '--login', ct.LOGIN,
         '--chrome', CHROME, '--out', str(result_path), '--screens', str(screens / 'w{}'.format(width)),
         '--limit', str(LIMIT_SCENARIO), '--width', str(width)],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
    try:
        browser.wait(timeout=LIMIT_SCENARIO)
    except subprocess.TimeoutExpired:
        pass
    finally:
        if browser.poll() is None:
            os.killpg(browser.pid, signal.SIGKILL)
    if not result_path.exists():
        raise ct.StepFailed('the browser wrote no result: ' + browser.stderr.read().decode()[:300])
    return json.loads(result_path.read_text())


def tree(spans, trace_id):
    """The spans of one trace as indented lines with their start and length in milliseconds
    from the first span's start."""
    chosen = [span for span in spans if span['traceId'] == trace_id]
    chosen.sort(key=lambda span: int(span['startNs']))
    origin = int(chosen[0]['startNs'])
    by_parent = {}
    ids = {span['spanId'] for span in chosen}
    for span in chosen:
        parent = span['parentSpanId'] if span['parentSpanId'] in ids else ''
        by_parent.setdefault(parent, []).append(span)
    lines = []

    def walk(parent, depth):
        for span in by_parent.get(parent, []):
            start = (int(span['startNs']) - origin) / 1e6
            length = (int(span['endNs']) - int(span['startNs'])) / 1e6
            detail = []
            for key in ('channel', 'outcome', 'started_by', 'kind', 'harold.row', 'url.full', 'pane_id'):
                if key in span['attributes']:
                    detail.append('{}={}'.format(key, span['attributes'][key]))
            lines.append('{}{:<32} {:>9.1f} ms +{:>8.1f} ms  [{}] {}'.format(
                '  ' * depth, span['name'], start, length, span['source'], ' '.join(detail)))
            walk(span['spanId'], depth + 1)

    walk('', 0)
    return lines


def trace_of(spans, name, url):
    for span in spans:
        if span['source'] == 'browser' and span['name'] in (name, 'HTTP ' + name) \
                and span['attributes'].get('url.full') == url:
            return span['traceId']
    return None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
    parser.add_argument('--harold', type=Path, default=REPO / 'target' / 'debug' / 'harold',
                        help='the Harold to test (an absolute path)')
    parser.add_argument('--default-config', type=Path, default=REPO / 'harold' / 'config' / 'default.toml')
    parser.add_argument('--screens', type=Path, help='where to keep the browser screenshots')
    args = parser.parse_args(argv)
    for tool in ('tmux', 'claude', 'node', 'grpcurl'):
        if not shutil.which(tool):
            print('FAIL {} is not on PATH'.format(tool))
            return 1
    if not Path(CHROME).exists():
        print('FAIL no Google Chrome at ' + CHROME)
        return 1
    binary = args.harold.resolve()
    if not binary.exists():
        print('FAIL no Harold at {}'.format(binary))
        return 1

    ports = {'grpc': ct.free_port(), 'http': ct.free_port()}
    run = OtelRun(binary, args.default_config.resolve(), args.keep, ports)
    for name in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(name, lambda number, frame: sys.exit(128 + number))
    collector = None
    failed = False
    try:
        run.scratch.mkdir(exist_ok=True)
        collector, spans_path = start_collector(run.scratch, ports)
        run.write_config()
        run.start_harold()
        run.start_chief()
        run.wait_for_chief_idle()
        print(run.seed_todo())
        screens = args.screens or run.scratch / 'screens'

        reply = run_scenario(run, 'reply', 'please rerun the build', spans_path, screens)
        keys = run_scenario(run, 'keys', 'line two', spans_path, screens)
        keyboard = run_layout_scenario(run, 'keyboard', screens)
        keyboard_narrow = run_layout_scenario(run, 'keyboard', screens, width=320)
        todo = run_scenario(run, 'todo', 'ANSWERS', spans_path, screens)
        time.sleep(8)  # Harold's exporter sends every 5 s
        spans = spans_in(spans_path)

        print('browser: reply {}'.format(json.dumps(reply)))
        print('browser: keys  {}'.format(json.dumps(keys)))
        print('browser: keyboard {}'.format(json.dumps(keyboard)))
        print('browser: todo  {}'.format(json.dumps(todo)))
        print('browser: keyboard at 320 px {}'.format(json.dumps(keyboard_narrow)))
        for shown in (keyboard, keyboard_narrow):
            if not shown.get('ok'):
                failed = True
                print('FAIL the keyboard scenario: {}'.format(shown.get('error')))
                continue
            heights = [shown[key]['height'] for key in ('box0', 'box1', 'box10', 'dictated')]
            print('box height with 0, 1, 10 typed and 10 dictated lines: {}; scrollHeight/clientHeight at 10 lines: {}/{}; bar bottom {}'.format(
                heights, shown['box10']['scrollHeight'], shown['box10']['clientHeight'], shown['box10']['replyBottom']))
            if len(set(heights)) != 1 or shown['box10']['scrollHeight'] <= shown['box10']['clientHeight'] \
                    or abs(shown['box10']['replyBottom'] - 508) > 1:
                failed = True
                print('FAIL the reply box is not 3 lines tall and scrolling inside')
            taps = [shown.get(key, {}).get('focusedAfter') for key in ('topBand', 'leftBand')]
            print('tap in the top band, in the left band focuses the text box: {}'.format(taps))
            if taps != [True, True]:
                failed = True
                print('FAIL a tap inside the reply box border does not focus the text box')
            edges = [(shown[key]['edgeTop'], shown[key]['edgeBottom']) for key in ('box0', 'box10', 'dictated')]
            print('edge space top/bottom with 0, 10 typed and 10 dictated lines: {}; lines from the end: {}, {}'.format(
                edges, shown['box10']['linesFromEnd'], shown['dictated']['linesFromEnd']))
            if any(abs(a - edges[0][0]) > 0.5 or abs(b - edges[0][1]) > 0.5 for a, b in edges) \
                    or any(shown[key]['fromEnd'] >= 1 for key in ('box10', 'dictated')):
                failed = True
                print('FAIL the reply box edge space changes or the last line is out of view')
        if not keys.get('ok'):
            failed = True
            print('FAIL the keys scenario: {}'.format(keys.get('error')))
        for label, name, url in (('one reply', 'POST', '/replies'), ('one todo answer', 'POST', '/todos/answer')):
            trace_id = trace_of(spans, name, url)
            print('\n== {} ==  trace {}'.format(label, trace_id))
            if trace_id is None:
                failed = True
                print('no browser span for {} {}'.format(name, url))
                continue
            print('\n'.join(tree(spans, trace_id)))
            names = {span['name'] for span in spans if span['traceId'] == trace_id}
            wanted = {'harold.phone_request', 'harold.turn', 'browser.message_shown'} if url == '/replies' \
                else {'harold.answers_typed', 'harold.turn', 'browser.message_shown'}
            missing = sorted(wanted - names)
            if missing:
                failed = True
                print('MISSING from the trace: ' + ', '.join(missing))

        browser_spans = [span for span in spans if span['source'] == 'browser']
        kinds = sorted({span['name'] for span in browser_spans})
        print('\nbrowser span names: ' + ', '.join(kinds))
        print('service names: ' + ', '.join(sorted({str(span.get('service')) for span in browser_spans})))
        query = [span for span in browser_spans if any('?' in str(v) for v in span['attributes'].values())]
        print('browser spans carrying a "?": {}'.format(len(query)))
        errors = [span for span in browser_spans if span['name'] == 'browser.error']
        print('browser.error spans: {}'.format(len(errors)))
        failed = failed or bool(query) or not {'documentLoad'} <= set(kinds)
    except ct.StepFailed as error:
        failed = True
        print('FAIL ' + str(error))
        print(run.stand_in_stop())
        if run.sampler is not None:
            print(write_load_report(run))
    finally:
        if run.sampler is not None:
            run.sampler.stop()
        run.tear_down()
        if collector is not None and collector.poll() is None:
            os.killpg(collector.pid, signal.SIGTERM)
    log = run.log()
    problems = [line for line in log.splitlines() if 'iMessage listener started' in line]
    outside = [pane for pane in ct.typed_panes(log) if pane != run.pane]
    if problems or outside:
        failed = True
        print('FAIL the test Harold reached beyond its scratch pane: {} {}'.format(problems[:1], outside[:3]))
    run.finish(not failed)
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
