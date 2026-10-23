#!/usr/bin/env node
// A stand-in for the local collector, for scripts/otel_test.py. It takes what a test Harold
// exports over OTLP gRPC (spans, as protobuf, on --grpc) and what Harold forwards from the
// phone's browser over OTLP HTTP (spans, as JSON, on --http), and appends one JSON line for
// each span to --out: {source, traceId, spanId, parentSpanId, name, startNs, endNs, attributes}.
// Logs and metrics are accepted and dropped. Plain Node, no packages. It stops on SIGTERM.

import fs from "node:fs";
import http from "node:http";
import http2 from "node:http2";

const args = Object.fromEntries(
  process.argv.slice(2).reduce((pairs, arg, i, all) => {
    if (arg.startsWith("--")) pairs.push([arg.slice(2), all[i + 1]]);
    return pairs;
  }, []),
);
const out = fs.openSync(args.out, "a");

function write(record) {
  fs.writeSync(out, JSON.stringify(record) + "\n");
}

// --- a protobuf reader, just enough for ExportTraceServiceRequest -----------------------

function varint(buffer, at) {
  let value = 0n;
  let shift = 0n;
  for (;;) {
    const byte = buffer[at++];
    value |= BigInt(byte & 0x7f) << shift;
    if (!(byte & 0x80)) return [value, at];
    shift += 7n;
  }
}

/** Every field of one message: [{number, wire, value}], value a BigInt or a Buffer. */
function fields(buffer) {
  const found = [];
  let at = 0;
  while (at < buffer.length) {
    let key;
    [key, at] = varint(buffer, at);
    const number = Number(key >> 3n);
    const wire = Number(key & 7n);
    if (wire === 0) {
      let value;
      [value, at] = varint(buffer, at);
      found.push({ number, wire, value });
    } else if (wire === 1) {
      found.push({ number, wire, value: buffer.readBigUInt64LE(at) });
      at += 8;
    } else if (wire === 2) {
      let length;
      [length, at] = varint(buffer, at);
      found.push({ number, wire, value: buffer.subarray(at, at + Number(length)) });
      at += Number(length);
    } else if (wire === 5) {
      found.push({ number, wire, value: BigInt(buffer.readUInt32LE(at)) });
      at += 4;
    } else {
      throw new Error(`wire type ${wire}`);
    }
  }
  return found;
}

const only = (list, number) => list.filter((field) => field.number === number);
const first = (list, number) => only(list, number)[0]?.value;
const text = (buffer) => (buffer ? buffer.toString("utf8") : "");

function attributes(list) {
  const result = {};
  for (const { value } of only(list, 9)) {
    const pair = fields(value);
    const any = fields(first(pair, 2) ?? Buffer.alloc(0));
    const stringValue = first(any, 1);
    const intValue = first(any, 3);
    result[text(first(pair, 1))] =
      stringValue !== undefined ? text(stringValue) : intValue !== undefined ? Number(intValue) : "";
  }
  return result;
}

function spansOfGrpc(body) {
  for (const { value: resourceSpans } of only(fields(body), 1)) {
    const resource = fields(resourceSpans);
    for (const { value: scopeSpans } of only(resource, 2)) {
      for (const { value: spanBytes } of only(fields(scopeSpans), 2)) {
        const span = fields(spanBytes);
        write({
          source: "harold",
          traceId: first(span, 1).toString("hex"),
          spanId: first(span, 2).toString("hex"),
          parentSpanId: (first(span, 4) ?? Buffer.alloc(0)).toString("hex"),
          name: text(first(span, 5)),
          startNs: String(first(span, 7)),
          endNs: String(first(span, 8)),
          attributes: attributes(span),
          links: only(span, 13).map(({ value }) => {
            const link = fields(value);
            return { traceId: first(link, 1).toString("hex"), spanId: first(link, 2).toString("hex") };
          }),
        });
      }
    }
  }
}

// --- OTLP gRPC on --grpc: HTTP/2 without TLS ------------------------------------------------

const grpc = http2.createServer();
grpc.on("stream", (stream, headers) => {
  const chunks = [];
  stream.on("data", (chunk) => chunks.push(chunk));
  stream.on("end", () => {
    const body = Buffer.concat(chunks);
    if (headers[":path"] === "/opentelemetry.proto.collector.trace.v1.TraceService/Export") {
      try {
        if (body.length >= 5 && body[0] === 0) spansOfGrpc(body.subarray(5, 5 + body.readUInt32BE(1)));
      } catch (error) {
        console.error("could not read a gRPC batch:", error.message);
      }
    }
    stream.respond({ ":status": 200, "content-type": "application/grpc" }, { waitForTrailers: true });
    stream.on("wantTrailers", () => stream.sendTrailers({ "grpc-status": "0" }));
    stream.end(Buffer.from([0, 0, 0, 0, 0]));
  });
});
grpc.listen(Number(args.grpc), "127.0.0.1");

// --- OTLP JSON on --http: the browser's spans, as Harold forwards them ----------------------

const attributeValue = (value = {}) => value.stringValue ?? (value.intValue !== undefined ? Number(value.intValue) : "");

const web = http.createServer((request, response) => {
  const chunks = [];
  request.on("data", (chunk) => chunks.push(chunk));
  request.on("end", () => {
    if (request.method === "POST" && request.url === "/v1/traces") {
      try {
        const batch = JSON.parse(Buffer.concat(chunks).toString("utf8"));
        for (const resource of batch.resourceSpans ?? []) {
          const service = Object.fromEntries(
            (resource.resource?.attributes ?? []).map(({ key, value }) => [key, attributeValue(value)]),
          );
          for (const scope of resource.scopeSpans ?? []) {
            for (const span of scope.spans ?? []) {
              write({
                source: "browser",
                service: service["service.name"],
                scope: scope.scope?.name,
                traceId: span.traceId,
                spanId: span.spanId,
                parentSpanId: span.parentSpanId ?? "",
                name: span.name,
                startNs: span.startTimeUnixNano,
                endNs: span.endTimeUnixNano,
                attributes: Object.fromEntries(
                  (span.attributes ?? []).map(({ key, value }) => [key, attributeValue(value)]),
                ),
                events: (span.events ?? []).map((event) => event.name),
              });
            }
          }
        }
      } catch (error) {
        console.error("could not read a JSON batch:", error.message);
      }
    }
    response.writeHead(200, { "content-type": "application/json" });
    response.end("{}");
  });
});
web.listen(Number(args.http), "127.0.0.1");

process.on("SIGTERM", () => process.exit(0));
console.log("collector stand-in ready");
