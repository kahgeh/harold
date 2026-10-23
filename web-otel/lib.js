// The names harold/static/web_app/telemetry.js uses, and nothing else. esbuild keeps
// only what these reach.
export { context, propagation, SpanStatusCode, trace } from "@opentelemetry/api";
export { W3CTraceContextPropagator } from "@opentelemetry/core";
export { OTLPTraceExporter } from "@opentelemetry/exporter-trace-otlp-http";
export { registerInstrumentations } from "@opentelemetry/instrumentation";
export { DocumentLoadInstrumentation } from "@opentelemetry/instrumentation-document-load";
export { FetchInstrumentation } from "@opentelemetry/instrumentation-fetch";
export { resourceFromAttributes } from "@opentelemetry/resources";
export { BatchSpanProcessor, StackContextManager, TracerProvider } from "@opentelemetry/sdk-trace";
export { ATTR_SERVICE_NAME } from "@opentelemetry/semantic-conventions";
