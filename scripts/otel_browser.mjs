#!/usr/bin/env node
// The phone's side of scripts/otel_test.py: a headless Chrome at 390 px that opens the test
// Harold's web app, does one thing a person does and waits for the chief's answer row. It
// drives Chrome over the DevTools protocol, with Node's own WebSocket, and adds the login
// header to every request the way Tailscale Serve does for the phone. Plain Node, no packages.
//
//   otel_browser.mjs --scenario reply|keys|todo --url URL --login LOGIN --chrome PATH --out FILE
//                    [--text WORDS] [--screens FOLDER] [--limit SECONDS]
//
// It writes FILE as JSON: when the page loaded, when Send was tapped, and when the answer
// row was drawn, all in the page's clock (Date.now(), milliseconds), and the console errors.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const args = Object.fromEntries(
  process.argv.slice(2).reduce((pairs, arg, i, all) => {
    if (arg.startsWith("--")) pairs.push([arg.slice(2), all[i + 1]]);
    return pairs;
  }, []),
);
const limitMs = Number(args.limit ?? 240) * 1000;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const profile = fs.mkdtempSync(path.join(os.tmpdir(), "otel-chrome-"));
const port = 9300 + Math.floor(Math.random() * 500);
const chrome = spawn(
  args.chrome,
  [
    "--headless=new",
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-extensions",
    "--window-size=390,844",
    "about:blank",
  ],
  { stdio: "ignore" },
);

async function finish(code) {
  chrome.kill("SIGTERM");
  await sleep(500);
  fs.rmSync(profile, { recursive: true, force: true });
  process.exit(code);
}

async function target() {
  for (let i = 0; i < 100; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = list.find((entry) => entry.type === "page");
      if (page) return page.webSocketDebuggerUrl;
    } catch {
      // Chrome is still starting.
    }
    await sleep(200);
  }
  throw new Error("Chrome did not start");
}

const socket = new WebSocket(await target());
await new Promise((resolve, reject) => {
  socket.onopen = resolve;
  socket.onerror = reject;
});
let next = 0;
const waiting = new Map();
const consoleErrors = [];
const requests = [];
const posts = [];
socket.onmessage = (event) => {
  const message = JSON.parse(event.data);
  if (message.id && waiting.has(message.id)) {
    waiting.get(message.id)(message);
    waiting.delete(message.id);
  } else if (message.method === "Network.requestWillBeSent") {
    const { url, method } = message.params.request;
    if (method === "POST") posts.push(new URL(url).pathname);
  } else if (message.method === "Network.responseReceived") {
    const { url, status } = message.params.response;
    requests.push(`${status} ${new URL(url).pathname}`);
  } else if (message.method === "Network.loadingFailed") {
    requests.push(`failed ${message.params.errorText}`);
  } else if (message.method === "Runtime.exceptionThrown") {
    consoleErrors.push(message.params.exceptionDetails.text + " " + (message.params.exceptionDetails.exception?.description ?? ""));
  } else if (message.method === "Runtime.consoleAPICalled" && message.params.type === "error") {
    consoleErrors.push(message.params.args.map((arg) => arg.value ?? arg.description).join(" "));
  }
};
const send = (method, params = {}) =>
  new Promise((resolve) => {
    const id = ++next;
    waiting.set(id, resolve);
    socket.send(JSON.stringify({ id, method, params }));
  });
const evaluate = async (expression) => {
  const reply = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (reply.result.exceptionDetails) throw new Error(reply.result.exceptionDetails.text + " in " + expression);
  return reply.result.result.value;
};
const until = async (what, check, ms = limitMs) => {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (await evaluate(check)) return;
    await sleep(100);
  }
  throw new Error(`timed out waiting for ${what}`);
};
const screenshot = async (name) => {
  if (!args.screens) return;
  fs.mkdirSync(args.screens, { recursive: true });
  const shot = await send("Page.captureScreenshot", { format: "png" });
  fs.writeFileSync(path.join(args.screens, `${name}.png`), Buffer.from(shot.result.data, "base64"));
};

// A real key press: DevTools sends it as the keyboard does, so the page's own key rules run.
const press = async (key, modifiers = 0) => {
  const base = { key, code: key, windowsVirtualKeyCode: 13, modifiers };
  await send("Input.dispatchKeyEvent", { type: "keyDown", text: "\r", ...base });
  await send("Input.dispatchKeyEvent", { type: "keyUp", ...base });
};
const SHIFT = 8;
const expect = (what, ok, seen) => {
  if (!ok) throw new Error(`${what}, saw ${JSON.stringify(seen)}`);
};

const result = { scenario: args.scenario, consoleErrors, requests, posts };
try {
  await send("Runtime.enable");
  await send("Network.enable");
  await send("Page.enable");
  await send("Network.setExtraHTTPHeaders", { headers: { "Tailscale-User-Login": args.login } });
  await send("Emulation.setDeviceMetricsOverride", {
    width: 390,
    height: 844,
    deviceScaleFactor: 2,
    mobile: true,
  });
  await send("Page.navigate", { url: args.url });
  await until("the page to load", `document.readyState === "complete" && !!document.querySelector("#reply")`, 60000);
  await sleep(1500);
  result.loadedAt = await evaluate("Date.now()");
  const chiefRows = `document.querySelectorAll("#messages .message.chief").length`;
  const before = await evaluate(chiefRows);
  await screenshot("1-loaded");

  if (args.scenario === "reply") {
    // A script error, for the browser.error span.
    await evaluate(`setTimeout(() => { throw new TypeError("deliberate test error"); }, 0)`);
    await evaluate(`document.querySelector("#reply textarea").focus()`);
    await send("Input.insertText", { text: args.text });
    await sleep(300);
    await screenshot("2-typed");
    result.tappedAt = await evaluate(`(() => { document.querySelector('#reply button[type="submit"]').click(); return Date.now(); })()`);
  } else if (args.scenario === "keys") {
    // Shift+Enter adds a line and sends nothing; Enter alone sends both lines.
    const box = `document.querySelector("#reply textarea")`;
    await evaluate(`${box}.focus()`);
    await send("Input.insertText", { text: "line one" });
    await press("Enter", SHIFT);
    await send("Input.insertText", { text: "line two" });
    await sleep(500);
    const held = await evaluate(`${box}.value`);
    expect("the box holds the two lines", held === "line one\nline two", held);
    expect("Shift+Enter makes no POST /replies", !posts.includes("/replies"), posts);
    result.boxHeightOfTwo = await evaluate(`${box}.offsetHeight`);
    await screenshot("2-two-lines");
    await press("Enter");
    result.tappedAt = await evaluate("Date.now()");
    await until("the thread to show both lines", `[...document.querySelectorAll("#messages .message .text")].some((p) => p.textContent === "line one\\nline two")`);
    expect("Enter makes one POST /replies", posts.filter((path) => path === "/replies").length === 1, posts);
    const sent = requests.filter((entry) => entry.endsWith(" /replies"));
    expect("the POST /replies is answered 2xx", sent.length === 1 && sent[0].startsWith("2"), sent);
    result.sent = sent;
    await until("the box to be empty and one line high", `${box}.value === "" && ${box}.offsetHeight <= 48`);
  } else if (args.scenario === "todo") {
    await evaluate(`document.querySelector("#to-todos").click()`);
    await until("the todo card", `!!document.querySelector("#todos article.card form.todo-form")`, 30000);
    await sleep(800);
    await evaluate(`(() => {
      const form = document.querySelector("#todos article.card form.todo-form");
      const pick = form.querySelector('input[name="pick.0"]');
      pick.click();
    })()`);
    await sleep(1500);
    await screenshot("2-picked");
    result.tappedAt = await evaluate(`(() => { document.querySelector("#todos article.card form.todo-form button.send").click(); return Date.now(); })()`);
  } else {
    throw new Error("unknown scenario " + args.scenario);
  }

  await until("the chief's answer row", `${chiefRows} > ${before}`);
  result.answerDrawnAt = await evaluate("Date.now()");
  await screenshot("3-answered");
  // The page sends its spans every 5 s and when it is hidden.
  await sleep(7000);
  result.ok = true;
} catch (error) {
  result.ok = false;
  result.error = String(error.message ?? error);
}
fs.writeFileSync(args.out, JSON.stringify(result, null, 2));
await finish(result.ok ? 0 : 1);
