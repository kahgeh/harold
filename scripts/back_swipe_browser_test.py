#!/usr/bin/env python3
"""Checks the back swipe in a headless Chrome with touch events, on a test Harold.

It reuses scripts/call_test.py's set-up (a scratch config: a temporary HAROLD_CONFIG_DIR, HOME,
TMPDIR and TMUX_TMPDIR, free ports, a private tmux server, no iMessage listener) and scripts/
otel_test.py's browser set-up (a headless Chrome over the DevTools protocol, with the login header
that Tailscale Serve adds). The test Harold has two collections (one with a very long title), a
scratch product registry with one spec, and the web app listener only. The web app is paired off.
The test needs a Harold built with the default features (cargo build --locked).

The Notes page and a note page are not checked: they need the knowledge tool, which needs a vault,
an embedding model and a bucket that a scratch config cannot give. The test says so with `SKIP`.

Each check prints `PASS name` or `FAIL name: why`. The exit code is 1 when one check fails.
The installed Harold, ~/bin, the user's tmux server, Rex, Tailscale and the real config are not
touched. Needs node, tmux and Google Chrome. Python 3 standard library only.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile

import call_test as ct
import otel_test as ot

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
NAME = 'swipe-test'
TITLE = 'Swipe Test Pages'
LONG_NAME = 'long-title'
LONG_TITLE = 'WWWWWWWWWW WWWWWWWWWW WWWWWWWWWW WWWWWWWWWW WWWWWWWWWW'  # 60 wide letters
PRODUCT = 'demo'
SPEC_ID = 'swipe-spec'
MISSING_ID = 'no-such-spec'
PAGE = ('<!doctype html><html><head><meta charset="utf-8"><title>Swipe Page</title></head>'
        '<body><h1>Swipe page</h1><p>Plain page.</p></body></html>\n')
WIDE = '| ' + ' | '.join('column-%d' % i for i in range(12)) + ' |\n'
WIDE += '|' + '---|' * 12 + '\n'
WIDE += '| ' + ' | '.join('a-long-cell-value-%d' % i for i in range(12)) + ' |\n'
NOTES = '# Swipe Notes\n\nSome text to select.\n\n' + WIDE
SPEC = 'Waiting for your approval.\n\n# swipe-spec: Swipe spec\n\nA paragraph.\n'

# Pages in scope: (key, path, back address, has the Harold control).
PAGES = [
    ('collections', '/c', '/', False),
    ('list', '/c/' + NAME, '/c', True),
    ('md-page', '/c/{}/notes.md'.format(NAME), '/c/' + NAME, True),
    ('html-page', '/c/{}/page.html'.format(NAME), '/c/' + NAME, True),
    ('specs', '/specs', '/c', True),
    ('spec', '/specs/{}/{}'.format(PRODUCT, SPEC_ID), '/', False),
    ('spec-gone', '/specs/{}/{}'.format(PRODUCT, MISSING_ID), '/specs', True),
]

BROWSER = r'''
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const args = JSON.parse(process.argv[2]);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const profile = fs.mkdtempSync(path.join(os.tmpdir(), "backswipe-chrome-"));
const port = args.debugPort;
const chrome = spawn(args.chrome, ["--headless=new", `--remote-debugging-port=${port}`,
  `--user-data-dir=${profile}`, "--no-first-run", "--no-default-browser-check", "--mute-audio",
  "--disable-extensions", "--window-size=390,844", "about:blank"], { stdio: "ignore" });
const result = { pages: {}, errors: [] };
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
const until = async (what, check, ms = 20000) => {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) { if (await check()) return; await sleep(100); }
  throw new Error("timed out waiting for " + what);
};
const shot = async (name) => {
  if (!args.shots) return;
  fs.mkdirSync(args.shots, { recursive: true });
  const s = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync(path.join(args.shots, name + ".png"), Buffer.from(s.result.data, "base64"));
};
const emulate = async (width) => {
  await send("Emulation.setDeviceMetricsOverride", { width, height: 844, deviceScaleFactor: 2, mobile: true });
  await send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 1 });
};
const goto = async (url) => {
  await send("Page.navigate", { url });
  await sleep(300);
  await until("load of " + url, async () => (await evaluate("document.readyState")) === "complete");
  await sleep(300);
};
// Records, in the page, the defaultPrevented value of each touchmove after the page's own listener.
const watch = () => evaluate(`(() => { window.__marker = true; window.__moves = [];
  document.addEventListener("touchmove", (e) => window.__moves.push(e.defaultPrevented), { passive: true });
  return true; })()`);
const touch = (type, x, y) => send("Input.dispatchTouchEvent",
  { type, touchPoints: type === "touchEnd" || type === "touchCancel" ? [] : [{ x, y, id: 1 }] });
// A drag by touch: start, moves of 10 px, end (or cancel).
async function drag(x0, y0, x1, y1, end = "touchEnd") {
  await touch("touchStart", x0, y0);
  const steps = Math.max(1, Math.round(Math.max(Math.abs(x1 - x0), Math.abs(y1 - y0)) / 10));
  for (let i = 1; i <= steps; i++) {
    await touch("touchMove", x0 + ((x1 - x0) * i) / steps, y0 + ((y1 - y0) * i) / steps);
    await sleep(16);
  }
  await touch(end, x1, y1);
}
// Drags, then reports where the page is: moved is true when the page left its own address.
async function trial(url, x0, y0, x1, y1, end) {
  await goto(args.base + url);
  await watch();
  await drag(x0, y0, x1, y1, end);
  await sleep(900);
  const marker = await evaluate("window.__marker === true").catch(() => false);
  const here = await evaluate("location.pathname");
  const moves = marker ? await evaluate("window.__moves") : [];
  return { here, stayed: marker && here === url, moves };
}
const tap = async (x, y) => {
  const p = { x, y, button: "left", clickCount: 1 };
  await send("Input.dispatchMouseEvent", { type: "mousePressed", ...p });
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", ...p });
};

try {
  await send("Runtime.enable"); await send("Network.enable"); await send("Page.enable");
  await send("Network.setExtraHTTPHeaders", { headers: { "Tailscale-User-Login": args.login } });
  await emulate(390);

  // 1. An edge swipe opens the back address; the Harold control opens "/".
  for (const p of args.pages) {
    const r = (result.pages[p.key] = {});
    await goto(args.base + p.path);
    r.count = await evaluate(`document.querySelectorAll("[data-back]").length`);
    r.dataBack = await evaluate(`document.querySelector("[data-back]")?.getAttribute("href")`);
    r.script = await evaluate(`!!document.querySelector('script[src="/static/back-swipe.js"]')`);
    r.header = await evaluate(`(() => { const h = document.querySelector("header"); const b = document.querySelector("[data-back]");
      const k = document.querySelector("#to-chat"); const rect = (e) => { if (!e) return null; const r = e.getBoundingClientRect(); return { left: r.left, right: r.right, width: r.width, height: r.height }; };
      return { inHeader: !!h && !!b && h.contains(b), back: rect(b), harold: rect(k), haroldText: k ? k.textContent.trim() : null, innerWidth: innerWidth, scrollWidth: document.documentElement.scrollWidth }; })()`);
    await shot(p.key + "-390");
    const t = await trial(p.path, 8, 400, 140, 410);
    r.swipe = t;
    if (p.harold) {
      await goto(args.base + p.path);
      const rect = await evaluate(`(() => { const r = document.querySelector("#to-chat").getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; })()`);
      await tap(rect.x, rect.y);
      try { await until("Harold", async () => (await evaluate("location.pathname")) === "/", 8000); } catch {}
      r.haroldOpens = await evaluate("location.pathname");
    }
  }

  // 2 and 3. Touches from 18 px and from 40 px do nothing; also on a page shown in a frame.
  const html = "/c/" + args.name + "/page.html";
  const md = "/c/" + args.name + "/notes.md";
  result.from18 = await trial(html, 18, 400, 150, 410);
  result.from40 = await trial(html, 40, 400, 170, 410);
  result.from18md = await trial(md, 18, 400, 150, 410);
  result.from40list = await trial("/c/" + args.name, 40, 400, 170, 410);

  // 4. A table scrolled sideways by 50 px, and a text selection.
  await goto(args.base + md);
  result.tableFound = await evaluate(`!!document.querySelector(".note-text table")`);
  // The table starts inside the page's side padding; this style lets it reach the screen edge.
  await evaluate(`(() => { const t = document.querySelector(".note-text table"); const left = t.getBoundingClientRect().left;
    t.style.marginLeft = (-left) + "px"; t.style.maxWidth = "none"; t.querySelectorAll("th, td").forEach((c) => { c.style.whiteSpace = "nowrap"; }); t.style.width = "calc(100% + " + left + "px)"; t.scrollLeft = 50; })()`);
  await sleep(200);
  result.tableState = await evaluate(`(() => { const t = document.querySelector(".note-text table"); const r = t.getBoundingClientRect();
    return { scrollLeft: t.scrollLeft, left: r.left, top: r.top, bottom: r.bottom, scrollWidth: t.scrollWidth, clientWidth: t.clientWidth,
      hit: document.elementFromPoint(8, r.top + r.height / 2)?.closest("table") === t }; })()`);
  await watch();
  const ts = result.tableState;
  await drag(8, ts.top + (ts.bottom - ts.top) / 2, 140, ts.top + (ts.bottom - ts.top) / 2);
  await sleep(900);
  result.table = { stayed: (await evaluate("window.__marker === true").catch(() => false)) && (await evaluate("location.pathname")) === md,
    moves: await evaluate("window.__moves").catch(() => []) };

  await goto(args.base + md);
  await watch();
  await evaluate(`window.getSelection().selectAllChildren(document.querySelector(".note-text"))`);
  result.selectionText = await evaluate(`window.getSelection().toString().length`);
  await drag(8, 500, 140, 505);
  await sleep(900);
  result.selection = { stayed: (await evaluate("window.__marker === true").catch(() => false)) && (await evaluate("location.pathname")) === md,
    moves: await evaluate("window.__moves").catch(() => []) };

  // 5. A qualified swipe has defaultPrevented on its touchmove (from the first trial of each page).
  // 6. A mostly vertical edge drag, and a touchcancel.
  result.vertical = await trial(md, 8, 300, 30, 560);
  result.cancel = await trial(md, 8, 400, 140, 410, "touchCancel");
  result.short = await trial(md, 8, 400, 60, 405);

  // 7. Regression: on the thread the todos swipe still works and the page has no back-swipe.js.
  await goto(args.base + "/");
  result.thread = { script: await evaluate(`!!document.querySelector('script[src="/static/back-swipe.js"]') || !!document.querySelector("[data-back]")`),
    hasPages: await evaluate(`!!document.querySelector("#pages")`),
    toPages: await evaluate(`document.querySelector("#to-pages")?.getAttribute("href") ?? null`) };
  await shot("thread-390");
  await drag(330, 400, 40, 405);
  try { await until("the todos page", async () => await evaluate(`document.body.classList.contains("on-todos")`), 5000); } catch {}
  result.thread.onTodos = await evaluate(`document.body.classList.contains("on-todos")`);
  result.thread.path = await evaluate("location.pathname");
  await shot("todos-390");

  // 8. At 320 px a very long collection title keeps Harold visible and at least 44 px.
  await emulate(320);
  for (const [key, url] of [["long-list", "/c/" + args.longName], ["long-page", "/c/" + args.longName + "/page.html"]]) {
    await goto(args.base + url);
    result.pages[key] = { header: await evaluate(`(() => { const k = document.querySelector("#to-chat"); const b = document.querySelector("[data-back]");
      const label = b.querySelector(".back-label");
      const rect = (e) => { if (!e) return null; const r = e.getBoundingClientRect(); return { left: r.left, right: r.right, width: r.width, height: r.height }; };
      return { harold: rect(k), back: rect(b), innerWidth: innerWidth, scrollWidth: document.documentElement.scrollWidth,
        labelCut: label.scrollWidth > label.clientWidth, visible: !!k && k.getBoundingClientRect().right <= innerWidth + 0.5 && k.getBoundingClientRect().left >= 0 }; })()`) };
    await shot(key + "-320");
  }
  await finish(0);
} catch (e) {
  result.errors.push(String(e.stack || e));
  await finish(1);
}
'''


class Results:
    def __init__(self):
        self.failed = False

    def check(self, name, ok, why=''):
        if ok:
            print('PASS ' + name)
        else:
            self.failed = True
            print('FAIL {}: {}'.format(name, why))


class SwipeRun(ct.Run):
    """The call test's run with two collections, a product registry and no stand-in chief."""

    def __init__(self, binary, default_toml, keep, folder, long_folder, registry):
        super().__init__(ct.INSTALL_DIR, keep, binary=binary, default_toml=default_toml)
        self.folder = folder
        self.long_folder = long_folder
        self.registry = registry

    def write_config(self):
        for folder in (self.config_dir, self.chief_dir, self.scratch / 't', self.scratch / 'home'):
            folder.mkdir(parents=True, exist_ok=True)
        shutil.copy(self.default_toml, self.config_dir / 'default.toml')
        self.grpc_port = ct.free_port()
        self.web_port = ct.free_port()
        specs_port = ct.free_port()
        config = ct.scratch_config(
            '', chief_dir=str(self.chief_dir), grpc_port=self.grpc_port, web_port=self.web_port,
            store_path=str(self.scratch / 'store'), chat_db_path=str(self.scratch / 'no-messages.db'))
        # A free port for the specs listener, so the test never meets the real one on 4331.
        config = config.replace('require_pairing = false\n',
                                'require_pairing = false\nspecs_listen = "127.0.0.1:{}"\n'.format(specs_port), 1)
        config = config.replace('[telemetry]\nexport = false\n',
                                '[telemetry]\nexport = false\npod_registry = "{}"\n'.format(self.registry), 1)
        for name, title, folder in ((NAME, TITLE, self.folder), (LONG_NAME, LONG_TITLE, self.long_folder)):
            config += ('\n[[web_app.collections]]\nname = "{}"\ntitle = "{}"\nfolder = "{}"\n'
                       'extensions = ["html", "md"]\n'.format(name, title, folder))
        (self.config_dir / 'local.toml').write_text(config)
        self.env['HOME'] = str(self.scratch / 'home')


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
    parser.add_argument('--harold', type=Path, default=REPO / 'target' / 'debug' / 'harold')
    parser.add_argument('--default-config', type=Path, default=REPO / 'harold' / 'config' / 'default.toml')
    parser.add_argument('--shots', type=Path, help='where to write the 390 px screenshots (default: none)')
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

    root = Path(os.path.realpath(tempfile.mkdtemp(prefix='hbs-')))
    folder, long_folder, repo = root / 'pages', root / 'long', root / 'repo'
    for path in (folder, long_folder, repo / '.superpowers' / 'pod' / 'specs'):
        path.mkdir(parents=True)
    (folder / 'page.html').write_text(PAGE)
    (folder / 'notes.md').write_text(NOTES)
    (long_folder / 'page.html').write_text(PAGE)
    (repo / '.superpowers' / 'pod' / 'specs' / (SPEC_ID + '.md')).write_text(SPEC)
    registry = root / 'products.toml'
    registry.write_text('[products.{}]\nrepo = "{}"\nbase = "main"\n'.format(PRODUCT, repo))
    run = SwipeRun(binary, args.default_config.resolve(), args.keep, folder, long_folder, registry)
    for name in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(name, lambda number, frame: sys.exit(128 + number))
    results = Results()
    try:
        run.write_config()
        run.start_harold()
        base = 'http://127.0.0.1:{}'.format(run.web_port)
        out = run.scratch / 'browser.json'
        script = run.scratch / 'browser.mjs'
        script.write_text(BROWSER)
        config = json.dumps({
            'chrome': ot.CHROME, 'base': base, 'login': ct.LOGIN, 'name': NAME, 'longName': LONG_NAME,
            'shots': str(args.shots) if args.shots else '', 'out': str(out), 'debugPort': ct.free_port(),
            'pages': [{'key': k, 'path': p, 'back': b, 'harold': h} for k, p, b, h in PAGES]})
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
            return 1
        data = json.loads(out.read_text())
        if data['errors']:
            results.check('browser ran', False, data['errors'][0][:3000])
            return 1
        verify(results, data)
        if args.shots:
            print('screenshots in ' + str(args.shots))
    except ct.StepFailed as error:
        results.check('test Harold starts', False, error)
    finally:
        run.tear_down()
        shutil.rmtree(root, ignore_errors=True)
    run.finish(not results.failed)
    return 1 if results.failed else 0


def verify(results, data):
    check = results.check
    print('SKIP /notes and a note page: they need the knowledge tool (a vault, a model and a bucket)')
    for key, path, back, harold in PAGES:
        r = data['pages'][key]
        check('one data-back with the address, and the script: ' + path,
              r['count'] == 1 and r['dataBack'] == back and r['script'], r)
        h = r['header']
        check('header holds the back control: ' + path, h['inHeader'] and h['back']['width'] > 0, h)
        check('edge swipe opens ' + back + ' from ' + path, r['swipe']['here'] == back and not r['swipe']['stayed'], r['swipe'])
        if harold:
            check('Harold control is in the header and opens / from ' + path,
                  h['harold'] is not None and h['haroldText'] == 'Harold' and r['haroldOpens'] == '/', r)
        else:
            check('no Harold control on ' + path, h['harold'] is None, h)
        check('no horizontal scroll at 390 px on ' + path, h['scrollWidth'] <= h['innerWidth'], h)

    def nothing(name, t, prevented=False):
        check(name, t['stayed'] and (not any(t['moves']) if not prevented else True), t)

    nothing('a touch from 18 px on a collection .html page does nothing', data['from18'])
    nothing('a touch from 40 px on a collection .html page does nothing', data['from40'])
    nothing('a touch from 18 px on a markdown page does nothing', data['from18md'])
    nothing('a touch from 40 px on a list does nothing', data['from40list'])
    check('the table was scrolled by 50 px and reaches the edge', data['tableFound'] and data['tableState']['scrollLeft'] >= 49
          and data['tableState']['hit'], data['tableState'])
    nothing('a swipe from a table scrolled sideways does nothing', data['table'])
    check('the selection held text', data['selectionText'] > 0, data['selectionText'])
    nothing('a swipe with a text selection does nothing', data['selection'])
    nothing('a mostly vertical edge drag does nothing', data['vertical'])
    check('a touchcancel does nothing', data['cancel']['stayed'], data['cancel'])
    check('a short edge swipe (52 px) does nothing', data['short']['stayed'], data['short'])
    check('touchmove of a qualified swipe is prevented',
          len(data['cancel']['moves']) > 3 and all(data['cancel']['moves']), data['cancel'])
    # On the .html pages the touch lands in the frame, so the page sees no touchmove at all.
    check('touchmove of every skipped swipe is not prevented',
          all(not any(data[k]['moves']) for k in ('from18', 'from40', 'from18md', 'from40list', 'table', 'selection', 'vertical')),
          {k: data[k]['moves'] for k in ('from18', 'from40', 'from18md', 'from40list', 'table', 'selection', 'vertical')})
    t = data['thread']
    check('the thread has no back-swipe.js and no data-back', t['hasPages'] and not t['script'], t)
    check('the thread header links Pages', t['toPages'] == '/c', t)
    check('the todos swipe still works on the thread', t['onTodos'] and t['path'] == '/', t)
    for key in ('long-list', 'long-page'):
        h = data['pages'][key]['header']
        k = h['harold']
        check('at 320 px Harold stays visible, 44 px or more, with a long title: ' + key,
              k is not None and h['visible'] and k['width'] >= 44 and k['height'] >= 44 and h['labelCut']
              and h['scrollWidth'] <= h['innerWidth'], h)
        print('info: at 320 px on {} the back control is {:.0f} x {:.0f} px'.format(key, h['back']['width'], h['back']['height']))


if __name__ == '__main__':
    sys.exit(main())
