//! Harold's logs and telemetry. `harold.log` is written as it always was. When
//! `[telemetry] otlp_endpoint` is set, spans and log records are also exported to an
//! OpenTelemetry collector over OTLP, carrying only the fields on `ALLOWED_FIELDS`.
//!
//! Export is queued in memory and sent in batches by background threads, so a slow or
//! missing collector never slows Harold down. An unreachable collector is reported in
//! `harold.log` once when exports start failing and once when they recover.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{
    BatchLogProcessor, LogBatch, LogExporter, SdkLogRecord, SdkLogger, SdkLoggerProvider,
    SimpleLogProcessor,
};
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use opentelemetry_sdk::trace::{
    BatchSpanProcessor, SdkTracerProvider, SimpleSpanProcessor, SpanData, SpanEvents, SpanExporter,
    SpanLinks,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{Level, Metadata, Subscriber, info, warn};
use tracing_subscriber::filter::{EnvFilter, FilterExt, FilterFn, LevelFilter, filter_fn};
use tracing_subscriber::layer::{Filter, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, fmt};

use crate::settings::TelemetrySettings;

// ---------------------------------------------------------------------------
// Targets and allowed fields
// ---------------------------------------------------------------------------

/// The target of every `tracing` span that is exported as an OpenTelemetry span. These
/// spans never appear as prefixes in `harold.log`. Make them `info` spans: the export
/// layer's level hint is `INFO`, so a `debug` or `trace` span is dropped when the log
/// level is `info`.
pub(crate) const TRACE_TARGET: &str = "harold::trace";

/// The target of the two collector-health lines. They go to `harold.log` only.
pub(crate) const HEALTH_TARGET: &str = "harold::telemetry_health";

/// The only span attributes and log-record attributes that leave Harold. Everything else,
/// such as `text`, `msg`, `raw_output`, `tag` and `error`, is dropped before export.
pub(crate) const ALLOWED_FIELDS: &[&str] = &[
    "pane_id",
    "pane",
    "session.id",
    "session.source",
    "component",
    "reason",
    "outcome",
    "channel",
    "resolved_by",
    "kind",
    "started_by",
    "turn.start",
    "state",
    "alert",
    "pod.product",
    "spec.id",
    "spec.state_at",
    "spec.words",
    "spec.revisions",
    "phase",
    "change",
    "event_type",
    "event_version",
    "result",
];

/// How long shutting the exporters down may take in all.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

/// How long one send to the collector may take before it gives up.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often metrics are sent.
const METRIC_INTERVAL: Duration = Duration::from_secs(30);

/// The instrumentation scope of Harold's spans, logs and metrics.
const SCOPE: &str = "harold";

fn is_allowed(key: &str) -> bool {
    ALLOWED_FIELDS.contains(&key)
}

/// Whether a log event with `target` is exported. Only Harold's and the `events`
/// crate's own events are: a target that is `harold` or starts with `harold::`, or is
/// `events` or starts with `events::`, apart from the health lines. Other crates, such
/// as turso and reqwest, interpolate errors and paths into their messages, so their
/// events (including `log` records the LogTracer bridges) stay in `harold.log` only.
fn is_exported_log_target(target: &str) -> bool {
    let ours = |name: &str| {
        target == name
            || target
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with("::"))
    };
    target != HEALTH_TARGET && (ours("harold") || ours("events"))
}

// ---------------------------------------------------------------------------
// Local time
// ---------------------------------------------------------------------------

static LOCAL_OFFSET: OnceLock<time::UtcOffset> = OnceLock::new();

/// Records the Mac's UTC offset. `main` reads it before the tokio runtime starts any
/// threads, because the `time` crate refuses to read it once threads exist.
pub(crate) fn set_local_offset(offset: time::UtcOffset) {
    let _ = LOCAL_OFFSET.set(offset);
}

/// The Mac's UTC offset as `main` read it at start, or UTC when it was not read.
#[allow(
    dead_code,
    reason = "the pod facts read the local offset in a later telemetry task"
)]
pub(crate) fn local_offset() -> time::UtcOffset {
    LOCAL_OFFSET.get().copied().unwrap_or(time::UtcOffset::UTC)
}

// ---------------------------------------------------------------------------
// Global access
// ---------------------------------------------------------------------------

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Harold's tracer, for spans that start in the past. It is a no-op tracer when
/// telemetry is off.
#[allow(
    dead_code,
    reason = "the phone-request and turn spans use the tracer in later telemetry tasks"
)]
pub(crate) fn tracer() -> BoxedTracer {
    #[cfg(test)]
    test_support::install();
    global::tracer(SCOPE)
}

/// Whether spans, logs and metrics are being exported.
#[allow(
    dead_code,
    reason = "later telemetry tasks skip telemetry-only work when export is off"
)]
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    test_support::install();
    ENABLED.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Instruments
// ---------------------------------------------------------------------------

/// Harold's counters and histograms, with the names, units and buckets of the spec's
/// Metrics table. The four pod gauges are observable gauges that `pod_stats` registers,
/// because it owns their data.
#[allow(
    dead_code,
    reason = "the phone-request, turn and pod tasks record into the instruments later"
)]
pub(crate) struct Instruments {
    /// `harold.phone_requests`, `{request}`.
    pub phone_requests: Counter<u64>,
    /// `harold.phone_requests.wrong`, `{request}`.
    pub phone_requests_wrong: Counter<u64>,
    /// `harold.phone_request.latency`, seconds.
    pub phone_request_latency: Histogram<f64>,
    /// `harold.turns`, `{turn}`.
    pub turns: Counter<u64>,
    /// `harold.turn.duration`, seconds.
    pub turn_duration: Histogram<f64>,
    /// `pod.merges`, `{merge}`.
    pub pod_merges: Counter<u64>,
    /// `pod.merged_lines`, `{line}`.
    pub pod_merged_lines: Counter<u64>,
    /// `pod.spec.lead_time`, seconds.
    pub pod_spec_lead_time: Histogram<f64>,
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

/// Harold's instruments, made once from the global meter. With telemetry off that is
/// the no-op meter, so nothing is recorded.
#[allow(
    dead_code,
    reason = "the phone-request, turn and pod tasks record into the instruments later"
)]
pub(crate) fn instruments() -> &'static Instruments {
    // A test that ran before the test providers were set would otherwise bind the
    // instruments to the no-op meter for the whole test binary.
    #[cfg(test)]
    test_support::install();
    INSTRUMENTS.get_or_init(|| {
        let meter = global::meter(SCOPE);
        let counter = |name: &'static str, unit: &'static str| {
            meter.u64_counter(name).with_unit(unit).build()
        };
        let seconds = |name: &'static str, buckets: &[f64]| {
            meter
                .f64_histogram(name)
                .with_unit("s")
                .with_boundaries(buckets.to_vec())
                .build()
        };
        Instruments {
            phone_requests: counter("harold.phone_requests", "{request}"),
            phone_requests_wrong: counter("harold.phone_requests.wrong", "{request}"),
            phone_request_latency: seconds(
                "harold.phone_request.latency",
                &[1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0],
            ),
            turns: counter("harold.turns", "{turn}"),
            turn_duration: seconds(
                "harold.turn.duration",
                &[10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0],
            ),
            pod_merges: counter("pod.merges", "{merge}"),
            pod_merged_lines: counter("pod.merged_lines", "{line}"),
            pod_spec_lead_time: seconds(
                "pod.spec.lead_time",
                &[
                    600.0, 1800.0, 3600.0, 7200.0, 14400.0, 43200.0, 86400.0, 172800.0,
                ],
            ),
        }
    })
}

// ---------------------------------------------------------------------------
// Phone requests
// ---------------------------------------------------------------------------

/// A reply typed within this long of being sent is on time. One typed later, or never,
/// is unavailable. A tag for another pane within this long of a reply Harold routed
/// marks that reply as having gone to the wrong agent.
#[allow(
    dead_code,
    reason = "the listener and projector judge phone requests in a later telemetry task"
)]
pub(crate) const ON_TIME: Duration = Duration::from_secs(600);

/// A prompt Harold typed joins the next turn end on its pane within this long.
#[allow(
    dead_code,
    reason = "the prompt map keeps prompts this long in a later telemetry task"
)]
pub(crate) const PROMPT_KEPT: Duration = Duration::from_secs(3600);

/// How a phone reply's pane was found: the `resolved_by` attribute.
#[allow(
    dead_code,
    reason = "the listener and projector judge phone requests in a later telemetry task"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedBy {
    ExactTag,
    TagSubstring,
    AiModel,
    LatestNotifier,
    MyAgent,
    None,
}

#[allow(
    dead_code,
    reason = "the listener and projector judge phone requests in a later telemetry task"
)]
impl ResolvedBy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ExactTag => "exact_tag",
            Self::TagSubstring => "tag_substring",
            Self::AiModel => "ai_model",
            Self::LatestNotifier => "latest_notifier",
            Self::MyAgent => "my_agent",
            Self::None => "none",
        }
    }

    /// Harold chose the pane (`latest_notifier`, `ai_model`, `my_agent`), rather than
    /// the user's tag.
    pub(crate) fn harold_chose(self) -> bool {
        matches!(self, Self::LatestNotifier | Self::AiModel | Self::MyAgent)
    }
}

/// What happened to a phone reply on this Mac.
#[allow(
    dead_code,
    reason = "the listener and projector judge phone requests in a later telemetry task"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Routing {
    /// Another Mac owns the reply and counts it.
    NotOwned,
    /// Messaging is paused: not counted.
    Paused,
    /// No pane, an unmatched tag, or the pane is gone.
    NoPane,
    /// Typed into `pane_id`. `tagged` is whether the reply carried a tag.
    Typed {
        pane_id: String,
        resolved_by: ResolvedBy,
        tagged: bool,
    },
}

/// How one phone reply counts.
#[allow(
    dead_code,
    reason = "the listener and projector judge phone requests in a later telemetry task"
)]
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PhoneVerdict {
    /// `Some("typed")`, `Some("unavailable")`, or `None` when the reply is not counted.
    pub outcome: Option<&'static str>,
    /// From the reply's own time to typing, when it was typed, on time or late.
    pub latency: Option<Duration>,
    /// The channel and `resolved_by` of the earlier request this one shows went to the
    /// wrong agent.
    pub wrong: Option<(String, ResolvedBy)>,
}

/// The last counted reply on this Mac, when it was typed on time. The next reply, if it
/// is a tag for another pane, may show it went to the wrong agent.
#[allow(
    dead_code,
    reason = "the listener and projector judge phone requests in a later telemetry task"
)]
#[derive(Clone, Debug)]
struct LastTyped {
    channel: String,
    reply_at: OffsetDateTime,
    pane_id: String,
    resolved_by: ResolvedBy,
}

/// Judges each phone reply in turn, remembering the last counted one when it was typed
/// on time.
#[allow(
    dead_code,
    reason = "the listener and projector judge phone requests in a later telemetry task"
)]
#[derive(Default)]
pub(crate) struct PhoneBook {
    last_typed: Option<LastTyped>,
}

#[allow(
    dead_code,
    reason = "the listener and projector judge phone requests in a later telemetry task"
)]
impl PhoneBook {
    /// Counts a reply sent at `reply_at` and handled at `typed_at`.
    ///
    /// - `NotOwned` and `Paused` are not counted, and leave the last typed reply as it was.
    /// - `NoPane` is unavailable.
    /// - `Typed` is typed when it was typed within `ON_TIME`, and unavailable otherwise.
    /// - A tagged `Typed` reply marks the last typed reply as wrong when that one went to
    ///   another pane, Harold chose its pane, and it was sent at most `ON_TIME` earlier.
    ///   A reply the user tagged went where the user said, so a later tag for another
    ///   pane is the user moving on, not a misroute.
    /// - Only the reply right after a typed one can mark it wrong, so every counted reply
    ///   replaces the last typed reply: with itself when it was typed on time, and with
    ///   none otherwise. A late reply is never wrong, because the right-agent rate is
    ///   typed minus wrong.
    pub(crate) fn judge(
        &mut self,
        channel: &str,
        reply_at: OffsetDateTime,
        typed_at: OffsetDateTime,
        routing: &Routing,
    ) -> PhoneVerdict {
        let Routing::Typed {
            pane_id,
            resolved_by,
            tagged,
        } = routing
        else {
            let counted = matches!(routing, Routing::NoPane);
            if counted {
                self.last_typed = None;
            }
            return PhoneVerdict {
                outcome: counted.then_some("unavailable"),
                latency: None,
                wrong: None,
            };
        };
        // A reply handled before its own time (a clock difference) took no time.
        let latency = Duration::try_from(typed_at - reply_at).unwrap_or(Duration::ZERO);
        let on_time = latency <= ON_TIME;
        let wrong = self
            .last_typed
            .as_ref()
            .filter(|last| {
                *tagged
                    && last.pane_id != *pane_id
                    && last.resolved_by.harold_chose()
                    && Duration::try_from(reply_at - last.reply_at)
                        .is_ok_and(|since| since <= ON_TIME)
            })
            .map(|last| (last.channel.clone(), last.resolved_by));
        self.last_typed = on_time.then(|| LastTyped {
            channel: channel.to_string(),
            reply_at,
            pane_id: pane_id.clone(),
            resolved_by: *resolved_by,
        });
        PhoneVerdict {
            outcome: Some(if on_time { "typed" } else { "unavailable" }),
            latency: Some(latency),
            wrong,
        }
    }
}

// ---------------------------------------------------------------------------
// Turn starts
// ---------------------------------------------------------------------------

/// What started a turn: the `started_by` attribute.
#[allow(
    dead_code,
    reason = "the turn span and turn metrics use the turn start in a later telemetry task"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartedBy {
    Phone,
    TimedPrompt,
    Desk,
    Unknown,
}

/// When a turn started, if known, and what started it.
#[allow(
    dead_code,
    reason = "the turn span and turn metrics use the turn start in a later telemetry task"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnStart {
    pub at: Option<OffsetDateTime>,
    pub started_by: StartedBy,
}

/// The spec's order: the hook's transcript time, then Harold's typing, then first busy.
/// A hook time that is not RFC 3339 counts as absent, and any time after `hook_reached`
/// is skipped. `started_by` is the typing's when Harold typed the prompt at or before
/// `hook_reached`; otherwise `Desk` when the start is known, and `Unknown` when it is not.
#[allow(
    dead_code,
    reason = "the turn span and turn metrics use the turn start in a later telemetry task"
)]
pub(crate) fn turn_start(
    hook_time: Option<&str>,
    typed: Option<(OffsetDateTime, StartedBy)>,
    busy_since: Option<OffsetDateTime>,
    hook_reached: OffsetDateTime,
) -> TurnStart {
    let hook = hook_time.and_then(|text| OffsetDateTime::parse(text, &Rfc3339).ok());
    let typed = typed.filter(|(at, _)| *at <= hook_reached);
    let at = [hook, typed.map(|(at, _)| at), busy_since]
        .into_iter()
        .flatten()
        .find(|at| *at <= hook_reached);
    let started_by = match (typed, at) {
        (Some((_, started_by)), _) => started_by,
        (None, Some(_)) => StartedBy::Desk,
        (None, None) => StartedBy::Unknown,
    };
    TurnStart { at, started_by }
}

// ---------------------------------------------------------------------------
// Start and stop
// ---------------------------------------------------------------------------

/// Keeps the exporters running. `shutdown` sends what is queued and stops them.
pub struct TelemetryGuard {
    pipeline: Option<Pipeline>,
}

impl TelemetryGuard {
    /// A guard for telemetry that is off: there is nothing to stop.
    pub(crate) fn off() -> Self {
        Self { pipeline: None }
    }

    /// Flushes and stops the exporters, waiting at most `SHUTDOWN_BUDGET` (2 s) in all.
    pub async fn shutdown(self) {
        let Some(pipeline) = self.pipeline else {
            return;
        };
        let stopping =
            tokio::task::spawn_blocking(move || pipeline.shutdown_within(SHUTDOWN_BUDGET));
        let _ = tokio::time::timeout(SHUTDOWN_BUDGET, stopping).await;
    }
}

/// Installs the global log subscriber: `harold.log` as before and, when `otlp_endpoint`
/// is set, the OTLP export of spans and log records. Also sets the global tracer and
/// meter providers.
///
/// Must run inside the tokio runtime: the exporters' gRPC channel is made here and its
/// connection task runs on the runtime.
pub fn init_telemetry(level: &str, telemetry: &TelemetrySettings, machine: &str) -> TelemetryGuard {
    let mut build_failed = None;
    let pipeline = telemetry.otlp_endpoint.as_deref().and_then(|endpoint| {
        Pipeline::otlp(endpoint, machine, ExportHealth::logging(endpoint))
            .map_err(|error| build_failed = Some(error))
            .ok()
    });

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let harold_log = fmt::layer()
        .log_internal_errors(true)
        .with_filter(harold_log_filter(env_filter));
    let export = pipeline.as_ref().map(|pipeline| pipeline.layers(level));
    tracing_subscriber::registry()
        .with(harold_log)
        .with(export)
        .init();

    if let Some(error) = build_failed {
        warn!(error = %error, "telemetry export off: the collector address could not be used");
    }
    let Some(pipeline) = pipeline else {
        return TelemetryGuard::off();
    };
    pipeline.install_global_providers();
    TelemetryGuard {
        pipeline: Some(pipeline),
    }
}

// ---------------------------------------------------------------------------
// Layer filters
// ---------------------------------------------------------------------------

/// Spans with target `TRACE_TARGET` exist for export only.
fn is_trace_span(meta: &Metadata<'_>) -> bool {
    meta.is_span() && meta.target() == TRACE_TARGET
}

/// What `harold.log` writes: what `env_filter` enables, apart from the exported spans,
/// so its lines get no new span prefixes.
pub(crate) fn harold_log_filter<S>(env_filter: EnvFilter) -> impl Filter<S> + Send + Sync + 'static
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    env_filter.and(not_a_trace_span())
}

/// Everything but the exported spans. Its level hint is `TRACE`, so that combining it
/// with another filter keeps that filter's hint.
fn not_a_trace_span() -> FilterFn<impl Fn(&Metadata<'_>) -> bool + Send + Sync + 'static> {
    filter_fn(|meta: &Metadata<'_>| !is_trace_span(meta)).with_max_level_hint(LevelFilter::TRACE)
}

/// What becomes an OpenTelemetry span: the `TRACE_TARGET` spans, plus error events,
/// which mark the span they happen in as failed. Other events would only become span
/// events, which `FieldFilter` drops.
fn span_export_filter() -> FilterFn<impl Fn(&Metadata<'_>) -> bool + Send + Sync + 'static> {
    filter_fn(|meta: &Metadata<'_>| {
        is_trace_span(meta) || (meta.is_event() && *meta.level() == Level::ERROR)
    })
    .with_max_level_hint(LevelFilter::INFO)
}

/// What becomes an OpenTelemetry log record: the events `level` enables whose target
/// passes the allow-list in `is_exported_log_target` (Harold's and the `events` crate's
/// own targets, apart from the health lines). Every other target, including the
/// exporter's own libraries, is written to `harold.log` only.
fn log_export_filter<S>(level: &str) -> impl Filter<S> + Send + Sync + use<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    EnvFilter::new(level).and(
        filter_fn(|meta: &Metadata<'_>| is_exported_log_target(meta.target()))
            .with_max_level_hint(LevelFilter::TRACE),
    )
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// How the exporters are driven.
pub(crate) enum Processing {
    /// Queued in memory and sent in batches by background threads (production).
    Batch,
    /// Sent as each span or log record ends, on the thread that ends it (tests).
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "only the test pipeline sends at once")
    )]
    Simple,
}

/// The tracer, logger and meter providers, each sending through `FieldFilter` and
/// `HealthWatch` to its exporter.
pub(crate) struct Pipeline {
    tracer_provider: SdkTracerProvider,
    logger_provider: SdkLoggerProvider,
    meter_provider: SdkMeterProvider,
}

impl Pipeline {
    /// Builds the providers around any span, log and metric exporters.
    pub(crate) fn build<S, L, M>(
        spans: S,
        logs: L,
        metrics: M,
        resource: Resource,
        health: &Arc<ExportHealth>,
        processing: Processing,
    ) -> Self
    where
        S: SpanExporter + 'static,
        L: LogExporter + 'static,
        M: PushMetricExporter,
    {
        let spans = FieldFilter(HealthWatch::new(spans, health));
        let logs = FieldFilter(HealthWatch::new(logs, health));
        let metrics = HealthWatch::new(metrics, health);

        let tracer_provider = SdkTracerProvider::builder().with_resource(resource.clone());
        let logger_provider = SdkLoggerProvider::builder().with_resource(resource.clone());
        let (tracer_provider, logger_provider) = match processing {
            Processing::Batch => (
                tracer_provider.with_span_processor(BatchSpanProcessor::builder(spans).build()),
                logger_provider.with_log_processor(BatchLogProcessor::builder(logs).build()),
            ),
            Processing::Simple => (
                tracer_provider.with_span_processor(SimpleSpanProcessor::new(spans)),
                logger_provider.with_log_processor(SimpleLogProcessor::new(logs)),
            ),
        };
        let meter_provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(
                PeriodicReader::builder(metrics)
                    .with_interval(METRIC_INTERVAL)
                    .build(),
            )
            .build();
        Self {
            tracer_provider: tracer_provider.build(),
            logger_provider: logger_provider.build(),
            meter_provider,
        }
    }

    /// The production pipeline: OTLP over gRPC to `endpoint`, sent in batches.
    pub(crate) fn otlp(
        endpoint: &str,
        machine: &str,
        health: Arc<ExportHealth>,
    ) -> Result<Self, opentelemetry_otlp::ExporterBuildError> {
        use opentelemetry_otlp::WithExportConfig as _;
        let spans = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(EXPORT_TIMEOUT)
            .build()?;
        let logs = opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(EXPORT_TIMEOUT)
            .build()?;
        let metrics = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .with_timeout(EXPORT_TIMEOUT)
            .build()?;
        Ok(Self::build(
            spans,
            logs,
            metrics,
            resource(machine),
            &health,
            Processing::Batch,
        ))
    }

    /// The two export layers: spans with target `TRACE_TARGET` become OpenTelemetry
    /// spans, and log events become OpenTelemetry log records that carry the trace of
    /// the span they happen in.
    pub(crate) fn layers<S>(&self, level: &str) -> impl Layer<S> + use<S>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let spans = tracing_opentelemetry::layer()
            .with_tracer(self.tracer_provider.tracer(SCOPE))
            .with_location(false)
            .with_threads(false)
            .with_tracked_inactivity(false)
            .with_filter(span_export_filter());
        let logs = OpenTelemetryTracingBridge::new(&self.logger_provider)
            .with_filter(log_export_filter(level));
        spans.and_then(logs)
    }

    /// A subscriber with only the export layers, for a test to set on its own thread.
    #[cfg(test)]
    pub(crate) fn subscriber_for_test(&self) -> impl Subscriber + Send + Sync + 'static {
        tracing_subscriber::registry().with(self.layers("info"))
    }

    /// Sends everything queued now.
    #[cfg(test)]
    pub(crate) fn force_flush(&self) -> OTelSdkResult {
        let spans = self.tracer_provider.force_flush();
        let logs = self.logger_provider.force_flush();
        let metrics = self.meter_provider.force_flush();
        spans.and(logs).and(metrics)
    }

    fn install_global_providers(&self) {
        global::set_tracer_provider(self.tracer_provider.clone());
        global::set_meter_provider(self.meter_provider.clone());
        ENABLED.store(true, Ordering::SeqCst);
    }

    /// Stops the three providers together, each sending what it has queued, and returns
    /// once they have stopped or `budget` has passed. A provider still stopping then
    /// carries on in the background.
    pub(crate) fn shutdown_within(&self, budget: Duration) {
        let deadline = Instant::now() + budget;
        let tracer_provider = self.tracer_provider.clone();
        let logger_provider = self.logger_provider.clone();
        let meter_provider = self.meter_provider.clone();
        let stops: [(&str, ProviderStop); 3] = [
            (
                "spans",
                Box::new(move || drop(tracer_provider.shutdown_with_timeout(budget))),
            ),
            (
                "logs",
                Box::new(move || drop(logger_provider.shutdown_with_timeout(budget))),
            ),
            (
                "metrics",
                Box::new(move || drop(meter_provider.shutdown_with_timeout(budget))),
            ),
        ];
        let (stopped, stopped_signals) = mpsc::channel::<()>();
        let mut stopping = 0;
        for (name, stop) in stops {
            let stopped = stopped.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("telemetry-shutdown-{name}"))
                .spawn(move || {
                    stop();
                    let _ = stopped.send(());
                });
            if spawned.is_ok() {
                stopping += 1;
            }
        }
        for _ in 0..stopping {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if stopped_signals.recv_timeout(remaining).is_err() {
                return;
            }
        }
    }
}

/// Stops one provider.
type ProviderStop = Box<dyn FnOnce() + Send>;

/// What every export carries: the service and the Mac it runs on.
fn resource(machine: &str) -> Resource {
    Resource::builder()
        .with_service_name("harold")
        .with_attribute(KeyValue::new("harold.machine", machine.to_string()))
        .build()
}

// ---------------------------------------------------------------------------
// Allowed fields only
// ---------------------------------------------------------------------------

/// Passes spans and log records on with only the attributes on `ALLOWED_FIELDS`. Span
/// events and links are dropped, and an error status loses its description.
#[derive(Debug)]
pub(crate) struct FieldFilter<E>(E);

fn allowed_span(mut span: SpanData) -> SpanData {
    span.attributes.retain(|kv| is_allowed(kv.key.as_str()));
    span.events = SpanEvents::default();
    span.links = SpanLinks::default();
    if matches!(span.status, Status::Error { .. }) {
        span.status = Status::error("");
    }
    span
}

/// A new log record with `record`'s body, times, severity and trace, and only its
/// allowed attributes. The SDK offers no way to remove an attribute, so the record is
/// rebuilt from a blank one.
fn allowed_record(record: &SdkLogRecord) -> SdkLogRecord {
    static BLANK: OnceLock<SdkLogger> = OnceLock::new();
    let mut allowed = BLANK
        .get_or_init(|| {
            SdkLoggerProvider::builder()
                .build()
                .logger("harold.field_filter")
        })
        .create_log_record();
    if let Some(name) = record.event_name() {
        allowed.set_event_name(name);
    }
    if let Some(target) = record.target() {
        allowed.set_target(target.clone());
    }
    if let Some(timestamp) = record.timestamp() {
        allowed.set_timestamp(timestamp);
    }
    if let Some(observed) = record.observed_timestamp() {
        allowed.set_observed_timestamp(observed);
    }
    if let Some(trace) = record.trace_context() {
        allowed.set_trace_context(trace.trace_id, trace.span_id, trace.trace_flags);
    }
    if let Some(text) = record.severity_text() {
        allowed.set_severity_text(text);
    }
    if let Some(number) = record.severity_number() {
        allowed.set_severity_number(number);
    }
    if let Some(body) = record.body() {
        allowed.set_body(body.clone());
    }
    allowed.add_attributes(
        record
            .attributes_iter()
            .filter(|(key, _)| is_allowed(key.as_str()))
            .cloned(),
    );
    allowed
}

impl<E: SpanExporter> SpanExporter for FieldFilter<E> {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        self.0.export(batch.into_iter().map(allowed_span).collect())
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn shutdown(&mut self) -> OTelSdkResult {
        self.0.shutdown()
    }

    fn force_flush(&mut self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

impl<E: LogExporter> LogExporter for FieldFilter<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let records: Vec<_> = batch
            .iter()
            .map(|(record, scope)| (allowed_record(record), scope.clone()))
            .collect();
        let borrowed: Vec<_> = records
            .iter()
            .map(|(record, scope)| (record, scope))
            .collect();
        self.0.export(LogBatch::new(&borrowed)).await
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.0.shutdown()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

// ---------------------------------------------------------------------------
// Collector health
// ---------------------------------------------------------------------------

/// A change in whether exports reach the collector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthChange {
    Failing,
    Recovered,
}

/// Whether exports are failing, shared by the span, log and metric exporters. Only a
/// change is reported, so a collector that stays down gives one line, not one per batch.
pub(crate) struct ExportHealth {
    failing: AtomicBool,
    report: Box<dyn Fn(HealthChange) + Send + Sync>,
}

impl std::fmt::Debug for ExportHealth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExportHealth")
            .field("failing", &self.failing)
            .finish_non_exhaustive()
    }
}

impl ExportHealth {
    pub(crate) fn new(report: Box<dyn Fn(HealthChange) + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            failing: AtomicBool::new(false),
            report,
        })
    }

    /// Reports changes as lines in `harold.log`.
    pub(crate) fn logging(endpoint: &str) -> Arc<Self> {
        let endpoint = endpoint.to_string();
        Self::new(Box::new(move |change| match change {
            HealthChange::Failing => warn!(
                target: HEALTH_TARGET,
                "telemetry export failing (collector unreachable at {endpoint})"
            ),
            HealthChange::Recovered => info!(target: HEALTH_TARGET, "telemetry export recovered"),
        }))
    }

    /// Records one export's result, and reports it when it changes the health.
    pub(crate) fn observe(&self, ok: bool) {
        let was_failing = self.failing.swap(!ok, Ordering::SeqCst);
        if was_failing == !ok {
            return;
        }
        (self.report)(if ok {
            HealthChange::Recovered
        } else {
            HealthChange::Failing
        });
    }
}

/// Passes each export's result to `ExportHealth`.
#[derive(Debug)]
pub(crate) struct HealthWatch<E> {
    inner: E,
    health: Arc<ExportHealth>,
}

impl<E> HealthWatch<E> {
    fn new(inner: E, health: &Arc<ExportHealth>) -> Self {
        Self {
            inner,
            health: Arc::clone(health),
        }
    }
}

impl<E: SpanExporter> SpanExporter for HealthWatch<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        self.health.observe(result.is_ok());
        result
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&mut self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn force_flush(&mut self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: LogExporter> LogExporter for HealthWatch<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let result = self.inner.export(batch).await;
        self.health.observe(result.is_ok());
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: PushMetricExporter> PushMetricExporter for HealthWatch<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let result = self.inner.export(metrics).await;
        self.health.observe(result.is_ok());
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn shutdown(&self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn temporality(&self) -> Temporality {
        self.inner.temporality()
    }
}

// ---------------------------------------------------------------------------
// Test support
// ---------------------------------------------------------------------------

/// The test binary's one global subscriber and its in-memory exporters. Tests find their
/// own data among everyone's by a unique pane id, trace id or product name.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::OnceLock;

    use opentelemetry_sdk::logs::InMemoryLogExporter;
    use opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::ResourceMetrics;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::filter::{FilterExt as _, LevelFilter};
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::{ExportHealth, Pipeline, Processing};
    use crate::outbound::fake::CapturedLogWriter;

    /// The machine name on every test export.
    pub(crate) const MACHINE: &str = "test-mac";

    struct Installed {
        pipeline: Pipeline,
        spans: InMemorySpanExporter,
        logs: InMemoryLogExporter,
        metrics: InMemoryMetricExporter,
    }

    static INSTALLED: OnceLock<Installed> = OnceLock::new();

    fn installed() -> &'static Installed {
        INSTALLED.get_or_init(|| {
            let spans = InMemorySpanExporter::default();
            let logs = InMemoryLogExporter::default();
            let metrics = InMemoryMetricExporter::default();
            let pipeline = Pipeline::build(
                spans.clone(),
                logs.clone(),
                metrics.clone(),
                super::resource(MACHINE),
                &ExportHealth::new(Box::new(|_| {})),
                Processing::Simple,
            );
            // The capturing `fmt` layer writes as `harold.log` does, without the time and
            // colours, and keeps only the lines of a thread that is capturing.
            let harold_log = tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .log_internal_errors(true)
                .with_writer(|| CapturedLogWriter)
                .with_filter(LevelFilter::INFO.and(super::not_a_trace_span()));
            let subscriber = tracing_subscriber::registry()
                .with(harold_log)
                .with(pipeline.layers("info"));
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other process-wide log subscriber is set in the test binary");
            pipeline.install_global_providers();
            Installed {
                pipeline,
                spans,
                logs,
                metrics,
            }
        })
    }

    /// Installs the test binary's global subscriber and providers, once.
    pub(crate) fn install() {
        installed();
    }

    /// Every span exported so far, by every test.
    pub(crate) fn spans() -> Vec<SpanData> {
        installed().spans.get_finished_spans().unwrap()
    }

    /// Every log record exported so far, by every test.
    pub(crate) fn logs() -> Vec<LogDataWithResource> {
        installed().logs.get_emitted_logs().unwrap()
    }

    /// Every metric export so far, after collecting the current values.
    pub(crate) fn metrics() -> Vec<ResourceMetrics> {
        let installed = installed();
        let _ = installed.pipeline.meter_provider.force_flush();
        installed.metrics.get_finished_metrics().unwrap()
    }

    /// Debug text of every exported span, log record and metric, for searching.
    pub(crate) fn exported_text() -> String {
        let mut text = String::new();
        for span in spans() {
            text.push_str(&format!("{span:?}\n"));
        }
        for log in logs() {
            text.push_str(&format!("{log:?}\n"));
        }
        for metric in metrics() {
            text.push_str(&format!("{metric:?}\n"));
        }
        text
    }
}

#[cfg(test)]
#[path = "telemetry_tests.rs"]
mod tests;
