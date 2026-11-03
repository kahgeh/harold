#!/usr/bin/env python3
"""Checks the page collections in a headless Chrome, on a test Harold.

It reuses scripts/call_test.py's set-up (a scratch config: a temporary HAROLD_CONFIG_DIR, HOME,
TMPDIR and TMUX_TMPDIR, free ports, a private tmux server, no iMessage listener) and scripts/
otel_test.py's browser set-up (a headless Chrome over the DevTools protocol, with the login header
that Tailscale Serve adds). The test Harold has one collection with a scratch folder that holds
`page.html` (an inline style, an inline SVG, a `data:` image and a script) and `notes.md`. A small
HTTP server on a second free port frames the page's `/raw` address from another origin.

Each check prints `PASS name` or `FAIL name: why`. The exit code is 1 when one check fails.
The installed Harold, ~/bin, the user's tmux server, Rex, Tailscale and the real config are not
touched. Needs node, tmux and Google Chrome. Python 3 standard library only.
"""

import argparse
import http.client
import http.server
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading

import call_test as ct
import otel_test as ot

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
NAME = 'browser-test'
TITLE = 'Browser Test Pages'
PAGE_TITLE = 'Browser Test Page'
MD_TITLE = 'Browser Test Notes'
POLICY = ("sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; "
          "script-src 'none'; connect-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'")
# A 2 x 2 red PNG.
PNG = ('iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAFElEQVQI12P8z8Dwn4GBgYGJAQoAHxcCAr7'
       'Eb3cAAAAASUVORK5CYII=')
PAGE = '''<!doctype html>
<html><head><meta charset="utf-8"><title>{title}</title>
<style>body {{ font-family: sans-serif; }} h1 {{ color: #036; }}</style></head>
<body><h1>Sandbox check</h1>
<svg id="shape" width="120" height="60" viewBox="0 0 120 60"><rect id="box" x="0" y="0" width="120" height="60" fill="#c33"/></svg>
<img id="pic" alt="" width="40" height="40" src="data:image/png;base64,{png}">
<script>document.title = "script-ran"; var p = document.createElement("p"); p.id = "ran"; document.body.appendChild(p);</script>
</body></html>
'''.format(title=PAGE_TITLE, png=PNG)

BROWSER = r'''
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const args = JSON.parse(process.argv[2]);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const profile = fs.mkdtempSync(path.join(os.tmpdir(), "collections-chrome-"));
const port = args.debugPort;
const chrome = spawn(args.chrome, ["--headless=new", `--remote-debugging-port=${port}`,
  `--user-data-dir=${profile}`, "--no-first-run", "--no-default-browser-check", "--mute-audio",
  "--disable-extensions", "--window-size=390,844", "about:blank"], { stdio: "ignore" });
const result = { widths: {}, errors: [] };
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
let events = [];
const sessions = new Map();
socket.onmessage = (event) => {
  const m = JSON.parse(event.data);
  if (m.id && waiting.has(m.id)) { waiting.get(m.id)(m); waiting.delete(m.id); }
  else if (m.method === "Target.attachedToTarget") sessions.set(m.params.sessionId, m.params.targetInfo);
  else if (m.method === "Target.detachedFromTarget") sessions.delete(m.params.sessionId);
  else if (m.method === "Target.targetInfoChanged") { for (const [sid, info] of sessions) if (info.targetId === m.params.targetInfo.targetId) sessions.set(sid, m.params.targetInfo); }
  else if (m.method) events.push(m);
};
const send = (method, params = {}, sessionId) => new Promise((res) => {
  const id = ++next; waiting.set(id, res);
  const message = { id, method, params };
  if (sessionId) message.sessionId = sessionId;
  socket.send(JSON.stringify(message));
});
const evaluate = async (expression, contextId, sessionId) => {
  const p = { expression, awaitPromise: true, returnByValue: true };
  if (contextId) p.contextId = contextId;
  const r = await send("Runtime.evaluate", p, sessionId);
  if (r.result.exceptionDetails) throw new Error(r.result.exceptionDetails.text + " in " + expression);
  return r.result.result.value;
};
const until = async (what, check, ms = 30000) => {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) { if (await check()) return; await sleep(100); }
  throw new Error("timed out waiting for " + what);
};
const shot = async (name) => {
  fs.mkdirSync(args.shots, { recursive: true });
  const s = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync(path.join(args.shots, name + ".png"), Buffer.from(s.result.data, "base64"));
};
const goto = async (url) => {
  await send("Page.navigate", { url });
  await sleep(300);
  await until("load of " + url, async () => (await evaluate("document.readyState")) === "complete");
};
// The frame is one of the page's frames, or a target of its own when Chrome puts it in another
// process (a sandboxed frame has an opaque origin). Both are read through the DevTools protocol.
const childSession = () => [...sessions].find(([, info]) => info.type === "iframe");
const childFrame = async () => {
  const tree = (await send("Page.getFrameTree")).result.frameTree;
  const own = (tree.childFrames || [])[0]?.frame;
  if (own) return own;
  const found = childSession();
  return found ? { id: found[1].targetId, url: found[1].url, session: found[0] } : undefined;
};
const inFrame = async (expression) => {
  const frame = await childFrame();
  if (frame.session) return evaluate(expression, undefined, frame.session);
  const world = await send("Page.createIsolatedWorld", { frameId: frame.id, worldName: "check" });
  return evaluate(expression, world.result.executionContextId);
};
const tap = async (x, y) => {
  const p = { x, y, button: "left", clickCount: 1 };
  await send("Input.dispatchMouseEvent", { type: "mousePressed", ...p });
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", ...p });
};
const rows = `[...document.querySelectorAll("a.note-row")].map((a) => ({
  href: a.getAttribute("href"), text: a.textContent.trim().replace(/\\s+/g, " "),
  badge: a.querySelector(".badge")?.textContent.trim() ?? null }))`;

try {
  await send("Target.setAutoAttach", { autoAttach: true, waitForDebuggerOnStart: false, flatten: true });
  await send("Runtime.enable"); await send("Network.enable"); await send("Page.enable"); await send("Log.enable");
  await send("Network.setExtraHTTPHeaders", { headers: { "Tailscale-User-Login": args.login } });
  const pagePath = `/c/${args.name}/page.html`;
  for (const width of [390, 320]) {
    const w = (result.widths[width] = {});
    await send("Emulation.setDeviceMetricsOverride", { width, height: 844, deviceScaleFactor: 2, mobile: true });
    await goto(args.base + "/c");
    w.index = { rows: await evaluate(rows), scrollWidth: await evaluate("document.documentElement.scrollWidth"), innerWidth: await evaluate("innerWidth") };
    await shot(`index-${width}`);
    await goto(args.base + "/c/" + args.name);
    w.list = { rows: await evaluate(rows), scrollWidth: await evaluate("document.documentElement.scrollWidth"), innerWidth: await evaluate("innerWidth") };
    await shot(`list-${width}`);
    await goto(args.base + pagePath);
    let last = "";
    try {
      await until("the frame content", async () => {
        const f = await childFrame();
        last = JSON.stringify((await send("Page.getFrameTree")).result) + await evaluate("location.href + document.body.innerHTML.slice(-300)");
        return f && f.url.includes("/raw") && (await inFrame("document.readyState === 'complete' && !!document.querySelector('img') && document.querySelector('img').complete"));
      });
    } catch (e) { throw new Error(e.message + " last frame " + last + " events " + JSON.stringify(events.filter((x) => x.method === "Log.entryAdded").map((x) => x.params.entry.text))); }
    await sleep(500);
    w.shell = await evaluate(`(() => {
      const back = document.querySelector("#to-list"); const frame = document.querySelector("iframe.collection-frame");
      const header = document.querySelector("header"); const b = back.getBoundingClientRect(); const f = frame.getBoundingClientRect();
      const h = header.getBoundingClientRect();
      return { backText: back.textContent.trim(), backHref: back.getAttribute("href"), backInHeader: header.contains(back),
        frameInHeader: header.contains(frame), headerBottom: h.bottom, frame: { left: f.left, top: f.top, right: f.right, bottom: f.bottom },
        backRect: { left: b.left, top: b.top, width: b.width, height: b.height },
        innerWidth: innerWidth, innerHeight: innerHeight, scrollWidth: document.documentElement.scrollWidth,
        bodyScrollWidth: document.body.scrollWidth, sandbox: frame.getAttribute("sandbox"), bg: getComputedStyle(frame).backgroundColor };
    })()`);
    w.inner = await inFrame(`(() => {
      const img = document.querySelector("img"); const box = document.querySelector("#box").getBoundingClientRect();
      const svg = document.querySelector("#shape").getBoundingClientRect();
      return { imgW: img.naturalWidth, imgH: img.naturalHeight, boxW: box.width, boxH: box.height, svgW: svg.width,
        ran: !!document.querySelector("#ran"), title: document.title, docW: document.documentElement.clientWidth,
        h1Color: getComputedStyle(document.querySelector("h1")).color, bodyBg: getComputedStyle(document.body).backgroundColor };
    })()`);
    await shot(`page-${width}`);
    if (width === 390) {
      const b = w.shell.backRect;
      await tap(b.left + b.width / 2, b.top + b.height / 2);
      try { await until("the back navigation", async () => (await evaluate("location.pathname")) === "/c/" + args.name, 10000); } catch {}
      result.afterBack = await evaluate("location.pathname");
      await goto(args.base + pagePath + "/raw");
      await sleep(500);
      await shot("raw-390");
    }
  }
  // Another origin frames the raw page.
  events = [];
  await send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true });
  await goto(args.other);
  await sleep(2500);
  const f = await childFrame();
  result.other = { childUrl: f ? f.url : null, childError: f ? (f.unreachableUrl || null) : null,
    childLoaded: false,
    failures: events.filter((e) => e.method === "Network.loadingFailed").map((e) => e.params),
    responses: events.filter((e) => e.method === "Network.responseReceived").map((e) => ({ url: e.params.response.url, status: e.params.response.status, type: e.params.type })),
    logs: events.filter((e) => e.method === "Log.entryAdded").map((e) => e.params.entry.text),
    consoles: events.filter((e) => e.method === "Runtime.consoleAPICalled").map((e) => e.params.args.map((a) => a.value ?? a.description).join(" ")) };
  if (f) {
    try { result.other.childLoaded = await inFrame("!!document.querySelector('#box') || !!document.querySelector('img#pic')"); }
    catch (e) { result.other.childLoaded = "not readable: " + e.message; }
  }
  await shot("other-origin-390");
  await finish(0);
} catch (e) {
  result.errors.push(String(e.stack || e));
  await finish(1);
}
'''


class Second(http.server.BaseHTTPRequestHandler):
    raw = ''

    def do_GET(self):
        body = ('<!doctype html><html><body><h1>Another origin</h1>'
                '<iframe id="victim" width="360" height="400" src="{}"></iframe></body></html>'.format(self.raw)).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class Results:
    def __init__(self):
        self.failed = False

    def check(self, name, ok, why=''):
        if ok:
            print('PASS ' + name)
        else:
            self.failed = True
            print('FAIL {}: {}'.format(name, why))


class CollectionsRun(ct.Run):
    """The call test's run with a collection in the config and no stand-in chief."""

    def __init__(self, binary, default_toml, keep, folder):
        super().__init__(ct.INSTALL_DIR, keep, binary=binary, default_toml=default_toml)
        self.folder = folder

    def write_config(self):
        for folder in (self.config_dir, self.chief_dir, self.scratch / 't', self.scratch / 'home'):
            folder.mkdir(parents=True, exist_ok=True)
        shutil.copy(self.default_toml, self.config_dir / 'default.toml')
        self.grpc_port = ct.free_port()
        self.web_port = ct.free_port()
        config = ct.scratch_config(
            '', chief_dir=str(self.chief_dir), grpc_port=self.grpc_port, web_port=self.web_port,
            store_path=str(self.scratch / 'store'), chat_db_path=str(self.scratch / 'no-messages.db'))
        config += ('\n[[web_app.collections]]\nname = "{}"\ntitle = "{}"\nfolder = "{}"\n'
                   'extensions = ["html", "md"]\n'.format(NAME, TITLE, self.folder))
        (self.config_dir / 'local.toml').write_text(config)
        self.env['HOME'] = str(self.scratch / 'home')

    def raw_response(self):
        connection = http.client.HTTPConnection('127.0.0.1', self.web_port, timeout=10)
        try:
            connection.request('GET', '/c/{}/page.html/raw'.format(NAME), headers={'Tailscale-User-Login': ct.LOGIN})
            response = connection.getresponse()
            response.read()
            return response.status, dict((k.lower(), v) for k, v in response.getheaders())
        finally:
            connection.close()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--keep', action='store_true', help='keep the scratch folder')
    parser.add_argument('--harold', type=Path, default=REPO / 'target' / 'debug' / 'harold')
    parser.add_argument('--default-config', type=Path, default=REPO / 'harold' / 'config' / 'default.toml')
    parser.add_argument('--screens', type=Path, help='where to keep the screenshots (default: a new temporary folder)')
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
        print('FAIL no Harold at {} (run cargo build --locked -p harold)'.format(binary))
        return 1

    folder = Path(os.path.realpath(tempfile.mkdtemp(prefix='hcc-')))
    (folder / 'page.html').write_text(PAGE)
    (folder / 'notes.md').write_text('# {}\n\nPlain text, no diagram.\n'.format(MD_TITLE))
    run = CollectionsRun(binary, args.default_config.resolve(), args.keep, folder)
    for name in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(name, lambda number, frame: sys.exit(128 + number))
    results = Results()
    server = None
    try:
        run.write_config()
        run.start_harold()
        base = 'http://127.0.0.1:{}'.format(run.web_port)
        Second.raw = '{}/c/{}/page.html/raw'.format(base, NAME)
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Second)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        other = 'http://127.0.0.1:{}/'.format(server.server_address[1])

        out = run.scratch / 'browser.json'
        script = run.scratch / 'browser.mjs'
        script.write_text(BROWSER)
        screens = args.screens or Path(tempfile.mkdtemp(prefix='collections-browser-'))
        screens.mkdir(parents=True, exist_ok=True)
        config = json.dumps({'chrome': ot.CHROME, 'base': base, 'other': other, 'login': ct.LOGIN,
                             'name': NAME, 'shots': str(screens), 'out': str(out), 'debugPort': ct.free_port()})
        node = subprocess.Popen(['node', str(script), config], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, start_new_session=True)
        try:
            stdout, stderr = node.communicate(timeout=240)
        except subprocess.TimeoutExpired:
            # The group holds node and its Chrome.
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

        for width in ('390', '320'):
            w = data['widths'][width]
            tag = ' at {} px'.format(width)
            index = [r for r in w['index']['rows'] if r['href'] == '/c/' + NAME]
            results.check('index lists the collection with a badge' + tag,
                          len(index) == 1 and TITLE in index[0]['text'] and index[0]['badge'] == '2',
                          w['index']['rows'])
            files = {r['href']: r['text'] for r in w['list']['rows']}
            page_row = files.get('/c/{}/page.html'.format(NAME), '')
            md_row = files.get('/c/{}/notes.md'.format(NAME), '')
            results.check('list shows page.html with its title' + tag,
                          PAGE_TITLE in page_row and MD_TITLE in md_row, files)
            s = w['shell']
            results.check('header back control names the collection, outside the frame' + tag,
                          s['backText'] == TITLE and s['backInHeader'] and not s['frameInHeader']
                          and s['backHref'] == '/c/' + NAME and s['backRect']['width'] > 0, s)
            i = w['inner']
            results.check('data: image loaded in the frame' + tag, i['imgW'] > 0, i)
            results.check('SVG has a bounding box in the frame' + tag, i['boxW'] > 0 and i['boxH'] > 0, i)
            results.check('script did not run in the frame' + tag, not i['ran'] and i['title'] == PAGE_TITLE, i)
            f = s['frame']
            results.check('frame starts 16 px from the left edge' + tag, abs(f['left'] - 16) < 0.5, f)
            results.check('frame fills the screen under the header' + tag,
                          abs(f['top'] - s['headerBottom']) < 1.5 and abs(f['bottom'] - s['innerHeight']) < 1.5
                          and abs(f['right'] - s['innerWidth']) < 1.5, s)
            results.check('no horizontal scroll' + tag,
                          s['scrollWidth'] <= s['innerWidth'] and w['index']['scrollWidth'] <= w['index']['innerWidth']
                          and w['list']['scrollWidth'] <= w['list']['innerWidth'], s)
            print('info{}: frame background {}, page body background {}'.format(tag, s['bg'], i['bodyBg']))

        status, headers = run.raw_response()
        results.check('raw response header is the sandbox policy exactly',
                      status == 200 and headers.get('content-security-policy') == POLICY, headers.get('content-security-policy'))
        o = data['other']
        refused = (o['childLoaded'] is not True) and any(
            'frame-ancestors' in text or 'ancestor' in text for text in o['logs'] + o['consoles']) \
            or any(x.get('blockedReason') == 'csp' for x in o['failures'])
        shown_page = o['childLoaded'] is True
        results.check('another origin cannot frame the raw page',
                      not shown_page and refused, o)
        results.check('back control goes to the collection list', data['afterBack'] == '/c/' + NAME, data['afterBack'])
        print('info: other origin saw child {}, logs {}'.format(o['childUrl'], o['logs'][:2]))
        print('screenshots in ' + str(screens))
    except ct.StepFailed as error:
        results.check('test Harold starts', False, error)
    finally:
        if server is not None:
            server.shutdown()
        run.tear_down()
        shutil.rmtree(folder, ignore_errors=True)
    run.finish(not results.failed)
    return 1 if results.failed else 0


if __name__ == '__main__':
    sys.exit(main())
