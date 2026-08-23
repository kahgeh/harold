import { spawn } from "node:child_process";
import { basename, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const DEFAULT_ADDRESS = "localhost:50060";
const ADAPTER_ID = "opencode.lifecycle";
const BUSY = "AGENT_STATE_BUSY";
const IDLE = "AGENT_STATE_IDLE";
const MAX_SUMMARY_SCALARS = 160;
const REPOSITORY_PROTO = fileURLToPath(
  new URL("../../harold-api/proto/harold.proto", import.meta.url),
);
const UI_PLACEHOLDER = /^(?:ask (?:codex|claude|opencode) to do anything|ask anything(?:\.\.\.)?(?:\s+".*")?)$/iu;

export function normalizePrompt(parts) {
  if (!Array.isArray(parts)) return "";

  for (const part of parts.toReversed()) {
    if (
      part?.type !== "text" ||
      typeof part.text !== "string" ||
      part.synthetic ||
      part.ignored
    ) {
      continue;
    }

    const candidate = part.text
      .replace(/[\u0000-\u001f\u007f-\u009f]/gu, " ")
      .replace(/\s+/gu, " ")
      .trim();
    if (!candidate || UI_PLACEHOLDER.test(candidate)) continue;

    return Array.from(candidate).slice(0, MAX_SUMMARY_SCALARS).join("");
  }
  return "";
}

export function lifecycleUpdate(event) {
  const sessionID = event?.properties?.sessionID;
  if (typeof sessionID !== "string" || sessionID.length === 0) return null;

  if (event.type === "session.idle") {
    return { sessionID, state: IDLE };
  }
  if (event.type !== "session.status") return null;

  const status = event.properties?.status?.type;
  if (status === "busy" || status === "retry") {
    return { sessionID, state: BUSY };
  }
  if (status === "idle") {
    return { sessionID, state: IDLE };
  }
  return null;
}

export function buildGrpcurlInvocation(payload, config) {
  const protoPath = config.protoPath;
  return {
    command: "grpcurl",
    args: [
      "-plaintext",
      "-max-time",
      "3",
      "-import-path",
      dirname(protoPath),
      "-proto",
      basename(protoPath),
      "-d",
      "@",
      config.address,
      "harold.Harold/ReportAgentState",
    ],
    input: JSON.stringify(payload),
    options: {
      shell: false,
      stdio: ["pipe", "ignore", "ignore"],
      timeout: 4_000,
      windowsHide: true,
    },
  };
}

function spawnGrpcurl(invocation, spawnProcess = spawn) {
  return new Promise((resolve) => {
    let child;
    try {
      child = spawnProcess(
        invocation.command,
        invocation.args,
        invocation.options,
      );
    } catch {
      resolve();
      return;
    }

    child.once("error", resolve);
    child.once("close", resolve);
    child.stdin.once("error", resolve);
    child.stdin.end(invocation.input);
  });
}

export function createHaroldPlugin(overrides = {}) {
  const env = overrides.env ?? process.env;
  const paneId = env.TMUX_PANE?.trim() ?? "";
  const config = {
    address: env.HAROLD_ADDR?.trim() || DEFAULT_ADDRESS,
    protoPath:
      overrides.protoPath ?? (env.HAROLD_PROTO?.trim() || REPOSITORY_PROTO),
  };
  const dispatch =
    overrides.dispatch ??
    ((invocation) => spawnGrpcurl(invocation, overrides.spawnProcess));

  return async function HaroldOpenCodePlugin() {
    const busySessions = new Set();
    let lastState;
    let deliveryQueue = Promise.resolve();

    const report = (state, workSummary) => {
      if (!paneId) return;

      const payload = {
        paneId,
        state,
        adapterId: ADAPTER_ID,
      };
      if (workSummary) payload.workSummary = workSummary;

      const invocation = buildGrpcurlInvocation(payload, config);
      deliveryQueue = deliveryQueue
        .then(() => dispatch(invocation))
        .catch(() => undefined);
      lastState = state;
    };

    return {
      "chat.message": async (input, output) => {
        const sessionID = input?.sessionID;
        if (!paneId || typeof sessionID !== "string" || !sessionID) return;

        const summary = normalizePrompt(output?.parts);
        busySessions.add(sessionID);
        if (summary || lastState !== BUSY) report(BUSY, summary);
      },

      event: async ({ event } = {}) => {
        if (!paneId) return;

        const update = lifecycleUpdate(event);
        if (!update) return;

        if (update.state === BUSY) {
          busySessions.add(update.sessionID);
          if (lastState !== BUSY) report(BUSY);
          return;
        }

        busySessions.delete(update.sessionID);
        if (busySessions.size === 0 && lastState !== IDLE) report(IDLE);
      },
    };
  };
}
