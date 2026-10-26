//! Harold's logs and telemetry. `harold.log` is written as it always was. When
//! export is on (`[telemetry] export`, true by default) and `otlp_endpoint` is set, spans
//! and log records are also exported to an OpenTelemetry collector over OTLP, carrying
//! only the fields on `ALLOWED_FIELDS`.
//!
//! Export is queued in memory and sent in batches by background threads, so a slow or
//! missing collector never slows Harold down. An unreachable collector is reported in
//! `harold.log` once when exports start failing and once when they recover.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, mpsc};
use std::time::{Duration, Instant};

use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::propagation::{Extractor, TextMapPropagator as _};
use opentelemetry::trace::{
    Link, SpanBuilder, Status, TraceContextExt, Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue};
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
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{
    BatchSpanProcessor, SdkTracerProvider, SimpleSpanProcessor, SpanData, SpanEvents, SpanExporter,
    SpanLinks,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{Instrument as _, Level, Metadata, Subscriber, info, warn};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;
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
    "notes",
    "removed",
    "pending",
    "last_ok_age_s",
    "embed_ms",
    "search_ms",
    "results",
    "embed_ms_per_passage",
    "passages",
    "rss_mb",
    "buckwal.outcome",
    "buckwal.retries",
    "buckwal.bytes",
    "buckwal.generation",
    "s3.status",
    "pane_id",
    "provider_id",
    "program",
    "pane",
    "session.id",
    "session.source",
    "component",
    "reason",
    "route",
    "hidden_reported",
    "pages",
    "outcome",
    "channel",
    "resolved_by",
    "kind",
    "started_by",
    "turn.start",
    "state",
    "count",
    "text_cut",
    "call_from",
    "cut",
    "audio_ms",
    "language",
    "waited_ms",
    "source",
    "start_ms",
    "first_sound_ms",
    "answer_ms",
    "answered_by",
    "facts_ms",
    "model_ms",
    "cost_micros",
    "input_tokens",
    "output_tokens",
    "hand_over_id",
    "typed_while_busy",
    "photo_bytes",
    "wait_ms",
    "session_by",
    "alert",
    "pod.product",
    "product",
    "id",
    "listener",
    "sha",
    "spec.id",
    "spec.state_at",
    "spec.words",
    "spec.revisions",
    "spec.stage",
    "file",
    "phase",
    "change",
    "event_type",
    "event_version",
    "result",
    "watch",
    "run",
    "cost_usd",
    "agent",
    "seconds",
    "event_id",
    "exit_code",
    "signal",
    "subtype",
    "is_error",
    "terminal_reason",
    "api_error_status",
    "num_turns",
    "busy_reason",
    "heard_ms",
    "first_heard_ms",
    "backlog_ms",
    "background_start",
    "background",
    "loudest",
    "problem",
    "step",
    "error_name",
    "context_state",
    "track_state",
    "track_muted",
    "pieces_made",
    "pieces_posted",
    "pieces_dropped",
    "posts_failed",
    "empty_blocks",
    "loud_frames",
    "speech_cut",
    "setup",
    "verdict",
    "share_over_bar",
    "voice_found",
    "echo_cancellation",
    "standalone",
    "age_hours",
    "compactions",
    "cause",
    "model",
    "bytes",
    "total_bytes",
    "ms",
    "load_ms",
    "load_waited_ms",
    "cycles",
    "worker",
    "warm_ms",
    "check0_offline",
    "check0_started",
    "check0_results",
    "check0_first_result_ms",
    "check0_error",
    "check1_started",
    "check1_results",
    "check1_first_result_ms",
    "check1_error",
    "check2_started",
    "check2_results",
    "check2_error",
    "check3_started",
    "check3_errors",
    "check4_results",
    "check4_final_at_ms",
    "check4_gap_ms",
    "check4_error",
    "check5_started",
    "check5_kept_running",
    "check5_playback_ok",
    "check5_words_heard",
    "check5_error",
    "check6_restarted",
    "check6_heard",
    "check6_error",
    "sequence",
    "caller",
    "first_audio_ms",
    "loaded",
    "device",
    "waiting",
    "schema_steps",
    "records",
    "voided",
    "metric",
    "value",
    "normal_top",
    "days",
    "top_processes",
    "minutes",
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
    target != HEALTH_TARGET && (ours("harold") || ours("events") || ours("buckwal"))
}

/// Whether a span with `target` is buckwal's own.
fn is_buckwal_target(target: &str) -> bool {
    target == "buckwal" || target.starts_with("buckwal::")
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
pub(crate) fn local_offset() -> time::UtcOffset {
    LOCAL_OFFSET.get().copied().unwrap_or(time::UtcOffset::UTC)
}

// ---------------------------------------------------------------------------
// Global access
// ---------------------------------------------------------------------------

/// Harold's tracer, for spans that start in the past. It is a no-op tracer when
/// telemetry is off.
pub(crate) fn tracer() -> BoxedTracer {
    #[cfg(test)]
    test_support::install();
    global::tracer(SCOPE)
}

// ---------------------------------------------------------------------------
// Instruments
// ---------------------------------------------------------------------------

/// Harold's counters and histograms, with the names, units and buckets of the spec's
/// Metrics table. The four pod gauges are observable gauges that `pod_stats` registers,
/// because it owns their data.
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
    /// `watch.runs`, `{run}`, by `folder` and `outcome`.
    pub watch_runs: Counter<u64>,
    /// `watch.cost_usd`, `USD`, by `folder`.
    pub watch_cost_usd: Counter<f64>,
    /// `harold.notifications`, `{notification}`, by `agent`, `channel` and `outcome`.
    pub notifications: Counter<u64>,
    /// `harold.web_app.pushes`, `{push}`, by `outcome`.
    pub web_app_pushes: Counter<u64>,
    /// `harold.app_spans.not_forwarded`, `{span}`, by `reason`.
    pub app_spans_not_forwarded: Counter<u64>,
}

static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

/// Harold's meter, for instruments another module owns, such as the pod gauges. It is the
/// no-op meter when telemetry is off.
pub(crate) fn meter() -> Meter {
    #[cfg(test)]
    test_support::install();
    global::meter(SCOPE)
}

/// Harold's instruments, made once from the global meter. With telemetry off that is
/// the no-op meter, so nothing is recorded.
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
            watch_runs: counter("watch.runs", "{run}"),
            watch_cost_usd: meter.f64_counter("watch.cost_usd").with_unit("USD").build(),
            notifications: counter("harold.notifications", "{notification}"),
            web_app_pushes: counter("harold.web_app.pushes", "{push}"),
            app_spans_not_forwarded: counter("harold.app_spans.not_forwarded", "{span}"),
        }
    })
}

// ---------------------------------------------------------------------------
// Counting from 0
// ---------------------------------------------------------------------------

/// Counts one notification an agent's pane sent (`outcome = sent`) or whose channel list
/// kept back (`outcome = kept`). `agent` is the pane's folder below the home folder; it
/// never holds message text.
pub(crate) fn count_notification(agent: &str, channel: &str, outcome: &str) {
    instruments().notifications.add(
        1,
        &[
            KeyValue::new("agent", agent.to_string()),
            KeyValue::new("channel", channel.to_string()),
            KeyValue::new("outcome", outcome.to_string()),
        ],
    );
}

/// Every `reason` browser spans can go unforwarded for.
pub(crate) const APP_SPANS_NOT_FORWARDED_REASONS: [&str; 4] = [
    "collector_unreachable",
    "collector_refused",
    "timed_out",
    "queue_full",
];

/// Counts `spans` browser spans Harold took but the collector never got.
pub(crate) fn count_app_spans_not_forwarded(spans: u64, reason: &'static str) {
    instruments()
        .app_spans_not_forwarded
        .add(spans, &[KeyValue::new("reason", reason)]);
}

/// Every `channel` a phone request can carry.
#[cfg(feature = "imessage")]
pub(crate) const PHONE_CHANNELS: [&str; 2] = ["imessage", "unknown"];
/// Every `channel` a phone request can carry: a build without `imessage` seeds only `unknown`.
#[cfg(not(feature = "imessage"))]
pub(crate) const PHONE_CHANNELS: [&str; 1] = ["unknown"];

/// Records 0 once for every attribute set of the phone-request counters the alerts read.
/// HyperDX reads the first point of a running total as 0, so without this the first
/// request of each set after a start would never show as a rise. `main` calls it once,
/// after `init_telemetry`, when export is on.
pub(crate) fn register_phone_counters() {
    let instruments = instruments();
    for outcome in ["accepted", "gone", "refused", "unreachable"] {
        instruments
            .web_app_pushes
            .add(0, &[KeyValue::new("outcome", outcome)]);
    }
    for reason in APP_SPANS_NOT_FORWARDED_REASONS {
        count_app_spans_not_forwarded(0, reason);
    }
    for channel in PHONE_CHANNELS {
        for resolved_by in ResolvedBy::ALL {
            let channel = KeyValue::new("channel", channel);
            let resolved = KeyValue::new("resolved_by", resolved_by.as_str());
            for outcome in ["typed", "unavailable"] {
                instruments.phone_requests.add(
                    0,
                    &[
                        channel.clone(),
                        KeyValue::new("outcome", outcome),
                        resolved.clone(),
                    ],
                );
            }
            instruments
                .phone_requests_wrong
                .add(0, &[channel, resolved]);
        }
    }
    // Web app replies are counted but never judged, so they have no `.wrong` series.
    for outcome in ["typed", "unavailable"] {
        instruments.phone_requests.add(
            0,
            &[
                KeyValue::new("channel", WEB_APP_CHANNEL),
                KeyValue::new("outcome", outcome),
                KeyValue::new("resolved_by", ResolvedBy::None.as_str()),
            ],
        );
    }
}

/// Records 0 once for `product`'s merge counters with no `spec.id`: `pod.merges`, and
/// `pod.merged_lines` for each `change`. The pod facts call it on a product's first poll.
pub(crate) fn register_merge_counters(product: &str) {
    let instruments = instruments();
    let product = KeyValue::new("pod.product", product.to_owned());
    instruments
        .pod_merges
        .add(0, std::slice::from_ref(&product));
    for change in ["added", "removed"] {
        instruments
            .pod_merged_lines
            .add(0, &[product.clone(), KeyValue::new("change", change)]);
    }
}

/// Every `outcome` of `watch.runs` that is a failure or a skipped run, counted from 0 at a
/// watch's start so HyperDX reads the first one as a rise.
pub(crate) const WATCH_FAILURE_OUTCOMES: [&str; 7] = [
    "timeout",
    "process_failed",
    "run_budget_reached",
    "unreadable_answer",
    "skill_missing",
    "max_turns_reached",
    "skipped",
];

/// Records 0 once for the failure and skipped outcomes of `watch.runs` and for
/// `watch.cost_usd` of `folder`. The watch tool calls it when a watch starts or resumes.
pub(crate) fn register_watch_counters(folder: &str) {
    let instruments = instruments();
    let folder = KeyValue::new("folder", folder.to_owned());
    for outcome in WATCH_FAILURE_OUTCOMES {
        instruments
            .watch_runs
            .add(0, &[folder.clone(), KeyValue::new("outcome", outcome)]);
    }
    instruments
        .watch_cost_usd
        .add(0.0, std::slice::from_ref(&folder));
}

// ---------------------------------------------------------------------------
// Phone requests
// ---------------------------------------------------------------------------

/// A reply typed within this long of being sent is on time. One typed later, or never,
/// is unavailable. A tag for another pane within this long of a reply Harold routed
/// marks that reply as having gone to the wrong agent.
pub(crate) const ON_TIME: Duration = Duration::from_secs(600);

/// A prompt Harold typed joins the next turn end on its pane within this long.
pub(crate) const PROMPT_KEPT: Duration = Duration::from_secs(3600);

/// How a phone reply's pane was found: the `resolved_by` attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedBy {
    ExactTag,
    TagSubstring,
    AiModel,
    LatestNotifier,
    MyAgent,
    None,
}

impl ResolvedBy {
    /// Every value, for counting each attribute set from 0.
    pub(crate) const ALL: [Self; 6] = [
        Self::ExactTag,
        Self::TagSubstring,
        Self::AiModel,
        Self::LatestNotifier,
        Self::MyAgent,
        Self::None,
    ];

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Routing {
    /// Another Mac owns the reply and counts it. `tagged` is whether the reply's tag
    /// named that Mac, rather than the reply following the latest notifier there.
    NotOwned { tagged: bool },
    /// Messaging is paused: not counted.
    Paused,
    /// No pane, an unmatched tag, or the pane is gone.
    NoPane,
    /// Typed into `pane_id` at `typed_at`, when Enter was pressed and before the user
    /// was told. `tagged` is whether the reply carried a tag.
    Typed {
        pane_id: String,
        resolved_by: ResolvedBy,
        tagged: bool,
        typed_at: OffsetDateTime,
    },
}

/// How one phone reply counts.
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
#[derive(Clone, Debug)]
struct LastTyped {
    channel: String,
    reply_at: OffsetDateTime,
    pane_id: String,
    resolved_by: ResolvedBy,
    /// A later reply has already counted this one wrong. A request is counted wrong at
    /// most once.
    counted_wrong: bool,
}

/// Judges each phone reply in turn, remembering the last counted one when it was typed
/// on time.
#[derive(Default)]
pub(crate) struct PhoneBook {
    last_typed: Option<LastTyped>,
}

impl PhoneBook {
    /// A phone book that has judged no reply yet.
    const fn new() -> Self {
        Self { last_typed: None }
    }

    /// Counts a reply sent at `reply_at` and handled at `typed_at`.
    ///
    /// - A `web_app` reply is counted but not judged: it never replaces or clears the last
    ///   typed reply and never marks one wrong (see `count_only`).
    /// - `NotOwned` and `Paused` are not counted, and leave the last typed reply as it was.
    /// - `NoPane` is unavailable.
    /// - `Typed` is typed when it was typed within `ON_TIME`, and unavailable otherwise.
    /// - A tagged `Typed` reply marks the last typed reply as wrong when that one went to
    ///   another pane, Harold chose its pane, and it was sent at most `ON_TIME` earlier.
    ///   A reply the user tagged went where the user said, so a later tag for another
    ///   pane is the user moving on, not a misroute.
    /// - A tagged `NotOwned` reply names a pane on another Mac, so it marks the last typed
    ///   reply as wrong under the same rules, though it is not counted itself.
    /// - A reply is counted wrong at most once, however many later replies name another
    ///   pane.
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
        if channel == WEB_APP_CHANNEL {
            return Self::count_only(reply_at, typed_at, routing);
        }
        let Routing::Typed {
            pane_id,
            resolved_by,
            tagged,
            ..
        } = routing
        else {
            let counted = matches!(routing, Routing::NoPane);
            if counted {
                self.last_typed = None;
            }
            // Any pane on another Mac is another pane than this Mac's last typed reply.
            let names_another_mac = matches!(routing, Routing::NotOwned { tagged: true });
            return PhoneVerdict {
                outcome: counted.then_some("unavailable"),
                latency: None,
                wrong: self.wrong_before(reply_at, |_| names_another_mac),
            };
        };
        // A reply handled before its own time (a clock difference) took no time.
        let latency = Duration::try_from(typed_at - reply_at).unwrap_or(Duration::ZERO);
        let on_time = latency <= ON_TIME;
        let wrong = self.wrong_before(reply_at, |last| *tagged && last.pane_id != *pane_id);
        self.last_typed = on_time.then(|| LastTyped {
            channel: channel.to_string(),
            reply_at,
            pane_id: pane_id.clone(),
            resolved_by: *resolved_by,
            counted_wrong: false,
        });
        PhoneVerdict {
            outcome: Some(if on_time { "typed" } else { "unavailable" }),
            latency: Some(latency),
            wrong,
        }
    }

    /// Counts a web app reply without judging it. It always goes to the chief's pane, so
    /// no pane Harold picked can be wrong, and it must not replace the last typed reply
    /// that the wrong-agent rule looks back at.
    fn count_only(
        reply_at: OffsetDateTime,
        typed_at: OffsetDateTime,
        routing: &Routing,
    ) -> PhoneVerdict {
        match routing {
            Routing::Typed { .. } => {
                // A reply handled before its own time (a clock difference) took no time.
                let latency = Duration::try_from(typed_at - reply_at).unwrap_or(Duration::ZERO);
                PhoneVerdict {
                    outcome: Some(if latency <= ON_TIME {
                        "typed"
                    } else {
                        "unavailable"
                    }),
                    latency: Some(latency),
                    wrong: None,
                }
            }
            Routing::NoPane => PhoneVerdict {
                outcome: Some("unavailable"),
                latency: None,
                wrong: None,
            },
            Routing::NotOwned { .. } | Routing::Paused => PhoneVerdict {
                outcome: None,
                latency: None,
                wrong: None,
            },
        }
    }

    /// The channel and `resolved_by` of the last typed reply, when a reply sent at
    /// `reply_at` that `elsewhere` says went to another pane shows it was wrong: Harold
    /// chose its pane, it was sent at most `ON_TIME` earlier, and no reply has counted it
    /// wrong yet. Counting it marks it, so it is never counted again.
    fn wrong_before(
        &mut self,
        reply_at: OffsetDateTime,
        elsewhere: impl FnOnce(&LastTyped) -> bool,
    ) -> Option<(String, ResolvedBy)> {
        let last = self.last_typed.as_mut().filter(|last| {
            !last.counted_wrong
                && last.resolved_by.harold_chose()
                && Duration::try_from(reply_at - last.reply_at).is_ok_and(|since| since <= ON_TIME)
                && elsewhere(last)
        })?;
        last.counted_wrong = true;
        Some((last.channel.clone(), last.resolved_by))
    }
}

// ---------------------------------------------------------------------------
// Open phone requests
// ---------------------------------------------------------------------------

/// A phone request that has started and is not finished: its `harold.phone_request`
/// span, which started at the reply's own time, and what the request is judged by.
#[derive(Clone, Debug)]
pub(crate) struct OpenReply {
    pub cx: Context,
    pub channel: &'static str,
    pub reply_at: OffsetDateTime,
}

impl OpenReply {
    fn trace_id(&self) -> String {
        self.cx.span().span_context().trace_id().to_string()
    }
}

/// A prompt Harold typed into a pane, waiting for the next turn end there.
struct PanePrompt {
    typed_at: OffsetDateTime,
    started_by: StartedBy,
    cx: Option<Context>,
}

/// Requests whose reply is stored and not yet routed, by trace id (or, for a request the
/// event handler opened, by its event's key), each with when it was kept.
static OPEN_REPLIES: Mutex<BTreeMap<String, (OffsetDateTime, OpenReply)>> =
    Mutex::new(BTreeMap::new());

/// Judges every phone reply this Mac counts, in the order they are routed.
static PHONE_BOOK: Mutex<PhoneBook> = Mutex::new(PhoneBook::new());

/// The last prompt Harold typed into each pane.
static PANE_PROMPTS: Mutex<BTreeMap<String, PanePrompt>> = Mutex::new(BTreeMap::new());

/// A map's lock. A test that panicked while holding it leaves the map usable.
fn locked<T>(map: &Mutex<T>) -> MutexGuard<'_, T> {
    map.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether something from `at` is older than `PROMPT_KEPT` at `now`.
fn expired(at: OffsetDateTime, now: OffsetDateTime) -> bool {
    Duration::try_from(now - at).is_ok_and(|age| age > PROMPT_KEPT)
}

/// Opens `harold.phone_request` at the reply's own time, as the root of a new trace.
pub(crate) fn open_phone_request(channel: &'static str, reply_at: OffsetDateTime) -> OpenReply {
    open_phone_request_in(channel, reply_at, None)
}

/// Opens `harold.phone_request` at the reply's own time. With a `parent`, such as the
/// phone's span that `traceparent` names, it joins that trace; without one it is the root
/// of a new trace.
pub(crate) fn open_phone_request_in(
    channel: &'static str,
    reply_at: OffsetDateTime,
    parent: Option<&Context>,
) -> OpenReply {
    let root = Context::new();
    let span = tracer().build_with_context(
        SpanBuilder::from_name("harold.phone_request")
            .with_start_time(reply_at)
            .with_attributes([KeyValue::new("channel", channel)]),
        parent.unwrap_or(&root),
    );
    OpenReply {
        cx: Context::new().with_span(span),
        channel,
        reply_at,
    }
}

/// The channel of a reply typed in the web app. Its requests are counted but not
/// judged: a web app reply always goes to the chief's pane, so there is no pane Harold
/// picked that could be wrong.
pub(crate) const WEB_APP_CHANNEL: &str = "web_app";

/// Reads one `traceparent` value as the propagator's extractor.
struct TraceparentValue<'a>(&'a str);

impl Extractor for TraceparentValue<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        key.eq_ignore_ascii_case("traceparent").then_some(self.0)
    }

    fn keys(&self) -> Vec<&str> {
        vec!["traceparent"]
    }
}

/// The remote parent a W3C `traceparent` value names, or none when it is missing or
/// malformed.
pub(crate) fn context_from_traceparent(value: &str) -> Option<Context> {
    let cx = TraceContextPropagator::new()
        .extract_with_context(&Context::new(), &TraceparentValue(value));
    cx.span().span_context().is_valid().then_some(cx)
}

/// The `traceparent` of the span `cx` holds, when it has a valid one.
pub(crate) fn traceparent_of(cx: &Context) -> Option<String> {
    let span = cx.span();
    let span_context = span.span_context();
    span_context.is_valid().then(|| {
        format!(
            "00-{}-{}-{:02x}",
            span_context.trace_id(),
            span_context.span_id(),
            span_context.trace_flags().to_u8()
        )
    })
}

/// Opens the request for a reply sent at `reply_at` and runs `store` inside
/// `harold.reply_stored`, so the event it stores records the request's trace. A stored
/// reply's request stays open for the event handler; otherwise it ends as `ignored`.
/// Returns whether the reply was stored.
#[cfg(any(feature = "imessage", test))]
pub(crate) async fn store_phone_reply(
    channel: &'static str,
    reply_at: OffsetDateTime,
    store: impl Future<Output = bool>,
) -> bool {
    store_phone_reply_in(channel, reply_at, None, store).await
}

/// `store_phone_reply` for a request that joins the trace of `parent`.
pub(crate) async fn store_phone_reply_in(
    channel: &'static str,
    reply_at: OffsetDateTime,
    parent: Option<&Context>,
    store: impl Future<Output = bool>,
) -> bool {
    let reply = open_phone_request_in(channel, reply_at, parent);
    let storing = tracing::info_span!(target: TRACE_TARGET, "harold.reply_stored");
    // Fails only when no export layer is installed, and then there is no trace to join.
    let _ = storing.set_parent(reply.cx.clone());
    // Kept open before the reply is stored: the event handler may route it as soon as
    // the store commits, and must find this request rather than open another.
    keep_open_reply(reply.clone());
    let stored = store.instrument(storing).await;
    if !stored {
        forget_open_reply(&reply);
        let span = reply.cx.span();
        span.set_attribute(KeyValue::new("outcome", "ignored"));
        span.end();
    }
    stored
}

/// Whether `reply` has a trace to be kept by. With telemetry off, every request has the
/// same invalid (all-zero) trace id, so none is kept.
fn has_trace(reply: &OpenReply) -> bool {
    reply.cx.span().span_context().is_valid()
}

/// Keeps `reply` open under its trace id until it is finished.
pub(crate) fn keep_open_reply(reply: OpenReply) {
    let key = reply.trace_id();
    keep_open_reply_as(key, reply);
}

/// Keeps `reply` open under `key` until it is finished. A request with no trace is not
/// kept.
pub(crate) fn keep_open_reply_as(key: String, reply: OpenReply) {
    if !has_trace(&reply) {
        return;
    }
    let now = OffsetDateTime::now_utc();
    let mut open = locked(&OPEN_REPLIES);
    open.retain(|_, (kept_at, _)| !expired(*kept_at, now));
    open.insert(key, (now, reply));
}

/// The open request kept under `key`, a trace id or an event's key. It stays open until
/// it is finished.
pub(crate) fn open_reply_for(key: &str) -> Option<OpenReply> {
    locked(&OPEN_REPLIES)
        .get(key)
        .map(|(_, reply)| reply.clone())
}

/// Finishes a request routed as `routing`: judges it, records its metrics, ends its span
/// with `outcome`, `resolved_by` and `pane_id`, and stops keeping it open. A typed
/// request ends at its own `typed_at`; any other at `handled_at`. A typed request keeps
/// the pane's prompt for the next turn end there.
pub(crate) fn finish_phone_request(
    reply: &OpenReply,
    routing: &Routing,
    handled_at: OffsetDateTime,
) {
    let typed_at = ended_at(routing, handled_at);
    let verdict = locked(&PHONE_BOOK).judge(reply.channel, reply.reply_at, typed_at, routing);
    record_finished(reply, routing, typed_at, verdict);
}

/// When a request routed as `routing` and handled at `handled_at` ended.
fn ended_at(routing: &Routing, handled_at: OffsetDateTime) -> OffsetDateTime {
    match routing {
        Routing::Typed { typed_at, .. } => *typed_at,
        Routing::NotOwned { .. } | Routing::Paused | Routing::NoPane => handled_at,
    }
}

/// `finish_phone_request`, judged by `book` rather than the process-wide one, so a test's
/// wrong-agent results are its own.
#[cfg(test)]
pub(crate) fn finish_phone_request_in(
    book: &mut PhoneBook,
    reply: &OpenReply,
    routing: &Routing,
    handled_at: OffsetDateTime,
) {
    let typed_at = ended_at(routing, handled_at);
    let verdict = book.judge(reply.channel, reply.reply_at, typed_at, routing);
    record_finished(reply, routing, typed_at, verdict);
}

fn record_finished(
    reply: &OpenReply,
    routing: &Routing,
    typed_at: OffsetDateTime,
    verdict: PhoneVerdict,
) {
    let resolved_by = match routing {
        // A web app reply always goes to the chief's pane: no pane was chosen.
        Routing::Typed { .. } if reply.channel == WEB_APP_CHANNEL => ResolvedBy::None,
        Routing::Typed { resolved_by, .. } => *resolved_by,
        Routing::NotOwned { .. } | Routing::Paused | Routing::NoPane => ResolvedBy::None,
    };
    let instruments = instruments();
    let channel = KeyValue::new("channel", reply.channel);
    let resolved = KeyValue::new("resolved_by", resolved_by.as_str());
    if let Some(outcome) = verdict.outcome {
        instruments.phone_requests.add(
            1,
            &[
                channel.clone(),
                KeyValue::new("outcome", outcome),
                resolved.clone(),
            ],
        );
    }
    if let Some(latency) = verdict.latency {
        instruments
            .phone_request_latency
            .record(latency.as_secs_f64(), &[channel, resolved.clone()]);
    }
    if let Some((channel, resolved_by)) = verdict.wrong {
        instruments.phone_requests_wrong.add(
            1,
            &[
                KeyValue::new("channel", channel),
                KeyValue::new("resolved_by", resolved_by.as_str()),
            ],
        );
    }

    // Only a reply another Mac owns, or one routed while messaging is paused, has no
    // counted outcome.
    let outcome = verdict
        .outcome
        .unwrap_or(if matches!(routing, Routing::NotOwned { .. }) {
            "not_owned"
        } else {
            "paused"
        });
    let span = reply.cx.span();
    span.set_attribute(KeyValue::new("outcome", outcome));
    span.set_attribute(resolved);
    if let Routing::Typed { pane_id, .. } = routing {
        span.set_attribute(KeyValue::new("pane_id", pane_id.clone()));
    }
    span.end_with_timestamp(typed_at.into());

    forget_open_reply(reply);
    if let Routing::Typed { pane_id, .. } = routing {
        prompt_typed(pane_id, StartedBy::Phone, Some(reply.cx.clone()), typed_at);
    }
}

/// Stops keeping `reply` open, under every key it was kept by. A request with no trace
/// was never kept, and its all-zero trace id must not match another's.
fn forget_open_reply(reply: &OpenReply) {
    if !has_trace(reply) {
        return;
    }
    let trace_id = reply.trace_id();
    locked(&OPEN_REPLIES).retain(|_, (_, open)| open.trace_id() != trace_id);
}

/// Records that Harold typed a prompt into `pane_id` at `at`, for the next turn end there.
/// `cx` is the trace the turn joins, when the prompt has one.
pub(crate) fn prompt_typed(
    pane_id: &str,
    started_by: StartedBy,
    cx: Option<Context>,
    at: OffsetDateTime,
) {
    let now = OffsetDateTime::now_utc();
    let mut prompts = locked(&PANE_PROMPTS);
    prompts.retain(|_, prompt| !expired(prompt.typed_at, now));
    prompts.insert(
        pane_id.to_string(),
        PanePrompt {
            typed_at: at,
            started_by,
            cx,
        },
    );
}

/// The context of the prompt Harold last typed into `pane_id`, read without taking it, when
/// it was typed within `PROMPT_KEPT` of `now` and carries one.
fn peek_prompt_context(pane_id: &str, now: OffsetDateTime) -> Option<Context> {
    let prompts = locked(&PANE_PROMPTS);
    let prompt = prompts.get(pane_id)?;
    (!expired(prompt.typed_at, now))
        .then(|| prompt.cx.clone())
        .flatten()
}

/// `harold.away_message`: a message the chief sent mid-turn from `pane_id`. It continues
/// the trace of the prompt last typed into the pane while that prompt is within
/// `PROMPT_KEPT`, and is the root of a new trace after it. The prompt is left for the
/// turn's own end.
pub(crate) fn away_message_span(pane_id: &str) -> tracing::Span {
    let span = tracing::info_span!(
        target: TRACE_TARGET,
        parent: None,
        "harold.away_message",
        pane_id = crate::pane_id_for_log(pane_id),
    );
    if let Some(prompt) = peek_prompt_context(pane_id, OffsetDateTime::now_utc()) {
        // Fails only when no export layer is installed, and then there is no trace to join.
        let _ = span.set_parent(prompt);
    }
    span
}

/// `harold.app_row_sent`: the stream sending a row to the page, a child of the span that
/// stored the row.
pub(crate) struct RowSent {
    cx: Context,
}

/// Opens `harold.app_row_sent` under the span `stored` names, a W3C `traceparent`. None
/// when it names no valid span.
pub(crate) fn open_app_row_sent(stored: &str) -> Option<RowSent> {
    let parent = context_from_traceparent(stored)?;
    let span = tracer().build_with_context(SpanBuilder::from_name("harold.app_row_sent"), &parent);
    Some(RowSent {
        cx: Context::new().with_span(span),
    })
}

impl RowSent {
    /// The `traceparent` of the span, which the page's row carries.
    pub(crate) fn traceparent(&self) -> Option<String> {
        traceparent_of(&self.cx)
    }

    pub(crate) fn end(self) {
        self.cx.span().end();
    }
}

/// `harold.answers_typed`: Harold typed an `ANSWERS` line into `pane_id`, started at
/// `typing_from`. The span continues the trace of the first of `traces`, the answers'
/// `traceparent`s, and links the others. The prompt is recorded with its context, so the
/// chief's turn joins the trace. With no usable trace, such as a line of answers from the
/// dash, nothing is recorded and the turn is a root as before.
pub(crate) fn answers_typed(pane_id: &str, traces: &[String], typing_from: OffsetDateTime) {
    let mut parents = traces
        .iter()
        .filter_map(|trace| context_from_traceparent(trace));
    let Some(first) = parents.next() else {
        return;
    };
    let links: Vec<Link> = parents
        .map(|parent| Link::with_context(parent.span().span_context().clone()))
        .collect();
    let now = OffsetDateTime::now_utc();
    let span = tracer().build_with_context(
        SpanBuilder::from_name("harold.answers_typed")
            .with_start_time(typing_from)
            .with_attributes([KeyValue::new(
                "pane_id",
                crate::pane_id_for_log(pane_id).to_string(),
            )])
            .with_links(links),
        &first,
    );
    let cx = Context::new().with_span(span);
    prompt_typed(pane_id, StartedBy::Phone, Some(cx.clone()), now);
    cx.span().end_with_timestamp(now.into());
}

/// The prompt Harold last typed into `pane_id`, if it was typed within `PROMPT_KEPT` of
/// `now`. It is taken once.
pub(crate) fn take_prompt(
    pane_id: &str,
    now: OffsetDateTime,
) -> Option<(OffsetDateTime, StartedBy, Option<Context>)> {
    let prompt = locked(&PANE_PROMPTS).remove(pane_id)?;
    (!expired(prompt.typed_at, now)).then_some((prompt.typed_at, prompt.started_by, prompt.cx))
}

// ---------------------------------------------------------------------------
// Turn starts
// ---------------------------------------------------------------------------

/// What started a turn: the `started_by` attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartedBy {
    Phone,
    TimedPrompt,
    #[cfg_attr(not(feature = "calls"), allow(dead_code))]
    Voice,
    Desk,
    Unknown,
}

impl StartedBy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Phone => "phone",
            Self::TimedPrompt => "timed_prompt",
            Self::Voice => "voice",
            Self::Desk => "desk",
            Self::Unknown => "unknown",
        }
    }
}

/// When a turn started, if known, and what started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnStart {
    pub at: Option<OffsetDateTime>,
    pub started_by: StartedBy,
}

/// The spec's order: the hook's transcript time, then Harold's typing, then first busy.
/// A hook time that is not RFC 3339 counts as absent, and any time after `hook_reached`
/// is skipped. `started_by` is the typing's when Harold typed the prompt at or before
/// `hook_reached`; otherwise `Desk` when the start is known, and `Unknown` when it is not.
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
// Turns
// ---------------------------------------------------------------------------

/// When each pane first turned busy since its last turn end, with when that was kept.
static BUSY_SINCE: Mutex<BTreeMap<String, (OffsetDateTime, OffsetDateTime)>> =
    Mutex::new(BTreeMap::new());

/// Each turn end waiting to be delivered, by its event id: the `harold.turn_complete`
/// context its delivery continues, with when it was kept.
static TURN_DELIVERIES: Mutex<BTreeMap<String, (OffsetDateTime, Context)>> =
    Mutex::new(BTreeMap::new());

/// Turn ends whose event is being stored, by pane, oldest first: each
/// `harold.turn_complete` context with when it was kept. The event handler may deliver a
/// stored turn end before the handler that stored it learns its event id; it then finds
/// the turn here, by the pane.
static PENDING_TURNS: Mutex<BTreeMap<String, Vec<(OffsetDateTime, Context)>>> =
    Mutex::new(BTreeMap::new());

/// Records that the agent monitor saw `pane_id` busy at `at_ms` (milliseconds since the
/// Unix epoch). Only the first since the pane's last turn end is kept.
pub(crate) fn pane_turned_busy(pane_id: &str, at_ms: i64) {
    let Ok(at) = OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000) else {
        return;
    };
    let now = OffsetDateTime::now_utc();
    let mut busy = locked(&BUSY_SINCE);
    busy.retain(|_, (kept_at, _)| !expired(*kept_at, now));
    busy.entry(pane_id.to_string()).or_insert((now, at));
}

/// When `pane_id` first turned busy since its last turn end, if that was kept within
/// `PROMPT_KEPT`. It is taken once: the turn end that takes it starts the next run.
pub(crate) fn take_busy_since(pane_id: &str) -> Option<OffsetDateTime> {
    let (kept_at, at) = locked(&BUSY_SINCE).remove(pane_id)?;
    (!expired(kept_at, OffsetDateTime::now_utc())).then_some(at)
}

/// A turn end's trace: the live `harold.turn_complete` span, parented on `harold.turn`.
pub(crate) struct TurnTrace {
    pub turn_complete: tracing::Span,
}

/// Opens and ends `harold.turn` (from the turn's start to `hook_reached`), records
/// `harold.turns` and `harold.turn.duration`, and returns `harold.turn_complete` parented
/// on it.
///
/// - The start is the first known of the hook's transcript time, the prompt Harold typed
///   into the pane, and when the pane first turned busy (`turn_start`).
/// - `harold.turn` continues the typed prompt's trace when it has one, and starts a new
///   trace otherwise. A prompt typed after `hook_reached` belongs to the next turn: it is
///   kept for that turn and this one does not join it.
/// - `session.id` is `session_id`, or the pane id with `session.source = "pane"`.
/// - The exported pane id is the one `harold.log` writes: a malformed one is
///   `<invalid-pane-id>`.
/// - An unknown start gives a span that starts at `hook_reached` with
///   `turn.start = "unknown"`, and no duration.
pub(crate) fn begin_turn_complete(
    pane_id: &str,
    session_id: Option<&str>,
    hook_time: Option<&str>,
    hook_reached: OffsetDateTime,
) -> TurnTrace {
    let prompt = match take_prompt(pane_id, hook_reached) {
        Some((typed_at, started_by, cx)) if typed_at > hook_reached => {
            prompt_typed(pane_id, started_by, cx, typed_at);
            None
        }
        prompt => prompt,
    };
    let (typed, parent) = match prompt {
        Some((typed_at, started_by, cx)) => (Some((typed_at, started_by)), cx),
        None => (None, None),
    };
    let start = turn_start(hook_time, typed, take_busy_since(pane_id), hook_reached);

    // Exported as harold.log writes it, so a malformed pane id cannot grow the metric's
    // labels without bound.
    let pane_id = crate::pane_id_for_log(pane_id);
    let session = session_id.unwrap_or(pane_id).to_string();
    let started_by = start.started_by.as_str();
    let mut attributes = vec![
        KeyValue::new("pane_id", pane_id.to_string()),
        KeyValue::new("session.id", session.clone()),
        KeyValue::new("started_by", started_by),
    ];
    if session_id.is_none() {
        attributes.push(KeyValue::new("session.source", "pane"));
    }
    if start.at.is_none() {
        attributes.push(KeyValue::new("turn.start", "unknown"));
    }
    let parent = parent.unwrap_or_default();
    let turn = tracer().build_with_context(
        SpanBuilder::from_name("harold.turn")
            .with_start_time(start.at.unwrap_or(hook_reached))
            .with_attributes(attributes),
        &parent,
    );
    let turn_cx = parent.with_span(turn);
    turn_cx.span().end_with_timestamp(hook_reached.into());

    let instruments = instruments();
    instruments.turns.add(
        1,
        &[
            KeyValue::new("session.id", session),
            KeyValue::new("pane_id", pane_id.to_string()),
            KeyValue::new("started_by", started_by),
        ],
    );
    if let Some(at) = start.at {
        let duration = Duration::try_from(hook_reached - at).unwrap_or(Duration::ZERO);
        instruments.turn_duration.record(
            duration.as_secs_f64(),
            &[KeyValue::new("started_by", started_by)],
        );
    }

    let turn_complete =
        tracing::info_span!(target: TRACE_TARGET, "harold.turn_complete", pane_id = %pane_id);
    // Fails only when no export layer is installed, and then there is no trace to join.
    let _ = turn_complete.set_parent(turn_cx);
    TurnTrace { turn_complete }
}

/// Whether `a` and `b` hold the same span.
fn same_span(a: &Context, b: &Context) -> bool {
    a.span().span_context().span_id() == b.span().span_context().span_id()
}

/// Keeps the `harold.turn_complete` context `cx` under `pane_id` while its turn end is
/// being stored. A context with no trace is not kept.
pub(crate) fn keep_pending_turn(pane_id: &str, cx: Context) {
    if !cx.span().span_context().is_valid() {
        return;
    }
    let now = OffsetDateTime::now_utc();
    let mut pending = locked(&PENDING_TURNS);
    for turns in pending.values_mut() {
        turns.retain(|(kept_at, _)| !expired(*kept_at, now));
    }
    pending.retain(|_, turns| !turns.is_empty());
    pending
        .entry(pane_id.to_string())
        .or_default()
        .push((now, cx));
}

/// Removes the turn `cx` from the pane's pending turns, and says whether it was there.
fn remove_pending_turn(pane_id: &str, cx: &Context) -> bool {
    let mut pending = locked(&PENDING_TURNS);
    let Some(turns) = pending.get_mut(pane_id) else {
        return false;
    };
    let before = turns.len();
    turns.retain(|(_, kept)| !same_span(kept, cx));
    let removed = turns.len() < before;
    if turns.is_empty() {
        pending.remove(pane_id);
    }
    removed
}

/// The turn end `cx` on `pane_id` is stored as `event_id`: its delivery finds it by the
/// event id from now on. When the delivery has already taken it by the pane, it is not
/// kept again.
pub(crate) fn settle_pending_turn(pane_id: &str, event_id: uuid::Uuid, cx: &Context) {
    if remove_pending_turn(pane_id, cx) {
        keep_turn_delivery(event_id, cx.clone());
    }
}

/// The turn end `cx` on `pane_id` was not stored: no delivery joins it.
pub(crate) fn forget_pending_turn(pane_id: &str, cx: &Context) {
    remove_pending_turn(pane_id, cx);
}

/// The oldest turn on `pane_id` whose turn end is still being stored, if it was kept
/// within `PROMPT_KEPT`. It is taken once.
pub(crate) fn take_pending_turn(pane_id: &str) -> Option<Context> {
    let now = OffsetDateTime::now_utc();
    let mut pending = locked(&PENDING_TURNS);
    let turns = pending.get_mut(pane_id)?;
    turns.retain(|(kept_at, _)| !expired(*kept_at, now));
    let taken = (!turns.is_empty()).then(|| turns.remove(0).1);
    if turns.is_empty() {
        pending.remove(pane_id);
    }
    taken
}

/// Keeps the `harold.turn_complete` context `cx` for the delivery of the turn end
/// `event_id`. A context with no trace is not kept.
pub(crate) fn keep_turn_delivery(event_id: uuid::Uuid, cx: Context) {
    if !cx.span().span_context().is_valid() {
        return;
    }
    let now = OffsetDateTime::now_utc();
    let mut deliveries = locked(&TURN_DELIVERIES);
    deliveries.retain(|_, (kept_at, _)| !expired(*kept_at, now));
    deliveries.insert(event_id.to_string(), (now, cx));
}

/// The trace kept for the delivery of the turn end `event_id`, if it was kept within
/// `PROMPT_KEPT`. It is taken once.
pub(crate) fn take_turn_delivery(event_id: &str) -> Option<Context> {
    let (kept_at, cx) = locked(&TURN_DELIVERIES).remove(event_id)?;
    (!expired(kept_at, OffsetDateTime::now_utc())).then_some(cx)
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

    /// The guard `telemetry` asks for: the OTLP pipeline when export is on and
    /// `otlp_endpoint` is set, and none otherwise. With an endpoint that cannot be used,
    /// also the reason, and no pipeline.
    pub(crate) fn for_settings(
        telemetry: &TelemetrySettings,
        machine: &str,
    ) -> (Self, Option<opentelemetry_otlp::ExporterBuildError>) {
        let endpoint = match telemetry.otlp_endpoint.as_deref() {
            Some(endpoint) if telemetry.export => endpoint,
            _ => return (Self::off(), None),
        };
        match Pipeline::otlp(endpoint, machine, ExportHealth::logging(endpoint)) {
            Ok(pipeline) => (
                Self {
                    pipeline: Some(pipeline),
                },
                None,
            ),
            Err(error) => (Self::off(), Some(error)),
        }
    }

    /// Whether this guard's pipeline exports spans, logs and metrics.
    pub(crate) fn exporting(&self) -> bool {
        self.pipeline.is_some()
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

/// Installs the global log subscriber: `harold.log` as before and, when export is on and
/// `otlp_endpoint` is set, the OTLP export of spans and log records. Also sets the global
/// tracer and meter providers.
///
/// Must run inside the tokio runtime: the exporters' gRPC channel is made here and its
/// connection task runs on the runtime.
pub fn init_telemetry(level: &str, telemetry: &TelemetrySettings, machine: &str) -> TelemetryGuard {
    let (guard, build_failed) = TelemetryGuard::for_settings(telemetry, machine);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let harold_log = fmt::layer()
        .log_internal_errors(true)
        .with_filter(harold_log_filter(env_filter));
    let export = guard
        .pipeline
        .as_ref()
        .map(|pipeline| pipeline.layers(level));
    tracing_subscriber::registry()
        .with(harold_log)
        .with(export)
        .init();

    if let Some(error) = build_failed {
        warn!(error = %error, "telemetry export off: the collector address could not be used");
    }
    if let Some(pipeline) = &guard.pipeline {
        pipeline.install_global_providers();
    }
    guard
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
        is_trace_span(meta)
            || (meta.is_span() && is_buckwal_target(meta.target()))
            || (meta.is_event() && *meta.level() == Level::ERROR)
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
    #[cfg_attr(not(feature = "imessage"), allow(dead_code))]
    pub(crate) fn force_flush(&self) -> OTelSdkResult {
        let spans = self.tracer_provider.force_flush();
        let logs = self.logger_provider.force_flush();
        let metrics = self.meter_provider.force_flush();
        spans.and(logs).and(metrics)
    }

    fn install_global_providers(&self) {
        global::set_tracer_provider(self.tracer_provider.clone());
        global::set_meter_provider(self.meter_provider.clone());
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

/// The `os.type` of this machine: `darwin` or `linux`.
fn os_type() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// What every export carries: the service, the machine it runs on and that machine's
/// operating system.
fn resource(machine: &str) -> Resource {
    Resource::builder()
        .with_service_name("harold")
        .with_attribute(KeyValue::new("harold.machine", machine.to_string()))
        .with_attribute(KeyValue::new("os.type", os_type()))
        .build()
}

// ---------------------------------------------------------------------------
// Allowed fields only
// ---------------------------------------------------------------------------

/// Passes spans and log records on with only the attributes on `ALLOWED_FIELDS`. Span
/// events and links are dropped, and an error status loses its description. The one
/// exception is the links of `harold.answers_typed`, which hold only the ids of the other
/// answers' spans.
#[derive(Debug)]
pub(crate) struct FieldFilter<E>(E);

fn allowed_span(mut span: SpanData) -> SpanData {
    span.attributes.retain(|kv| is_allowed(kv.key.as_str()));
    span.events = SpanEvents::default();
    if span.name != "harold.answers_typed" {
        span.links = SpanLinks::default();
    }
    if matches!(span.status, Status::Error { .. }) {
        span.status = Status::error("");
    }
    span
}

/// A new log record with `record`'s body, times, severity and trace, and only its
/// allowed attributes. The SDK offers no way to remove an attribute, so the record is
/// rebuilt from a blank one. A record with no record time gets its observed time, or
/// the current time, since the tracing bridge never sets one and the local stack's
/// ingestor reads only the record time.
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
    allowed.set_timestamp(
        record
            .timestamp()
            .or(record.observed_timestamp())
            .unwrap_or_else(std::time::SystemTime::now),
    );
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
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
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

    /// The value of `span`'s attribute `key`, as text.
    pub(crate) fn span_attr(span: &SpanData, key: &str) -> Option<String> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.as_str().into_owned())
    }

    fn carries_all<'a>(
        attributes: impl Iterator<Item = &'a opentelemetry::KeyValue>,
        wanted: &[(&str, &str)],
    ) -> bool {
        let attributes: Vec<_> = attributes.collect();
        wanted.iter().all(|(key, value)| {
            attributes
                .iter()
                .any(|kv| kv.key.as_str() == *key && kv.value.as_str() == *value)
        })
    }

    /// `read` applied to the latest export of the metric `name`. Exports are cumulative,
    /// so it holds every data point recorded so far.
    fn read_latest<T>(name: &str, read: impl Fn(&AggregatedMetrics) -> Option<T>) -> Option<T> {
        metrics().iter().rev().find_map(|resource| {
            resource
                .scope_metrics()
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == name)
                .map(|metric| read(metric.data()))
        })?
    }

    /// The value of the counter `name`'s data point whose attributes are exactly
    /// `wanted`, or `None` when there is no such point.
    pub(crate) fn exact_counter_value(name: &str, wanted: &[(&str, &str)]) -> Option<u64> {
        read_latest(name, |data| {
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = data else {
                return None;
            };
            sum.data_points()
                .find(|point| {
                    point.attributes().count() == wanted.len()
                        && carries_all(point.attributes(), wanted)
                })
                .map(|point| point.value())
        })
    }

    /// The total of the counter `name` over the data points that carry every attribute
    /// in `wanted`, or `None` when none does.
    pub(crate) fn counter_value(name: &str, wanted: &[(&str, &str)]) -> Option<u64> {
        read_latest(name, |data| {
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = data else {
                return None;
            };
            let values: Vec<u64> = sum
                .data_points()
                .filter(|point| carries_all(point.attributes(), wanted))
                .map(|point| point.value())
                .collect();
            (!values.is_empty()).then(|| values.iter().sum())
        })
    }

    /// The value of the `u64` gauge `name` at its data point that carries every attribute
    /// in `wanted`, or `None` when none does.
    pub(crate) fn gauge_value(name: &str, wanted: &[(&str, &str)]) -> Option<u64> {
        read_latest(name, |data| {
            let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = data else {
                return None;
            };
            gauge
                .data_points()
                .find(|point| carries_all(point.attributes(), wanted))
                .map(|point| point.value())
        })
    }

    /// The value of the `f64` gauge `name` at its data point that carries every attribute
    /// in `wanted`, or `None` when none does.
    pub(crate) fn gauge_f64_value(name: &str, wanted: &[(&str, &str)]) -> Option<f64> {
        read_latest(name, |data| {
            let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = data else {
                return None;
            };
            gauge
                .data_points()
                .find(|point| carries_all(point.attributes(), wanted))
                .map(|point| point.value())
        })
    }

    /// The sum of the histogram `name` over the data points that carry every attribute in
    /// `wanted`, or `None` when none does.
    pub(crate) fn histogram_sum(name: &str, wanted: &[(&str, &str)]) -> Option<f64> {
        read_latest(name, |data| {
            let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = data else {
                return None;
            };
            let sums: Vec<f64> = histogram
                .data_points()
                .filter(|point| carries_all(point.attributes(), wanted))
                .map(|point| point.sum())
                .collect();
            (!sums.is_empty()).then(|| sums.iter().sum())
        })
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
