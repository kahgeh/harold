#!/usr/bin/env python3
"""Checks dictation on every tap in a headless Chrome, on a test Harold.

It reuses scripts/call_test.py's set-up (a scratch config: a temporary HAROLD_CONFIG_DIR, HOME,
TMPDIR and TMUX_TMPDIR, free ports, a private tmux server, no iMessage listener) and scripts/
otel_test.py's browser set-up (a headless Chrome over the DevTools protocol, with the login header
that Tailscale Serve adds). The web app runs with require_pairing = false, because the test browser
is not a paired device. The test needs a Harold built with the default features (cargo build --locked).

The test puts a fake `webkitSpeechRecognition` in the page before the page's scripts run. The fake
models iOS Safari: it refuses a `start()` while another instance is active, and it refuses a
`start()` outside a user gesture. The test sets `inGesture` only while it dispatches the click on the
mic, so a start from an event or a timer is refused. The fake counts each refusal. The test records every body that the page posts to /dictation/check by wrapping `fetch`.
Each case loads the thread again, so the page and the fake start new.

Each check prints `PASS name` or `FAIL name: why`. The test stops at the first failure and the exit
code is 1. The installed Harold, ~/bin, the user's tmux server, Rex, Tailscale and the real config
are not touched. Needs node, tmux and Google Chrome. Python 3 standard library only.

Do not run this test on a Mac that holds the real Harold: it starts a Harold and opens ports. The
Send case dispatches a submit event on a blank reply box, and the call case dispatches a click on
the call button when it is not disabled; both are harmless on a test Harold.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys

import call_test as ct
import otel_test as ot

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
STOPPED = 'Dictation stopped. Tap the mic again.'
TAP_AGAIN = 'Tap the mic again.'

BROWSER = r'''
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const args = JSON.parse(process.argv[2]);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const profile = fs.mkdtempSync(path.join(os.tmpdir(), "dictation-chrome-"));
const port = args.debugPort;
const chrome = spawn(args.chrome, ["--headless=new", `--remote-debugging-port=${port}`,
  `--user-data-dir=${profile}`, "--no-first-run", "--no-default-browser-check", "--mute-audio",
  "--disable-extensions", "--window-size=390,844", "about:blank"], { stdio: "ignore" });
const result = { cases: {}, errors: [] };
async function finish(code) {
  fs.writeFileSync(args.out, JSON.stringify(result));
  chrome.kill("SIGTERM");
  await sleep(500);
  fs.rmSync(profile, { recursive: true, force: true });
  process.exit(code);
}
async function target() {
  for (let i = 0; i < 100; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = list.find((e) => e.type === "page");
      if (page) return page.webSocketDebuggerUrl;
    } catch {}
    await sleep(200);
  }
  throw new Error("Chrome did not start");
}
const socket = new WebSocket(await target());
await new Promise((res, rej) => { socket.onopen = res; socket.onerror = rej; });
let next = 0;
const waiting = new Map();
socket.onmessage = (event) => {
  const m = JSON.parse(event.data);
  if (m.id && waiting.has(m.id)) { waiting.get(m.id)(m); waiting.delete(m.id); }
};
const send = (method, params = {}) => new Promise((res) => {
  const id = ++next; waiting.set(id, res);
  socket.send(JSON.stringify({ id, method, params }));
});
const evaluate = async (expression) => {
  const r = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.result.exceptionDetails) throw new Error(r.result.exceptionDetails.text + " in " + expression);
  return r.result.result.value;
};
const until = async (what, check, ms = 5000) => {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) { if (await check()) return; await sleep(50); }
  throw new Error("timed out waiting for " + what);
};

// The page's side: a fake speech recognition and a record of the checks. It runs before the
// page's own scripts.
const FAKE = String.raw`(() => {
  // abortReleases: abort() makes the instance inactive at once (else only its end event does).
  const cfg = { throwOnNew: false, throwOnStart: false, noEnd: false, abortReleases: true };
  const F = { cfg, instances: [], current: null, refusals: 0, outsideGesture: 0, inGesture: false, reports: [] };
  window.__fake = F;
  class Fake {
    constructor() {
      if (cfg.throwOnNew) throw new Error("new refused");
      this.createdAt = performance.now();
      F.instances.push(this);
    }
    start() {
      if (cfg.throwOnStart) throw new DOMException("refused", "InvalidStateError");
      if (!F.inGesture) {
        F.outsideGesture += 1;
        throw new DOMException("start outside a user gesture", "NotAllowedError");
      }
      if (F.current && F.current !== this) {
        F.refusals += 1;
        throw new DOMException("another instance is active", "InvalidStateError");
      }
      F.current = this;
    }
    stop() { this.stopped = true; if (!cfg.noEnd) setTimeout(() => this.fireEnd(), 30); }
    abort() {
      this.aborted = true;
      if (cfg.abortReleases && F.current === this) F.current = null;
      if (cfg.noEnd) return;
      setTimeout(() => { if (this.onerror) this.onerror({ error: "aborted" }); this.fireEnd(); }, 30);
    }
    fireStart() { if (this.onstart) this.onstart({}); }
    fireResult(text) {
      const row = Object.assign([{ transcript: text }], { isFinal: true });
      if (this.onresult) this.onresult({ results: [row] });
    }
    fireError(name) { if (this.onerror) this.onerror({ error: name }); }
    fireEnd() { if (F.current === this) F.current = null; if (this.onend) this.onend({}); }
  }
  F.last = () => F.instances[F.instances.length - 1];
  window.webkitSpeechRecognition = Fake;
  const realFetch = window.fetch;
  window.fetch = function (url, options) {
    try {
      if (String(url).includes("/dictation/check")) F.reports.push(JSON.parse(options.body));
    } catch (error) { F.reports.push({ error: String(error) }); }
    return realFetch.apply(this, arguments);
  };
})()`;
await send("Page.addScriptToEvaluateOnNewDocument", { source: FAKE });

const load = async () => {
  await send("Page.navigate", { url: args.base + "/" });
  await sleep(300);
  await until("load", async () => (await evaluate("document.readyState")) === "complete");
  await until("the mic is on", async () => (await evaluate(`document.getElementById("dictation-listening") !== null && document.getElementById("dictation-on").value === "1"`)));
};
// The click runs inside the fake's gesture, and only there.
const tapMic = () => evaluate(`(() => { const f = window.__fake; f.inGesture = true;
  try { document.getElementById("dictation-mic").click(); } finally { f.inGesture = false; } })()`);
const count = () => evaluate("window.__fake.instances.length");
const waitInstances = (n) => until("instance " + n, async () => (await count()) >= n);
const fake = (code) => evaluate(`(() => { const f = window.__fake; ${code} })()`);
const state = () => evaluate(`(() => { const f = window.__fake; const n = document.getElementById("listening-note");
  return { reports: f.reports, instances: f.instances.length, refusals: f.refusals, outsideGesture: f.outsideGesture,
    box: document.querySelector("#reply textarea").value,
    note: { hidden: n.hidden, notice: n.classList.contains("notice"), text: document.getElementById("listening-text").textContent } }; })()`);
const reportsAre = async (n, ms = 3000) => { try { await until(n + " reports", async () => (await evaluate("window.__fake.reports.length")) >= n, ms); } catch {} };
// One tap that gets a result and ends by itself.
async function fullTap(n, text) {
  await tapMic();
  await waitInstances(n);
  await fake(`f.last().fireStart(); f.last().fireResult(${JSON.stringify(text)}); f.last().fireEnd();`);
}

try {
  await send("Runtime.enable"); await send("Network.enable"); await send("Page.enable");
  await send("Network.setExtraHTTPHeaders", { headers: { "Tailscale-User-Login": args.login } });
  await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });
  const cases = result.cases;

  // Two taps in a row, each with a result.
  await load();
  await fullTap(1, "first words");
  await reportsAre(1);
  await fullTap(2, "second words");
  await reportsAre(2);
  cases.two_taps = await state();

  // A tap right after an error, with no `end`: the page aborts the old instance and starts the
  // new one in the tap's own handler.
  await load();
  await tapMic(); await waitInstances(1);
  await fake(`f.last().fireStart(); f.last().fireError("network");`);
  await tapMic();
  await waitInstances(2);
  await fake(`f.last().fireStart(); f.last().fireResult("after the error"); f.last().fireEnd();`);
  await reportsAre(2);
  await sleep(200);
  cases.after_error_no_end = await state();

  // The same, but the abort does not release the old instance: the start is refused, the
  // notice shows, and the tap after the old `end` works.
  await load();
  await fake(`f.cfg.abortReleases = false;`);
  await tapMic(); await waitInstances(1);
  await fake(`f.last().fireStart(); f.last().fireError("network");`);
  await tapMic();
  await reportsAre(2);
  cases.refused_start = await state();
  await sleep(200);
  await tapMic();
  await waitInstances(3);
  await fake(`f.last().fireStart(); f.last().fireResult("third tap"); f.last().fireEnd();`);
  await reportsAre(3);
  cases.after_refused_start = await state();

  // A start() that throws, a new Recognition() that throws.
  await load();
  await fake(`f.cfg.throwOnStart = true;`);
  await tapMic();
  await reportsAre(1);
  cases.start_throws = await state();
  await load();
  await fake(`f.cfg.throwOnNew = true;`);
  await tapMic();
  await reportsAre(1);
  cases.new_throws = await state();

  // A browser aborted error, and an error with a name the page does not know.
  for (const [key, name] of [["aborted", "aborted"], ["other_error", "language-not-supported"]]) {
    await load();
    await tapMic(); await waitInstances(1);
    await fake(`f.last().fireStart(); f.last().fireError(${JSON.stringify(name)}); f.last().fireEnd();`);
    await reportsAre(1);
    cases[key] = await state();
  }

  // The page ends the recognition: Send, the page hiding, a call tap, a stop with no result.
  const pageEnds = {
    send: `document.getElementById("reply").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }))`,
    hide: `(() => { Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "hidden" }); document.dispatchEvent(new Event("visibilitychange")); })()`,
    call: `(() => { const c = document.getElementById("call-start"); if (!c || c.disabled) return "skip"; c.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true })); })()`,
    stop: `document.getElementById("dictation-stop").click()`,
  };
  for (const [key, code] of Object.entries(pageEnds)) {
    await load();
    await tapMic(); await waitInstances(1);
    await fake(`f.last().fireStart();`);
    const skipped = (await evaluate(code)) === "skip";
    if (!skipped) await reportsAre(1);
    await sleep(300);
    cases["page_ended_" + key] = { ...(await state()), skipped };
  }
  // A stop after a result gives ok.
  await load();
  await tapMic(); await waitInstances(1);
  await fake(`f.last().fireStart(); f.last().fireResult("then stop");`);
  await evaluate(`document.getElementById("dictation-stop").click()`);
  await reportsAre(1);
  await sleep(300);
  cases.stop_after_result = await state();

  // A stop with no `end`: a second tap on stop ends the tap at once.
  await load();
  await fake(`f.cfg.noEnd = true;`);
  await tapMic(); await waitInstances(1);
  await fake(`f.last().fireStart();`);
  await evaluate(`document.getElementById("dictation-stop").click()`);
  await sleep(200);
  const beforeSecondStop = await evaluate(`window.__fake.reports.length`);
  await evaluate(`document.getElementById("dictation-stop").click()`);
  await reportsAre(1);
  cases.stop_without_end = { ...(await state()), beforeSecondStop };

  // A stop with no `end` and no second stop: the page retires it after 1 second.
  await load();
  await fake(`f.cfg.noEnd = true;`);
  await tapMic(); await waitInstances(1);
  await fake(`f.last().fireStart();`);
  await evaluate(`document.getElementById("dictation-stop").click()`);
  await sleep(300);
  const beforeTimer = await evaluate(`window.__fake.reports.length`);
  await reportsAre(1, 3000);
  cases.stop_timer = { ...(await state()), beforeTimer };

  // The abort does not release the old instance and its `end` never comes: each later tap is
  // refused by the fake, shows the notice and reports start_failed. No second start succeeds.
  await load();
  await fake(`f.cfg.abortReleases = false; f.cfg.noEnd = true;`);
  await tapMic(); await waitInstances(1);
  await fake(`f.last().fireStart(); f.last().fireError("network");`);
  await tapMic();
  await reportsAre(2);
  cases.abort_never_ends_tap2 = await state();
  await tapMic();
  await reportsAre(3);
  cases.abort_never_ends_tap3 = {
    ...(await state()), firstStillActive: await evaluate(`window.__fake.current === window.__fake.instances[0]`) };

  // An end with no result and no error.
  await load();
  await tapMic(); await waitInstances(1);
  await fake(`f.last().fireStart(); f.last().fireEnd();`);
  await reportsAre(1);
  cases.no_result = await state();

  // The 101st tap sends no report.
  await load();
  for (let n = 1; n <= 101; n++) { await fullTap(n, "tap " + n); await sleep(40); }
  await sleep(600);
  cases.limit = await state();

  await finish(0);
} catch (e) {
  result.errors.push(String(e.stack || e));
  await finish(1);
}
'''


class Stop(Exception):
    """The first failed check ends the test."""


class Results:
    def __init__(self):
        self.failed = False

    def check(self, name, ok, why=''):
        if ok:
            print('PASS ' + name)
            return
        self.failed = True
        print('FAIL {}: {}'.format(name, why))
        raise Stop()


class DictationRun(ct.Run):
    """The call test's run with the web app only and no stand-in chief."""

    def write_config(self):
        for folder in (self.config_dir, self.chief_dir, self.scratch / 't', self.scratch / 'home'):
            folder.mkdir(parents=True, exist_ok=True)
        shutil.copy(self.default_toml, self.config_dir / 'default.toml')
        self.grpc_port = ct.free_port()
        self.web_port = ct.free_port()
        config = ct.scratch_config(
            '', chief_dir=str(self.chief_dir), grpc_port=self.grpc_port, web_port=self.web_port,
            store_path=str(self.scratch / 'store'), chat_db_path=str(self.scratch / 'no-messages.db'))
        (self.config_dir / 'local.toml').write_text(config)
        self.env['HOME'] = str(self.scratch / 'home')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
    parser.add_argument('--harold', type=Path, default=REPO / 'target' / 'debug' / 'harold')
    parser.add_argument('--default-config', type=Path, default=REPO / 'harold' / 'config' / 'default.toml')
    args = parser.parse_args(argv)
    for tool in ('tmux', 'node'):
        if not shutil.which(tool):
            print('FAIL {} is not on PATH'.format(tool))
            return 1
    if not Path(ot.CHROME).exists():
        print('FAIL no Google Chrome at ' + ot.CHROME)
        return 1
    binary = args.harold.resolve()
    if not binary.exists():
        print('FAIL no Harold at {} (run cargo build --locked)'.format(binary))
        return 1

    run = DictationRun(ct.INSTALL_DIR, args.keep, binary=binary, default_toml=args.default_config.resolve())
    for name in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(name, lambda number, frame: sys.exit(128 + number))
    results = Results()
    try:
        run.write_config()
        run.start_harold()
        out = run.scratch / 'browser.json'
        script = run.scratch / 'browser.mjs'
        script.write_text(BROWSER)
        config = json.dumps({
            'chrome': ot.CHROME, 'base': 'http://127.0.0.1:{}'.format(run.web_port), 'login': ct.LOGIN,
            'out': str(out), 'debugPort': ct.free_port()})
        node = subprocess.Popen(['node', str(script), config], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, start_new_session=True)
        try:
            stdout, stderr = node.communicate(timeout=300)
        except subprocess.TimeoutExpired:
            os.killpg(node.pid, signal.SIGKILL)
            stdout, stderr = node.communicate()
            stderr = 'the browser program timed out. ' + stderr
        if not out.exists():
            results.check('browser ran', False, (stderr or stdout)[:400])
        data = json.loads(out.read_text())
        if data['errors']:
            results.check('browser ran', False, data['errors'][0][:3000])
        verify(results, data['cases'])
    except Stop:
        pass
    except ct.StepFailed as error:
        print('FAIL test Harold starts: {}'.format(error))
        results.failed = True
    finally:
        run.tear_down()
    run.finish(not results.failed)
    return 1 if results.failed else 0


def verify(results, cases):
    check = results.check

    def rows(case):
        return [(r.get('result'), r.get('tap'), r.get('started')) for r in cases[case]['reports']]

    def only_signals(case):
        return all(sorted(r.keys()) == ['result', 'started', 'tap'] for r in cases[case]['reports'])

    c = cases['two_taps']
    check('two taps in a row give two ok reports, tap 1 and 2',
          rows('two_taps') == [('ok', 1, True), ('ok', 2, True)], c['reports'])
    check('the second tap writes its words into the box', c['box'] == 'second words', c['box'])
    check('the second tap met no refusal from the fake and no start outside a gesture',
          c['refusals'] == 0 and c['outsideGesture'] == 0, c)

    c = cases['after_error_no_end']
    check('a tap after an error with no end aborts the old instance and starts in its own handler',
          rows('after_error_no_end') == [('network', 1, True), ('ok', 2, True)] and c['refusals'] == 0
          and c['outsideGesture'] == 0 and c['instances'] == 2, c)

    c = cases['refused_start']
    check('a start that the old instance refuses gives start_failed for that tap and the notice',
          rows('refused_start') == [('network', 1, True), ('start_failed', 2, False)] and c['refusals'] == 1
          and not c['note']['hidden'] and c['note']['text'] == 'Tap the mic again.', c)
    c = cases['after_refused_start']
    check('the tap after the old end works',
          rows('after_refused_start') == [('network', 1, True), ('start_failed', 2, False), ('ok', 3, True)]
          and c['outsideGesture'] == 0, c)

    for case, why in (('start_throws', 'a start() that throws'), ('new_throws', 'a new Recognition() that throws')):
        c = cases[case]
        check(why + ' gives start_failed, tap 1, started false', rows(case) == [('start_failed', 1, False)], c['reports'])
        check(why + ' shows the notice', not c['note']['hidden'] and c['note']['notice'] and c['note']['text'] == STOPPED,
              c['note'])
    for case, word in (('aborted', 'aborted'), ('other_error', 'other_error')):
        c = cases[case]
        check('the browser error for {} gives {} and the notice'.format(case, word),
              rows(case) == [(word, 1, True)] and not c['note']['hidden'] and c['note']['text'] == STOPPED, c)

    for key in ('send', 'hide', 'call', 'stop'):
        c = cases['page_ended_' + key]
        if c['skipped']:
            print('SKIP page_ended after {}: the call button is disabled on this test Harold'.format(key))
            continue
        check('{} gives page_ended, tap 1, started true, and no notice'.format(key),
              rows('page_ended_' + key) == [('page_ended', 1, True)] and not c['note']['notice'], c)
    c = cases['stop_without_end']
    check('a stop with no end waits for it; a second stop gives page_ended and no notice',
          c['beforeSecondStop'] == 0 and rows('stop_without_end') == [('page_ended', 1, True)]
          and not c['note']['notice'], c)
    c = cases['stop_timer']
    check('a stop with no end is retired after 1 second: page_ended, no notice, Listening gone',
          c['beforeTimer'] == 0 and rows('stop_timer') == [('page_ended', 1, True)]
          and not c['note']['notice'] and c['note']['hidden'] and c['instances'] == 1, c)
    c = cases['abort_never_ends_tap2']
    check('abort never ends: the second tap gives start_failed and the notice, and the fake refuses it once',
          rows('abort_never_ends_tap2') == [('network', 1, True), ('start_failed', 2, False)]
          and c['refusals'] == 1 and not c['note']['hidden'] and c['note']['text'] == TAP_AGAIN
          and c['outsideGesture'] == 0, c)
    c = cases['abort_never_ends_tap3']
    check('abort never ends: the third tap gives start_failed and the notice, and the first instance stays active',
          rows('abort_never_ends_tap3') == [('network', 1, True), ('start_failed', 2, False), ('start_failed', 3, False)]
          and c['refusals'] == 2 and not c['note']['hidden'] and c['note']['text'] == TAP_AGAIN
          and c['firstStillActive'] and c['outsideGesture'] == 0, c)
    c = cases['stop_after_result']
    check('a stop after a result gives ok', rows('stop_after_result') == [('ok', 1, True)], c['reports'])
    c = cases['no_result']
    check('a tap with no result and no error gives no_result', rows('no_result') == [('no_result', 1, True)], c['reports'])

    c = cases['limit']
    taps = [r['tap'] for r in c['reports']]
    check('the 101st tap on one page sends no report', len(taps) == 100 and taps == list(range(1, 101)), taps[-3:])
    check('the 101st tap still writes its words into the box', c['box'] == 'tap 101', c['box'])
    check('every report posts result, tap and started, and no other signal',
          all(only_signals(k) for k in cases), {k: cases[k]['reports'][:1] for k in cases})


if __name__ == '__main__':
    sys.exit(main())
