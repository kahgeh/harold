import assert from "node:assert/strict";
import test from "node:test";

import {
  buildGrpcurlInvocation,
  createHaroldPlugin,
  lifecycleUpdate,
  normalizePrompt,
} from "./harold-plugin-core.mjs";
import * as discoveredPlugin from "./harold-plugin.js";

const textPart = (text, extra = {}) => ({ type: "text", text, ...extra });

test("the discovered module exports exactly one plugin function", () => {
  assert.deepEqual(Object.keys(discoveredPlugin), ["HaroldAgentState"]);
  assert.equal(typeof discoveredPlugin.HaroldAgentState, "function");
});

test("normalizePrompt selects the latest substantive submitted TextPart", () => {
  const summary = normalizePrompt([
    textPart("  Investigate\n the dashboard  "),
    { type: "file", filename: "notes.txt" },
    textPart("and explain the idle transition"),
    textPart("not user input", { synthetic: true }),
    textPart("ignored", { ignored: true }),
    textPart("  "),
    textPart("Ask OpenCode to do anything"),
  ]);

  assert.equal(summary, "and explain the idle transition");
  assert.equal(normalizePrompt([textPart("x".repeat(200))]).length, 160);
  assert.equal(normalizePrompt([textPart("Ask OpenCode to do anything")]), "");
});

test("normalizePrompt removes complete terminal control sequences", () => {
  const hiddenPayload = "PRIVATE_CONTROL_PAYLOAD";
  const prompt = [
    "\x1b[31mReview\x1b[0m",
    `\x1b]0;${hiddenPayload}\x07`,
    `\x1b]8;;https://example.invalid/${hiddenPayload}\x1b\\projector\x1b]8;;\x1b\\`,
    "\u009b32msafely\u009b0m",
    `\u009d0;${hiddenPayload}\u009c`,
    `\u0090${hiddenPayload}\u009c`,
    `\u0098${hiddenPayload}\u009c`,
    `\u009e${hiddenPayload}\u009c`,
    `\u009f${hiddenPayload}\u009c`,
  ].join(" ");

  const summary = normalizePrompt([textPart(prompt)]);

  assert.equal(summary === "Review projector safely", true);
  assert.equal(summary.includes(hiddenPayload), false);
});

test("lifecycleUpdate maps status and legacy idle events", () => {
  assert.deepEqual(
    lifecycleUpdate({
      type: "session.status",
      properties: { sessionID: "one", status: { type: "busy" } },
    }),
    { sessionID: "one", state: "AGENT_STATE_BUSY" },
  );
  assert.deepEqual(
    lifecycleUpdate({
      type: "session.status",
      properties: { sessionID: "one", status: { type: "retry" } },
    }),
    { sessionID: "one", state: "AGENT_STATE_BUSY" },
  );
  assert.deepEqual(
    lifecycleUpdate({
      type: "session.status",
      properties: { sessionID: "one", status: { type: "idle" } },
    }),
    { sessionID: "one", state: "AGENT_STATE_IDLE" },
  );
  assert.deepEqual(
    lifecycleUpdate({
      type: "session.idle",
      properties: { sessionID: "one" },
    }),
    { sessionID: "one", state: "AGENT_STATE_IDLE" },
  );
  assert.equal(lifecycleUpdate({ type: "file.edited", properties: {} }), null);
});

test("grpcurl invocation uses safe argv and sends JSON through stdin", () => {
  const maliciousAddress = "localhost:50060; touch /tmp/not-run";
  const invocation = buildGrpcurlInvocation(
    {
      paneId: "%35",
      state: "AGENT_STATE_BUSY",
      adapterId: "opencode.lifecycle",
      workSummary: "inspect 'quotes' and $(touch /tmp/not-run)",
    },
    {
      address: maliciousAddress,
      protoPath: "/tmp/a proto/harold.proto",
    },
  );

  assert.equal(invocation.command, "grpcurl");
  assert.equal(invocation.options.shell, false);
  assert.equal(invocation.args.at(-2), maliciousAddress);
  assert.equal(invocation.args.at(-1), "harold.Harold/ReportAgentState");
  assert.equal(invocation.args[invocation.args.indexOf("-d") + 1], "@");
  assert.equal(invocation.args.join(" ").includes("inspect 'quotes'"), false);
  assert.equal(invocation.input, JSON.stringify({
    paneId: "%35",
    state: "AGENT_STATE_BUSY",
    adapterId: "opencode.lifecycle",
    workSummary: "inspect 'quotes' and $(touch /tmp/not-run)",
  }));
});

test("chat message reports BUSY with the actual prompt", async () => {
  const calls = [];
  const plugin = await createHaroldPlugin({
    env: {
      TMUX_PANE: "%35",
      HAROLD_ADDR: "127.0.0.1:55060",
      HAROLD_PROTO: "/tmp/harold.proto",
    },
    dispatch: (invocation) => calls.push(invocation),
  })({});

  await plugin["chat.message"](
    { sessionID: "session-1" },
    { parts: [textPart("Find why the dashboard summary is stale")] },
  );

  assert.equal(calls.length, 1);
  assert.deepEqual(JSON.parse(calls[0].input), {
    paneId: "%35",
    state: "AGENT_STATE_BUSY",
    adapterId: "opencode.lifecycle",
    workSummary: "Find why the dashboard summary is stale",
  });
});

test("idle event omits workSummary and preserves a busy sibling session", async () => {
  const calls = [];
  const plugin = await createHaroldPlugin({
    env: { TMUX_PANE: "%35" },
    protoPath: "/tmp/harold.proto",
    dispatch: (invocation) => calls.push(invocation),
  })({});

  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "one", status: { type: "busy" } },
    },
  });
  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "two", status: { type: "busy" } },
    },
  });
  await plugin.event({
    event: { type: "session.idle", properties: { sessionID: "one" } },
  });
  await plugin.event({
    event: { type: "session.idle", properties: { sessionID: "two" } },
  });

  assert.equal(calls.length, 2, "duplicate BUSY and premature IDLE are suppressed");
  const idle = JSON.parse(calls[1].input);
  assert.equal(idle.state, "AGENT_STATE_IDLE");
  assert.equal(Object.hasOwn(idle, "workSummary"), false);
});

test("a placeholder submission reports BUSY without replacing the summary", async () => {
  const calls = [];
  const plugin = await createHaroldPlugin({
    env: { TMUX_PANE: "%35" },
    protoPath: "/tmp/harold.proto",
    dispatch: (invocation) => calls.push(invocation),
  })({});

  await plugin["chat.message"](
    { sessionID: "session-1" },
    { parts: [textPart("Ask OpenCode to do anything")] },
  );

  const busy = JSON.parse(calls[0].input);
  assert.equal(busy.state, "AGENT_STATE_BUSY");
  assert.equal(Object.hasOwn(busy, "workSummary"), false);
});

test("missing pane context is a silent no-op", async () => {
  const calls = [];
  const plugin = await createHaroldPlugin({
    env: {},
    protoPath: "/tmp/harold.proto",
    dispatch: (invocation) => calls.push(invocation),
  })({});

  await plugin["chat.message"](
    { sessionID: "session-1" },
    { parts: [textPart("This must not be reported")] },
  );
  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "session-1", status: { type: "busy" } },
    },
  });

  assert.deepEqual(calls, []);
});

test("an empty HAROLD_PROTO falls back to the repository contract", async () => {
  const calls = [];
  const plugin = await createHaroldPlugin({
    env: { TMUX_PANE: "%35", HAROLD_PROTO: "   " },
    dispatch: (invocation) => calls.push(invocation),
  })({});

  await plugin["chat.message"](
    { sessionID: "session-1" },
    { parts: [textPart("Use the repository proto fallback")] },
  );

  assert.equal(calls.length, 1);
  assert.equal(calls[0].args[calls[0].args.indexOf("-proto") + 1], "harold.proto");
  assert.match(
    calls[0].args[calls[0].args.indexOf("-import-path") + 1],
    /harold-api\/proto$/,
  );
});

test("dispatch failures do not reject an OpenCode hook", async () => {
  const plugin = await createHaroldPlugin({
    env: { TMUX_PANE: "%35" },
    protoPath: "/tmp/harold.proto",
    dispatch: () => {
      throw new Error("grpcurl unavailable");
    },
  })({});

  await assert.doesNotReject(() =>
    plugin["chat.message"](
      { sessionID: "session-1" },
      { parts: [textPart("Keep OpenCode usable")] },
    ),
  );
});

test("OpenCode hooks do not wait for a pending grpcurl delivery", async () => {
  let deliveryStarted = false;
  const plugin = await createHaroldPlugin({
    env: { TMUX_PANE: "%35" },
    protoPath: "/tmp/harold.proto",
    dispatch: () => {
      deliveryStarted = true;
      return new Promise(() => {});
    },
  })({});

  await plugin["chat.message"](
    { sessionID: "session-1" },
    { parts: [textPart("Do not block this agent")] },
  );

  assert.equal(deliveryStarted, true);
});
