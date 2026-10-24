// Records what the web app does in this browser, as OpenTelemetry spans, and posts them in
// batches to Harold at /telemetry/traces. Harold keeps the fields it allows and forwards
// them to the collector. Nothing here records text the user typed.
//
// The library is one vendored file, built by scripts/build_web_otel.sh. Its name changes
// whenever its bytes do; keep this import and OTEL_JS in src/web_app/pages.rs the same.
import {
  ATTR_SERVICE_NAME,
  BatchSpanProcessor,
  DocumentLoadInstrumentation,
  FetchInstrumentation,
  OTLPTraceExporter,
  SpanStatusCode,
  StackContextManager,
  TracerProvider,
  W3CTraceContextPropagator,
  context,
  propagation,
  registerInstrumentations,
  resourceFromAttributes,
  trace,
} from "/static/otel-2.12.0-8513c042.js";

const SERVICE = "harold-web-app";
const SEND_EVERY_MS = 5000;
const MESSAGE_CHARS = 200;

// Requests that would be noise, or never end: the update stream, a call's audio pieces, the
// page's own beats, and this script's own posts.
const NOT_TRACED = [
  /\/updates(?:[?#]|$)/,
  /\/call\/audio(?:[?#]|$)/,
  /\/page\/seen(?:[?#]|$)/,
  /\/page\/hidden(?:[?#]|$)/,
  /\/telemetry\/traces(?:[?#]|$)/,
];

/** `address` as its path alone: no host, query or fragment. */
function pathOnly(address) {
  try {
    return new URL(address, location.href).pathname;
  } catch {
    return "";
  }
}

function meta(name) {
  return document.querySelector(`meta[name="${name}"]`)?.content ?? "";
}

function start() {
  const provider = new TracerProvider({
    resource: resourceFromAttributes({
      [ATTR_SERVICE_NAME]: SERVICE,
      "service.version": meta("harold-version"),
    }),
    // The processor also sends what it holds when the page is hidden: the page may go away
    // at any moment on a phone.
    spanProcessors: [
      new BatchSpanProcessor({
        exporter: new OTLPTraceExporter({ url: `${location.origin}/telemetry/traces` }),
        scheduledDelayMillis: SEND_EVERY_MS,
      }),
    ],
  });
  const contextManager = new StackContextManager();
  contextManager.enable();
  context.setGlobalContextManager(contextManager);
  propagation.setGlobalPropagator(new W3CTraceContextPropagator());
  trace.setGlobalTracerProvider(provider);

  registerInstrumentations({
    instrumentations: [
      new DocumentLoadInstrumentation({
        applyCustomAttributesOnSpan: {
          resourceFetch: (span, resource) => {
            span.setAttribute("url.full", pathOnly(resource.name));
          },
        },
      }),
      new FetchInstrumentation({
        ignoreUrls: NOT_TRACED,
        // The span's address is cut to its path: Datastar's @get puts signals in the query.
        // (A request that failed has no response: Harold cuts that address too.)
        applyCustomAttributesOnSpan: (span, request, result) => {
          const address = request?.url ?? result?.url ?? "";
          if (address) span.setAttribute("url.full", pathOnly(address));
        },
      }),
    ],
  });

  const tracer = trace.getTracer(SERVICE);
  recordErrors(tracer);
  recordRowsDrawn(tracer);
  recordThreadOpened(tracer);
}

/** A script error or a promise nobody caught, as a `browser.error` span. */
function recordErrors(tracer) {
  const report = (kind, type, message, filename, line, column) => {
    const attributes = {
      "error.kind": kind,
      "exception.type": type || "Error",
      "exception.message": String(message ?? "").slice(0, MESSAGE_CHARS),
    };
    if (filename) attributes["code.filepath"] = pathOnly(filename);
    if (Number.isInteger(line)) attributes["code.lineno"] = line;
    if (Number.isInteger(column)) attributes["code.column"] = column;
    const span = tracer.startSpan("browser.error", { attributes });
    span.setStatus({ code: SpanStatusCode.ERROR });
    span.end();
  };
  addEventListener("error", (event) => {
    if (event.target && event.target !== window) return;
    report(
      "error",
      event.error?.name,
      event.error?.message ?? event.message,
      event.filename,
      event.lineno,
      event.colno,
    );
  });
  addEventListener("unhandledrejection", (event) => {
    const reason = event.reason;
    report("unhandledrejection", reason?.name, reason?.message, "", undefined, undefined);
  });
}

/**
 * A row added to the end of #messages after the page loaded, carrying `data-trace`, as a
 * `browser.message_shown` span under that trace, ending on the next animation frame.
 * Rows drawn with the page, and older rows added above the first, are left out.
 */
function recordRowsDrawn(tracer) {
  const messages = document.getElementById("messages");
  if (!messages) return;
  const shown = new Set();
  const draw = (row) => {
    const traceparent = row.getAttribute("data-trace");
    if (!traceparent || shown.has(row.id)) return;
    shown.add(row.id);
    const parent = propagation.extract(context.active(), { traceparent });
    const span = tracer.startSpan(
      "browser.message_shown",
      { attributes: { "harold.row": row.classList.contains("chief") ? "chief" : "me" } },
      parent,
    );
    requestAnimationFrame(() => span.end());
  };
  new MutationObserver((records) => {
    for (const record of records) {
      if (record.nextSibling) continue;
      for (const node of record.addedNodes) {
        if (node.nodeType === Node.ELEMENT_NODE && node.classList.contains("message")) {
          draw(node);
        }
      }
    }
  }).observe(messages, { childList: true });
}

/**
 * Where the thread opened, as a `browser.thread_opened` span: pages.js announces each
 * placement with a `harold:thread-opened` event holding `openedAt` (`first_unread`, `newest`
 * or `oldest_on_page`) and `unread`, the count of rows after the read mark that are not mine.
 */
function recordThreadOpened(tracer) {
  document.addEventListener("harold:thread-opened", (event) => {
    const { openedAt, unread } = event.detail ?? {};
    if (!Number.isInteger(unread)) return;
    const span = tracer.startSpan("browser.thread_opened", {
      attributes: { "harold.opened_at": String(openedAt), "harold.unread": unread },
    });
    span.end();
  });
}

try {
  start();
} catch (error) {
  // Recording must never stop the app; the console says why it did not start.
  console.error("telemetry.js did not start:", error?.message);
}
