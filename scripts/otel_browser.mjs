#!/usr/bin/env node
// The phone's side of scripts/otel_test.py: a headless Chrome at 390 px (or --width) that opens the test
// Harold's web app, does one thing a person does and waits for the chief's answer row. It
// drives Chrome over the DevTools protocol, with Node's own WebSocket, and adds the login
// header to every request the way Tailscale Serve does for the phone. Plain Node, no packages.
//
//   otel_browser.mjs --scenario reply|keys|todo|keyboard --url URL --login LOGIN --chrome PATH --out FILE
//                    [--text WORDS] [--screens FOLDER] [--limit SECONDS] [--width PIXELS]
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
const width = Number(args.width ?? 390);
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
    "--mute-audio",
    "--disable-extensions",
    `--window-size=${width},844`,
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
// A real tap: DevTools sends the press and the release as a finger or a mouse does.
const tap = async (x, y) => {
  const point = { x, y, button: "left", clickCount: 1 };
  await send("Input.dispatchMouseEvent", { type: "mousePressed", ...point });
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", ...point });
};
// The iPhone keyboard shortens the visual viewport and nothing else, and headless Chrome has no
// keyboard. So the keyboard scenario swaps window.visualViewport for a stand-in that holds the
// same numbers and sends the same "resize" event; window.__vv.set(height) plays the keyboard.
const VISUAL_VIEWPORT_STAND_IN = (WIDTH) => `(() => {
  const vv = new EventTarget();
  Object.assign(vv, { width: ${WIDTH}, height: 508, offsetTop: 0, offsetLeft: 0, pageTop: 0, pageLeft: 0, scale: 1 });
  Object.defineProperty(window, "visualViewport", { value: vv, configurable: true });
  window.__vv = { set(height, scale = 1) { vv.height = height; vv.scale = scale; vv.dispatchEvent(new Event("resize")); } };
})()`;
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
    width,
    height: 844,
    deviceScaleFactor: 2,
    mobile: true,
  });
  if (args.scenario === "keyboard") {
    await send("Page.addScriptToEvaluateOnNewDocument", { source: VISUAL_VIEWPORT_STAND_IN(width) });
  }
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
    result.boxHeightEmpty = await evaluate(`${box}.offsetHeight`);
    await send("Input.insertText", { text: "line one" });
    await press("Enter", SHIFT);
    await send("Input.insertText", { text: "line two" });
    await sleep(500);
    const held = await evaluate(`${box}.value`);
    expect("the box holds the two lines", held === "line one\nline two", held);
    expect("Shift+Enter makes no POST /replies", !posts.includes("/replies"), posts);
    result.boxHeightOfTwo = await evaluate(`${box}.offsetHeight`);
    expect("the box holds its 3-line height for two lines", result.boxHeightOfTwo === result.boxHeightEmpty, [result.boxHeightEmpty, result.boxHeightOfTwo]);
    const clipped = await evaluate(`${box}.scrollHeight > ${box}.clientHeight`);
    expect("two lines do not scroll", !clipped, clipped);
    await screenshot("2-two-lines");
    await press("Enter");
    result.tappedAt = await evaluate("Date.now()");
    await until("the thread to show both lines", `[...document.querySelectorAll("#messages .message .text")].some((p) => p.textContent === "line one\\nline two")`);
    expect("Enter makes one POST /replies", posts.filter((path) => path === "/replies").length === 1, posts);
    const sent = requests.filter((entry) => entry.endsWith(" /replies"));
    expect("the POST /replies is answered 2xx", sent.length === 1 && sent[0].startsWith("2"), sent);
    result.sent = sent;
    await until("the box to be empty and at its 3-line height", `${box}.value === "" && ${box}.offsetHeight === ${result.boxHeightEmpty}`);
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
  } else if (args.scenario === "keyboard") {
    // The stand-in starts 508 px tall in an 844 px window, so the page loads with the keyboard "open".
    const frame = () => evaluate(`new Promise((resolve) => requestAnimationFrame(() => resolve(true)))`);
    const measure = () =>
      evaluate(`(() => {
        const thread = document.getElementById("thread");
        return {
          replyBottom: document.getElementById("reply").getBoundingClientRect().bottom,
          headerTop: document.querySelector(".top").getBoundingClientRect().top,
          threadEnd: thread.scrollHeight - thread.scrollTop - thread.clientHeight,
          threadScrolls: thread.scrollHeight > thread.clientHeight,
          keyboardOpen: document.body.classList.contains("keyboard-open"),
          viewportHeight: document.documentElement.style.getPropertyValue("--viewport-height"),
        };
      })()`);
    // Enough rows to scroll, whatever the earlier scenarios left in the thread.
    await evaluate(`(() => {
      const thread = document.getElementById("thread");
      if (thread.scrollHeight > thread.clientHeight * 2) return;
      const filler = document.createElement("div");
      filler.style.height = "1500px";
      document.getElementById("messages").prepend(filler);
    })()`);
    await evaluate(`document.querySelector("#reply textarea").focus()`);
    await evaluate(`window.__vv.set(844)`);
    await frame();
    const closed = await measure();
    expect("closed: the reply bar ends at the window bottom", Math.abs(closed.replyBottom - 844) <= 1, closed);
    expect("closed: the page is not marked keyboard-open", !closed.keyboardOpen, closed);
    await screenshot("2-keyboard-closed");
    // The reader is at the end of the thread when the keyboard opens.
    await evaluate(`(() => { const thread = document.getElementById("thread"); thread.scrollTop = thread.scrollHeight; })()`);
    await sleep(300);
    result.atEndBefore = await measure();
    expect("the thread scrolls and sits at its end", result.atEndBefore.threadScrolls && result.atEndBefore.threadEnd <= 1, result.atEndBefore);
    await evaluate(`window.__vv.set(508)`);
    await frame();
    // The box keeps one height with 0, 1 and 10 lines typed, scrolls inside, and the bar stays at the keyboard.
    const box = `document.querySelector("#reply textarea")`;
    // The numbers of the reply box: the .reply-box's height, the textarea's scroll state, the space
    // between the border box and the text on both edges, and how far the text is from its end.
    const boxNow = () =>
      evaluate(`(() => {
        const b = ${box};
        const wrap = b.closest(".reply-box").getBoundingClientRect();
        const text = b.getBoundingClientRect();
        const line = parseFloat(getComputedStyle(b).lineHeight);
        const fromEnd = b.scrollHeight - b.clientHeight - b.scrollTop;
        return {
          height: b.closest(".reply-box").offsetHeight,
          scrollHeight: b.scrollHeight,
          clientHeight: b.clientHeight,
          lineHeight: line,
          fromEnd,
          linesFromEnd: fromEnd / line,
          edgeTop: text.top - wrap.top,
          edgeBottom: wrap.bottom - text.bottom,
          replyBottom: document.getElementById("reply").getBoundingClientRect().bottom,
        };
      })()`);
    // A tap in the space around the text, inside the border, focuses the text box: the top band
    // (the box's top + 5 px, at its middle) and the left band (its left + 6 px, at its middle).
    const tapBand = async (name, where) => {
      await evaluate(`${box}.blur()`);
      const point = await evaluate(`(() => {
        const wrap = ${box}.closest(".reply-box").getBoundingClientRect();
        return ${where};
      })()`);
      const before = await evaluate(`document.activeElement === ${box}`);
      await tap(point.x, point.y);
      await sleep(100);
      const after = await evaluate(`document.activeElement === ${box}`);
      result[name] = { point, focusedBefore: before, focusedAfter: after };
      expect(`a tap in the ${name} focuses the text box`, !before && after, result[name]);
    };
    await tapBand("topBand", `({ x: wrap.left + wrap.width / 2, y: wrap.top + 5 })`);
    await tapBand("leftBand", `({ x: wrap.left + 6, y: wrap.top + wrap.height / 2 })`);
    const fill = async (text) => {
      await evaluate(`(() => { const b = ${box}; b.focus(); b.value = ${JSON.stringify(text)}; b.dispatchEvent(new Event("input", { bubbles: true })); })()`);
      await sleep(300);
    };
    const wholeLines = (shown) => Math.abs(shown.linesFromEnd - Math.round(shown.linesFromEnd)) < 0.02;
    const tenLines = Array.from({ length: 10 }, (_, i) => "line " + (i + 1)).join("\n");
    result.box0 = await boxNow();
    await fill("one line");
    result.box1 = await boxNow();
    // Typed as a person types: Shift+Enter between lines, so the browser keeps the caret in view.
    await fill("");
    await evaluate(`${box}.focus()`);
    for (let i = 1; i <= 10; i++) {
      if (i > 1) await press("Enter", SHIFT);
      await send("Input.insertText", { text: "line " + i });
    }
    await sleep(300);
    result.box10 = await boxNow();
    expect("the box is the same height with 1 line typed", result.box1.height === result.box0.height, [result.box0, result.box1]);
    expect("the box is the same height with 10 lines typed", result.box10.height === result.box0.height, [result.box0, result.box10]);
    expect("with 10 lines the box scrolls inside", result.box10.scrollHeight > result.box10.clientHeight, result.box10);
    expect("with 10 lines the reply bar still ends at the keyboard top", Math.abs(result.box10.replyBottom - 508) <= 1, result.box10);
    expect("with 10 lines typed the newest line is in view", result.box10.fromEnd < 1, result.box10);
    expect("with 10 lines typed the text is a whole number of lines from its end", wholeLines(result.box10), result.box10);
    expect("the edge space is the same with 0 and 10 lines", Math.abs(result.box10.edgeTop - result.box0.edgeTop) < 0.5 && Math.abs(result.box10.edgeBottom - result.box0.edgeBottom) < 0.5, [result.box0, result.box10]);
    // Dictation writes box.value and fires "input": the last of 10 lines must show too.
    await fill("");
    await fill(tenLines);
    result.dictated = await boxNow();
    expect("with 10 lines written as dictation writes them the last line is in view", result.dictated.fromEnd < 1 && wholeLines(result.dictated), result.dictated);
    expect("the edge space is the same after a dictated write", Math.abs(result.dictated.edgeTop - result.box0.edgeTop) < 0.5 && Math.abs(result.dictated.edgeBottom - result.box0.edgeBottom) < 0.5, [result.box0, result.dictated]);
    // The same box in both colour schemes, empty and with 10 lines.
    for (const scheme of ["light", "dark"]) {
      await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: scheme }] });
      await fill("");
      await screenshot(`3a-box-${scheme}-empty`);
      await fill(tenLines);
      await screenshot(`3b-box-${scheme}-ten-lines`);
    }
    await send("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: "light" }] });
    await fill("");
    result.open = await measure();
    expect("open: the reply bar ends at the keyboard top, 508", Math.abs(result.open.replyBottom - 508) <= 1, result.open);
    expect("open: the header stays at the top", Math.abs(result.open.headerTop) <= 1, result.open);
    expect("open: the thread is still at its end", result.open.threadEnd <= 1, result.open);
    expect("open: the page is marked keyboard-open", result.open.keyboardOpen, result.open);
    await screenshot("3-keyboard-open");
    // A pinch zoom shortens the visual viewport too, but it is no keyboard: nothing moves.
    await evaluate(`window.__vv.set(422, 2)`);
    await frame();
    result.zoomed = await measure();
    expect("zoomed: the reply bar stays where the keyboard put it", Math.abs(result.zoomed.replyBottom - 508) <= 1, result.zoomed);
    await evaluate(`window.__vv.set(508, 1)`);
    await frame();
    await evaluate(`window.__vv.set(844)`);
    await frame();
    result.closed = await measure();
    expect("closed again: the reply bar ends at 844", Math.abs(result.closed.replyBottom - 844) <= 1, result.closed);
    expect("closed again: no keyboard-open", !result.closed.keyboardOpen, result.closed);
    await screenshot("4-keyboard-closed-again");
    result.ok = true;
    fs.writeFileSync(args.out, JSON.stringify(result, null, 2));
    await finish(0);
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
