use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use tokio::runtime::Builder;
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;

use crate::api::{AgentStateSource, SourceError, SourceStream};
use crate::app::{
    App, CallAction, CallView, ConnectionState, Effect, FocusReading, RuntimeStatus, SearchState,
    Snapshot, SnapshotError,
};
use crate::cli::Options;
use crate::navigation::{NavigationError, PaneNavigator, REX_BLOCK_PREFIX, TmuxNavigator};
use crate::notes::{DEBOUNCE, NoteLine, NotesAction, NotesEvent, SharedBackend};
use crate::questions::{Outgoing, PickView, Queue, SendResult};
use crate::terminal::{TerminalError, TerminalGuard};
use crate::text::sanitize_display;

const ERROR_LIMIT: usize = 512;
const RETRY_DELAYS_MS: [u64; 6] = [250, 500, 1_000, 2_000, 4_000, 5_000];
const INPUT_CHANNEL_CAPACITY: usize = 32;
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const FOCUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);
const CALL_POLL_DURING_CALL: Duration = Duration::from_millis(500);
const CALL_POLL_OTHERWISE: Duration = Duration::from_secs(5);
const QUESTION_POLL_INTERVAL: Duration = Duration::from_secs(2);
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug)]
pub enum AppError {
    NavigationDiscovery(String),
    Terminal(TerminalError),
    Runtime(String),
    Input(String),
    Signal(String),
    Panic(String),
    ExitAndRestore { primary: String, cleanup: String },
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NavigationDiscovery(detail) => {
                write!(
                    formatter,
                    "could not inspect invoking tmux client: {detail}"
                )
            }
            Self::Terminal(error) => write!(formatter, "terminal error: {error}"),
            Self::Runtime(detail) => write!(formatter, "could not start async runtime: {detail}"),
            Self::Input(detail) => write!(formatter, "terminal input failed: {detail}"),
            Self::Signal(detail) => {
                write!(formatter, "could not install shutdown signal: {detail}")
            }
            Self::Panic(detail) => write!(formatter, "dashboard panicked: {detail}"),
            Self::ExitAndRestore { primary, cleanup } => {
                write!(formatter, "{primary}; additionally, {cleanup}")
            }
        }
    }
}

impl std::error::Error for AppError {}

impl From<TerminalError> for AppError {
    fn from(error: TerminalError) -> Self {
        Self::Terminal(error)
    }
}

pub fn run(
    options: Options,
    source: AgentStateSource,
    navigator: TmuxNavigator,
) -> Result<(), AppError> {
    let endpoint_display = sanitize_display(&options.endpoint, ERROR_LIMIT);
    let (client, navigation_issue) = resolve_client(navigator.discover_client());
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| AppError::Runtime(sanitize_error(&error.to_string())))?;
    let shutdown = runtime.block_on(async { ShutdownSignals::new() })?;
    let mut terminal = TerminalGuard::acquire()?;

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(run_async(
            &mut terminal,
            &source,
            &navigator,
            client,
            navigation_issue,
            shutdown,
            endpoint_display,
        ))
    }));
    let restore_result = terminal.restore().map_err(AppError::from);

    match outcome {
        Ok(Ok(())) => restore_result,
        Ok(Err(error)) => Err(merge_exit(error, restore_result)),
        Err(payload) => {
            let error = AppError::Panic(panic_detail(payload));
            Err(merge_exit(error, restore_result))
        }
    }
}

fn merge_exit(primary: AppError, restore: Result<(), AppError>) -> AppError {
    match restore {
        Ok(()) => primary,
        Err(cleanup) => AppError::ExitAndRestore {
            primary: primary.to_string(),
            cleanup: cleanup.to_string(),
        },
    }
}

async fn run_async(
    terminal: &mut TerminalGuard,
    source: &AgentStateSource,
    navigator: &TmuxNavigator,
    client: Option<String>,
    navigation_issue: Option<RuntimeStatus>,
    mut shutdown: ShutdownSignals,
    endpoint_display: String,
) -> Result<(), AppError> {
    let mut core = RuntimeCore::new_with_endpoint(client, &endpoint_display);
    if let Some(status) = navigation_issue {
        core.set_navigation_issue(status);
    }
    let mut input = InputPump::start();
    run_loop(
        source,
        &mut core,
        &mut input,
        &mut shutdown,
        terminal,
        navigator,
        &SystemClock,
    )
    .await
}

/// Opens a stream and consumes it, again and again, until a quit key or a shutdown.
async fn run_loop<S, N, T>(
    source: &S,
    core: &mut RuntimeCore,
    input: &mut InputPump,
    shutdown: &mut impl ShutdownPort,
    terminal: &mut T,
    navigator: &N,
    clock: &impl ClockPort,
) -> Result<(), AppError>
where
    S: SourcePort,
    N: PaneNavigator,
    T: ScreenPort,
{
    draw(terminal, &core.app, clock)?;
    let mut retry_immediately = true;

    loop {
        if !retry_immediately {
            match wait_for_retry(core, input, shutdown, terminal, navigator, clock).await? {
                Control::Quit => return Ok(()),
                Control::Retry => redraw_for_retry(terminal, &core.app, clock)?,
                Control::Continue
                | Control::SetTtsMuted(_)
                | Control::SetMessagingPaused(_)
                | Control::Call(_)
                | Control::Answer(_)
                | Control::Tick { .. }
                | Control::SavePicks { .. }
                | Control::OpenLink(_) => {}
            }
        }
        retry_immediately = false;

        let stream =
            match open_stream(source, core, input, shutdown, terminal, navigator, clock).await? {
                OpenOutcome::Stream(stream) => stream,
                OpenOutcome::RetryNow => {
                    redraw_for_retry(terminal, &core.app, clock)?;
                    retry_immediately = true;
                    continue;
                }
                OpenOutcome::RetryLater => continue,
                OpenOutcome::Quit => return Ok(()),
            };
        core.begin_stream();

        let outcome =
            consume_stream(stream, core, input, shutdown, terminal, navigator, clock).await;
        // The block lives only while a stream does: without Harold nothing can be
        // answered, so it hides until a stream opens and the queue is read again.
        core.app.hide_questions();
        core.app.close_notes();
        core.notes_backend = None;
        match outcome {
            Ok(Control::Quit) => return Ok(()),
            Ok(Control::Retry) => {
                redraw_for_retry(terminal, &core.app, clock)?;
                retry_immediately = true;
            }
            Ok(
                Control::Continue
                | Control::SetTtsMuted(_)
                | Control::SetMessagingPaused(_)
                | Control::Call(_)
                | Control::Answer(_)
                | Control::Tick { .. }
                | Control::SavePicks { .. }
                | Control::OpenLink(_),
            ) => {
                let detail = core
                    .app
                    .runtime_status()
                    .map(runtime_status_detail)
                    .unwrap_or_else(|| "Harold stream closed".to_owned());
                core.retry_after(&detail);
                draw(terminal, &core.app, clock)?;
            }
            Err(error) => return Err(error),
        }
    }
}

enum OpenOutcome<S> {
    Stream(S),
    RetryNow,
    RetryLater,
    Quit,
}

trait SourcePort {
    type Stream: StreamPort;

    fn open_stream(&self) -> BoxFuture<'_, Result<Self::Stream, SourceError>>;
}

trait StreamPort: Sized {
    fn receive(&mut self) -> BoxFuture<'_, Option<Result<Snapshot, SourceError>>>;
    fn close_stream(self) -> BoxFuture<'static, ()>;
    fn set_tts_muted(&mut self, _muted: bool) -> BoxFuture<'_, Result<(), SourceError>> {
        Box::pin(async { Ok(()) })
    }
    fn set_messaging_paused(&mut self, _paused: bool) -> BoxFuture<'_, Result<(), SourceError>> {
        Box::pin(async { Ok(()) })
    }
    fn call(&mut self, _action: CallAction) -> BoxFuture<'_, Result<CallView, SourceError>> {
        Box::pin(async { Err(SourceError::Transport("call control unavailable".into())) })
    }
    fn list_questions(&mut self) -> BoxFuture<'_, Result<Queue, SourceError>> {
        Box::pin(async { Err(SourceError::Transport("question tool unavailable".into())) })
    }
    fn list_queue_now(&mut self) -> BoxFuture<'_, Result<Option<String>, SourceError>> {
        Box::pin(async { Err(SourceError::Transport("backlog tool unavailable".into())) })
    }
    fn answer_question<'a>(&'a mut self, _out: &'a Outgoing) -> BoxFuture<'a, SendResult> {
        Box::pin(async { SendResult::Unavailable })
    }
    fn tick_step<'a>(
        &'a mut self,
        _id: &'a str,
        _version: u32,
        _step: u32,
        _ticked: bool,
    ) -> BoxFuture<'a, SendResult> {
        Box::pin(async { SendResult::Unavailable })
    }
    fn save_picks<'a>(
        &'a mut self,
        _id: &'a str,
        _version: u32,
        _picks: &'a [PickView],
    ) -> BoxFuture<'a, SendResult> {
        Box::pin(async { SendResult::Unavailable })
    }
    /// The knowledge tool, for the notes search; none when this stream cannot reach it.
    fn notes_backend(&self) -> Option<SharedBackend> {
        None
    }
}

impl SourcePort for AgentStateSource {
    type Stream = SourceStream;

    fn open_stream(&self) -> BoxFuture<'_, Result<Self::Stream, SourceError>> {
        self.open()
    }
}

impl StreamPort for SourceStream {
    fn receive(&mut self) -> BoxFuture<'_, Option<Result<Snapshot, SourceError>>> {
        Box::pin(self.recv())
    }

    fn close_stream(self) -> BoxFuture<'static, ()> {
        Box::pin(self.close())
    }

    fn set_tts_muted(&mut self, muted: bool) -> BoxFuture<'_, Result<(), SourceError>> {
        Box::pin(self.request_tts_muted(muted))
    }

    fn set_messaging_paused(&mut self, paused: bool) -> BoxFuture<'_, Result<(), SourceError>> {
        Box::pin(self.request_messaging_paused(paused))
    }

    fn call(&mut self, action: CallAction) -> BoxFuture<'_, Result<CallView, SourceError>> {
        Box::pin(self.request_call(action))
    }

    // The inherent methods share these names, so they are called by path: a method
    // call here would pick the trait's `&mut self` method and recurse.
    fn list_questions(&mut self) -> BoxFuture<'_, Result<Queue, SourceError>> {
        Box::pin(SourceStream::list_questions(self))
    }

    fn list_queue_now(&mut self) -> BoxFuture<'_, Result<Option<String>, SourceError>> {
        Box::pin(SourceStream::list_queue_now(self))
    }

    fn answer_question<'a>(&'a mut self, out: &'a Outgoing) -> BoxFuture<'a, SendResult> {
        Box::pin(SourceStream::answer_question(self, out))
    }

    fn tick_step<'a>(
        &'a mut self,
        id: &'a str,
        version: u32,
        step: u32,
        ticked: bool,
    ) -> BoxFuture<'a, SendResult> {
        Box::pin(SourceStream::tick_step(self, id, version, step, ticked))
    }

    fn save_picks<'a>(
        &'a mut self,
        id: &'a str,
        version: u32,
        picks: &'a [PickView],
    ) -> BoxFuture<'a, SendResult> {
        Box::pin(SourceStream::save_picks(self, id, version, picks))
    }

    fn notes_backend(&self) -> Option<SharedBackend> {
        SourceStream::notes_backend(self)
    }
}

trait ScreenPort {
    fn draw_screen(&mut self, app: &App, now_ms: i64) -> Result<(), TerminalError>;
    /// Clears the whole screen, so the next draw repaints every cell.
    fn clear_screen(&mut self) -> Result<(), TerminalError>;
}

impl ScreenPort for TerminalGuard {
    fn draw_screen(&mut self, app: &App, now_ms: i64) -> Result<(), TerminalError> {
        self.draw(app, now_ms)
    }

    fn clear_screen(&mut self) -> Result<(), TerminalError> {
        self.clear()
    }
}

async fn open_stream<S, N, T>(
    source: &S,
    core: &mut RuntimeCore,
    input: &mut InputPump,
    shutdown: &mut impl ShutdownPort,
    terminal: &mut T,
    navigator: &N,
    clock: &impl ClockPort,
) -> Result<OpenOutcome<S::Stream>, AppError>
where
    S: SourcePort,
    N: PaneNavigator,
    T: ScreenPort,
{
    let open = source.open_stream();
    tokio::pin!(open);
    loop {
        tokio::select! {
            result = &mut open => {
                return match result {
                    Ok(stream) => Ok(OpenOutcome::Stream(stream)),
                    Err(error) => {
                        core.source_failed(error.clone());
                        core.retry_after(&error.to_string());
                        draw(terminal, &core.app, clock)?;
                        Ok(OpenOutcome::RetryLater)
                    }
                };
            }
            item = input.recv() => {
                match handle_input(item, core, navigator)? {
                    Control::Quit => return Ok(OpenOutcome::Quit),
                    Control::Retry => return Ok(OpenOutcome::RetryNow),
                    Control::Continue
                    | Control::SetTtsMuted(_)
                    | Control::SetMessagingPaused(_)
                    | Control::Call(_)
                    | Control::Answer(_)
                    | Control::Tick { .. }
                    | Control::SavePicks { .. }
                    | Control::OpenLink(_) => draw(terminal, &core.app, clock)?,
                }
            }
            () = shutdown.recv_shutdown() => return Ok(OpenOutcome::Quit),
        }
    }
}

async fn consume_stream<S, N, T>(
    mut stream: S,
    core: &mut RuntimeCore,
    input: &mut InputPump,
    shutdown: &mut impl ShutdownPort,
    terminal: &mut T,
    navigator: &N,
    clock: &impl ClockPort,
) -> Result<Control, AppError>
where
    S: StreamPort,
    N: PaneNavigator,
    T: ScreenPort,
{
    core.notes_backend = stream.notes_backend();
    core.probe_notes();
    let result = async {
        // The call's state is asked for as the stream opens, so the button finds the
        // chief's pane before any call, and then on every tick of the poll. Each poll
        // is awaited inline, like the mute and pause requests: Harold answers Status
        // synchronously from the call's last state, so the loop waits only one round
        // trip, and at most VOICE_REQUEST_TIMEOUT (2 s) when Harold hangs. A `c` or `t`
        // press is awaited inline too, for up to CALL_REQUEST_TIMEOUT (8 s), because a
        // STOP answers only once the call has cleaned up, and closing the worker alone
        // may take 5 s.
        //
        // The question queue is read the same way: as the stream opens, every
        // QUESTION_POLL_INTERVAL, before each key that goes to the question rows, and
        // after each answer. Each read is awaited inline and
        // bounded by VOICE_REQUEST_TIMEOUT. The chief's Now item is read beside the
        // queue as the stream opens and on each QUESTION_POLL_INTERVAL, the same way,
        // except when that queue read timed out (see `read_now`).
        let polled = stream.call(CallAction::Status).await;
        if core.call_polled(polled) {
            draw(terminal, &core.app, clock)?;
        }
        let listed = stream.list_questions().await;
        let questions_timed_out = matches!(listed, Err(SourceError::TimedOut(_)));
        let changed = core.questions_listed(listed).changed;
        let now = read_now(&mut stream, questions_timed_out).await;
        let now_changed = core.now_listed(now);
        if changed || now_changed {
            draw(terminal, &core.app, clock)?;
        }
        let poll = sleep(call_poll_interval(core.app.call.as_ref()));
        tokio::pin!(poll);
        let question_poll = sleep(core.question_poll_interval);
        tokio::pin!(question_poll);
        loop {
            tokio::select! {
            () = &mut poll => {
                let polled = stream.call(CallAction::Status).await;
                if core.call_polled(polled) {
                    draw(terminal, &core.app, clock)?;
                }
                poll.as_mut().reset(next_call_poll(&core.app));
            }
            () = &mut question_poll => {
                let listed = stream.list_questions().await;
                let questions_timed_out = matches!(listed, Err(SourceError::TimedOut(_)));
                let changed = core.questions_refreshed(listed);
                let now = read_now(&mut stream, questions_timed_out).await;
                let now_changed = core.now_listed(now);
                if changed || now_changed {
                    draw(terminal, &core.app, clock)?;
                }
                question_poll
                    .as_mut()
                    .reset(tokio::time::Instant::now() + core.question_poll_interval);
            }
            item = stream.receive() => {
                match item {
                    Some(Ok(snapshot)) => {
                        if core.accept_snapshot(snapshot, clock.now_ms()) == StreamControl::Reconnect {
                            draw(terminal, &core.app, clock)?;
                            break Ok(Control::Continue);
                        }
                        draw(terminal, &core.app, clock)?;
                    }
                    Some(Err(error)) => {
                        core.source_failed(error);
                        draw(terminal, &core.app, clock)?;
                        break Ok(Control::Continue);
                    }
                    None => {
                        core.app.mark_disconnected();
                        core.app.set_runtime_status(RuntimeStatus::SourceError("Harold stream closed".into()));
                        draw(terminal, &core.app, clock)?;
                        break Ok(Control::Continue);
                    }
                }
            }
            Some(event) = core.notes_events.recv() => {
                core.notes_arrived(event, navigator);
                draw(terminal, &core.app, clock)?;
            }
            item = input.recv() => {
                if let Some(Ok(RuntimeInput::Key(key))) = &item {
                    // A key meant for a question that just changed, went or hid is
                    // dropped: the user has not seen what it would act on. The change
                    // is found by this key's own read, or was flagged by an earlier one.
                    // A key for a block that this read hid (a notice it cleared) is
                    // dropped too, rather than handed to the list. Only a key that would
                    // still reach the question rows is dropped: the global keys and every
                    // key on the list act, whatever the flag says.
                    let flagged = std::mem::take(&mut core.drop_next_block_key);
                    if core.app.wants_fresh_questions(key) {
                        let listed = stream.list_questions().await;
                        let changed = core.questions_listed(listed).question_changed;
                        let hid = !core.app.questions.is_visible() && !core.app.swallowing_keys();
                        if flagged || changed || hid {
                            draw(terminal, &core.app, clock)?;
                            continue;
                        }
                    }
                }
                let control = handle_input(item, core, navigator)?;
                if let Control::Answer(out) = &control {
                    let result = stream.answer_question(out).await;
                    core.app.question_sent(&out.id, result);
                    let listed = stream.list_questions().await;
                    core.questions_refreshed(listed);
                    // A full interval before the next poll, so a notice this read
                    // kept stays a few seconds.
                    question_poll
                        .as_mut()
                        .reset(tokio::time::Instant::now() + core.question_poll_interval);
                    draw(terminal, &core.app, clock)?;
                    continue;
                }
                if let Control::Tick {
                    id,
                    version,
                    step,
                    ticked,
                } = &control
                {
                    let result = stream.tick_step(id, *version, *step, *ticked).await;
                    core.app.question_ticked(id, *step, *ticked, result);
                    let listed = stream.list_questions().await;
                    core.questions_refreshed(listed);
                    question_poll
                        .as_mut()
                        .reset(tokio::time::Instant::now() + core.question_poll_interval);
                    draw(terminal, &core.app, clock)?;
                    continue;
                }
                if let Control::SavePicks { id, version, picks } = &control {
                    let result = stream.save_picks(id, *version, picks).await;
                    core.app.question_picks_saved(id, *version, result);
                    // A saved pick changes nothing the listing shows; a refusal means the
                    // question moved on, so read it again.
                    if result != SendResult::Recorded {
                        let listed = stream.list_questions().await;
                        core.questions_refreshed(listed);
                        question_poll
                            .as_mut()
                            .reset(tokio::time::Instant::now() + core.question_poll_interval);
                    }
                    draw(terminal, &core.app, clock)?;
                    continue;
                }
                if let Control::OpenLink(link) = &control {
                    core.open_link(link, navigator);
                    draw(terminal, &core.app, clock)?;
                    continue;
                }
                if let Control::SetTtsMuted(muted) = control {
                    let result = stream.set_tts_muted(muted).await;
                    core.voice_result(result);
                    draw(terminal, &core.app, clock)?;
                    continue;
                }
                if let Control::SetMessagingPaused(paused) = control {
                    let result = stream.set_messaging_paused(paused).await;
                    core.messaging_result(result);
                    draw(terminal, &core.app, clock)?;
                    continue;
                }
                if let Control::Call(action) = control {
                    let result = stream.call(action).await;
                    core.call_result(action, result);
                    draw(terminal, &core.app, clock)?;
                    poll.as_mut().reset(next_call_poll(&core.app));
                    continue;
                }
                draw(terminal, &core.app, clock)?;
                if control != Control::Continue {
                    break Ok(control);
                }
            }
            () = shutdown.recv_shutdown() => break Ok(RuntimeCore::shutdown()),
            }
        }
    }
    .await;
    stream.close_stream().await;
    result
}

/// The Now read that follows a question read. When the question read timed out,
/// Harold is not answering, so the Now read is skipped and the line hides, as a failed
/// read would hide it: a hung Harold holds the keys for one timeout, not two. Any other
/// question error (an Unimplemented question tool, say) answers at once, so the Now read
/// still happens.
async fn read_now<S: StreamPort>(
    stream: &mut S,
    questions_timed_out: bool,
) -> Result<Option<String>, SourceError> {
    if questions_timed_out {
        return Err(SourceError::Transport(
            "skipped after a timed-out question read".into(),
        ));
    }
    stream.list_queue_now().await
}

async fn wait_for_retry(
    core: &mut RuntimeCore,
    input: &mut InputPump,
    shutdown: &mut impl ShutdownPort,
    terminal: &mut impl ScreenPort,
    navigator: &impl PaneNavigator,
    clock: &impl ClockPort,
) -> Result<Control, AppError> {
    let delay = match core.app.runtime_status() {
        Some(RuntimeStatus::Retrying { delay_ms, .. }) => Duration::from_millis(*delay_ms),
        _ => Duration::ZERO,
    };
    let timer = sleep(delay);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            () = &mut timer => return Ok(Control::Continue),
            item = input.recv() => {
                let control = handle_input(item, core, navigator)?;
                draw(terminal, &core.app, clock)?;
                if !matches!(
                    control,
                    Control::Continue
                        | Control::SetTtsMuted(_)
                        | Control::SetMessagingPaused(_)
                        | Control::Call(_)
                        | Control::Answer(_)
                        | Control::Tick { .. }
                        | Control::SavePicks { .. }
                        | Control::OpenLink(_)
                ) {
                    return Ok(control);
                }
            }
            () = shutdown.recv_shutdown() => return Ok(RuntimeCore::shutdown()),
        }
    }
}

/// How long until the call's state is asked for again: half a second while a call
/// is on, so the button keeps up with it, and five seconds otherwise.
fn call_poll_interval(call: Option<&CallView>) -> Duration {
    if call.is_some_and(|call| call.state.in_call()) {
        CALL_POLL_DURING_CALL
    } else {
        CALL_POLL_OTHERWISE
    }
}

fn next_call_poll(app: &App) -> tokio::time::Instant {
    tokio::time::Instant::now() + call_poll_interval(app.call.as_ref())
}

fn handle_input(
    item: Option<Result<RuntimeInput, String>>,
    core: &mut RuntimeCore,
    navigator: &impl PaneNavigator,
) -> Result<Control, AppError> {
    match item {
        Some(Ok(RuntimeInput::Key(key))) => Ok(core.handle_key(key, navigator)),
        Some(Ok(RuntimeInput::Redraw)) => Ok(Control::Continue),
        Some(Ok(RuntimeInput::Focus(reading))) => {
            core.app.observe_focus(reading);
            Ok(Control::Continue)
        }
        Some(Err(detail)) => Err(AppError::Input(detail)),
        None => Err(AppError::Input("terminal input worker stopped".into())),
    }
}

trait ClockPort {
    fn now_ms(&self) -> i64;
}

struct SystemClock;

impl ClockPort for SystemClock {
    fn now_ms(&self) -> i64 {
        local_now_ms()
    }
}

fn draw(terminal: &mut impl ScreenPort, app: &App, clock: &impl ClockPort) -> Result<(), AppError> {
    terminal
        .draw_screen(app, clock.now_ms())
        .map_err(AppError::from)
}

/// Ctrl+L clears the screen and draws it in full, then the stream is retried. Only
/// Ctrl+L asks for a retry, so this runs once for each press.
fn redraw_for_retry(
    terminal: &mut impl ScreenPort,
    app: &App,
    clock: &impl ClockPort,
) -> Result<(), AppError> {
    terminal.clear_screen()?;
    draw(terminal, app, clock)
}

fn local_now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn sanitize_error(detail: &str) -> String {
    sanitize_display(detail, ERROR_LIMIT)
}

fn resolve_client(
    result: Result<Option<String>, NavigationError>,
) -> (Option<String>, Option<RuntimeStatus>) {
    match result {
        Ok(Some(client)) => (Some(client), None),
        Ok(None) => (None, Some(RuntimeStatus::NavigationUnavailable)),
        Err(error) => (
            None,
            Some(RuntimeStatus::NavigationFailed(sanitize_error(
                &error.to_string(),
            ))),
        ),
    }
}

fn runtime_status_detail(status: &RuntimeStatus) -> String {
    match status {
        RuntimeStatus::Retrying { detail, .. }
        | RuntimeStatus::NavigationFailed(detail)
        | RuntimeStatus::VoiceFailed(detail)
        | RuntimeStatus::MessagingFailed(detail)
        | RuntimeStatus::CallFailed(detail)
        | RuntimeStatus::SourceError(detail) => detail.clone(),
        RuntimeStatus::NavigationUnavailable => "navigation unavailable".into(),
    }
}

fn panic_detail(payload: Box<dyn std::any::Any + Send>) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic payload");
    sanitize_error(detail)
}

enum InputMessage {
    Key(KeyEvent),
    Redraw,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuntimeInput {
    Key(KeyEvent),
    Redraw,
    Focus(FocusReading),
}

type FocusReceiver = watch::Receiver<Option<FocusReading>>;

struct InputPump {
    receiver: mpsc::Receiver<InputMessage>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    focus_worker: Option<JoinHandle<()>>,
    focus: Option<FocusReceiver>,
    redraw_pending: Arc<AtomicBool>,
}

struct RedrawCadence {
    next: Instant,
}

impl RedrawCadence {
    fn new(now: Instant) -> Self {
        Self {
            next: now + REDRAW_INTERVAL,
        }
    }

    fn take_if_due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + REDRAW_INTERVAL;
        true
    }
}

trait FocusProbe: Send + 'static {
    fn read(&mut self) -> FocusReading;
}

/// Set when the snapshot's agents have `block:` ids (Rex), cleared when they have `%` ids (tmux).
static AGENTS_ARE_IN_REX: AtomicBool = AtomicBool::new(false);

/// Reads the one multiplexer the agents run in. The other one may run too, so the order
/// of the two is not used to choose.
fn read_focus(
    agents_are_in_rex: bool,
    tmux: impl FnOnce() -> Option<String>,
    rex: impl FnOnce() -> Option<String>,
) -> FocusReading {
    let pane = if agents_are_in_rex { rex() } else { tmux() };
    pane.map_or(FocusReading::Unavailable, FocusReading::Active)
}

struct TmuxFocusProbe;

impl FocusProbe for TmuxFocusProbe {
    fn read(&mut self) -> FocusReading {
        // Built per read: TmuxNavigator's ports are not Send, and construction is free.
        let navigator = TmuxNavigator::new();
        read_focus(
            AGENTS_ARE_IN_REX.load(Ordering::Acquire),
            || navigator.focused_pane().ok().flatten(),
            || navigator.rex_focused_block().ok().flatten(),
        )
    }
}

/// Publishes the latest reading on a `watch` channel, only when it changed. A watch
/// is never full, so the newest reading can neither block nor be lost, and the input
/// channel keeps a single sender (the input worker) so it closes when that worker dies.
fn spawn_focus_worker(
    mut probe: impl FocusProbe,
    sender: watch::Sender<Option<FocusReading>>,
    cancelled: Arc<AtomicBool>,
    interval: Duration,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut next = Instant::now();
        while !cancelled.load(Ordering::Acquire) && !sender.is_closed() {
            if Instant::now() >= next {
                next = Instant::now() + interval;
                let reading = probe.read();
                sender.send_if_modified(|current| {
                    let changed = current.as_ref() != Some(&reading);
                    if changed {
                        *current = Some(reading);
                    }
                    changed
                });
            }
            std::thread::sleep(interval.min(INPUT_POLL_INTERVAL));
        }
    })
}

trait TerminalInput: Send + 'static {
    fn poll(&mut self, timeout: Duration) -> std::io::Result<bool>;
    fn read(&mut self) -> std::io::Result<Event>;
}

struct CrosstermInput;

impl TerminalInput for CrosstermInput {
    fn poll(&mut self, timeout: Duration) -> std::io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> std::io::Result<Event> {
        event::read()
    }
}

impl InputPump {
    fn start() -> Self {
        Self::start_with_focus(CrosstermInput, TmuxFocusProbe)
    }

    fn start_with_focus(input: impl TerminalInput, probe: impl FocusProbe) -> Self {
        let mut pump = Self::start_with(input);
        let (sender, receiver) = watch::channel(None);
        pump.focus = Some(receiver);
        pump.focus_worker = Some(spawn_focus_worker(
            probe,
            sender,
            Arc::clone(&pump.cancelled),
            FOCUS_POLL_INTERVAL,
        ));
        pump
    }

    fn start_with(mut input: impl TerminalInput) -> Self {
        let (sender, receiver) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        let cancelled = Arc::new(AtomicBool::new(false));
        let redraw_pending = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_redraw_pending = Arc::clone(&redraw_pending);
        let worker = std::thread::spawn(move || {
            let mut redraw = RedrawCadence::new(Instant::now());
            while !worker_cancelled.load(Ordering::Acquire) {
                match input.poll(INPUT_POLL_INTERVAL) {
                    Ok(true) => match input.read() {
                        Ok(Event::Key(key))
                            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                        {
                            if sender.try_send(InputMessage::Key(key)).is_err()
                                && sender.is_closed()
                            {
                                break;
                            }
                        }
                        Ok(Event::Resize(_, _)) => {
                            queue_redraw(&sender, &worker_redraw_pending);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = sender
                                .try_send(InputMessage::Error(sanitize_error(&error.to_string())));
                            break;
                        }
                    },
                    Ok(false) => {}
                    Err(error) => {
                        let _ = sender
                            .try_send(InputMessage::Error(sanitize_error(&error.to_string())));
                        break;
                    }
                }
                if redraw.take_if_due(Instant::now()) {
                    queue_redraw(&sender, &worker_redraw_pending);
                }
            }
        });
        Self {
            receiver,
            cancelled,
            worker: Some(worker),
            focus_worker: None,
            focus: None,
            redraw_pending,
        }
    }

    #[cfg(test)]
    fn channel() -> (mpsc::Sender<InputMessage>, Self) {
        let (sender, receiver) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        (
            sender,
            Self {
                receiver,
                cancelled: Arc::new(AtomicBool::new(false)),
                worker: None,
                focus_worker: None,
                focus: None,
                redraw_pending: Arc::new(AtomicBool::new(false)),
            },
        )
    }

    async fn recv(&mut self) -> Option<Result<RuntimeInput, String>> {
        // Input first: the main channel has one sender (the input worker), so it
        // closing means that worker died and ends the stream even while focus lives.
        let message = tokio::select! {
            biased;
            message = self.receiver.recv() => message,
            Some(reading) = next_focus(&mut self.focus) => {
                return Some(Ok(RuntimeInput::Focus(reading)));
            }
        };
        message.map(|message| match message {
            InputMessage::Key(key) => Ok(RuntimeInput::Key(key)),
            InputMessage::Redraw => {
                self.redraw_pending.store(false, Ordering::Release);
                Ok(RuntimeInput::Redraw)
            }
            InputMessage::Error(detail) => Err(detail),
        })
    }
}

/// The next published focus reading; `None` (disabling the select branch) when there is
/// no focus worker or it has gone away.
async fn next_focus(focus: &mut Option<FocusReceiver>) -> Option<FocusReading> {
    let receiver = focus.as_mut()?;
    if receiver.changed().await.is_err() {
        *focus = None;
        return None;
    }
    receiver.borrow_and_update().clone()
}

fn queue_redraw(sender: &mpsc::Sender<InputMessage>, pending: &AtomicBool) {
    if pending
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    if sender.try_send(InputMessage::Redraw).is_err() {
        pending.store(false, Ordering::Release);
    }
}

impl Drop for InputPump {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        // Detached, not joined: a hung tmux must not delay terminal restore on quit. The
        // worker exits at its next loop check, or when its send finds the receiver gone.
        drop(self.focus_worker.take());
    }
}

struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

trait ShutdownPort {
    fn recv_shutdown(&mut self) -> BoxFuture<'_, ()>;
}

impl ShutdownSignals {
    fn new() -> Result<Self, AppError> {
        use tokio::signal::unix::{SignalKind, signal};
        let interrupt = signal(SignalKind::interrupt())
            .map_err(|error| AppError::Signal(sanitize_error(&error.to_string())))?;
        let terminate = signal(SignalKind::terminate())
            .map_err(|error| AppError::Signal(sanitize_error(&error.to_string())))?;
        Ok(Self {
            interrupt,
            terminate,
        })
    }
}

impl ShutdownPort for ShutdownSignals {
    fn recv_shutdown(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::select! {
                _ = self.interrupt.recv() => {}
                _ = self.terminate.recv() => {}
            }
        })
    }
}

#[derive(Default)]
struct Backoff {
    attempt: usize,
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let index = self.attempt.min(RETRY_DELAYS_MS.len() - 1);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(RETRY_DELAYS_MS[index])
    }

    fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Control {
    Continue,
    Retry,
    Quit,
    SetTtsMuted(bool),
    SetMessagingPaused(bool),
    Call(CallAction),
    Answer(Outgoing),
    SavePicks {
        id: String,
        version: u32,
        picks: Vec<PickView>,
    },
    Tick {
        id: String,
        version: u32,
        step: u32,
        ticked: bool,
    },
    OpenLink(String),
}

/// What a read of the question queue did to the block.
struct QuestionsListed {
    changed: bool,
    /// The question in view changed: its id, version or mode, or it hid or appeared.
    question_changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamControl {
    Continue,
    Reconnect,
}

struct RuntimeCore {
    app: App,
    client: Option<String>,
    first_snapshot_pending: bool,
    backoff: Backoff,
    navigation_issue: Option<RuntimeStatus>,
    endpoint_display: String,
    /// How often the question queue is read; QUESTION_POLL_INTERVAL, shorter in tests.
    question_poll_interval: Duration,
    /// A read no key asked for changed the question under the cursor: the next key, if
    /// it still goes to the question rows, is dropped. Any key clears it.
    drop_next_block_key: bool,
    /// The knowledge tool, while a stream is open.
    notes_backend: Option<SharedBackend>,
    /// The notes search's answers, taken by the stream loop.
    notes_events: mpsc::UnboundedReceiver<NotesEvent>,
    notes_sender: mpsc::UnboundedSender<NotesEvent>,
    /// The newest read's number: a typing pause that is no longer the newest does not search.
    notes_latest: Arc<std::sync::atomic::AtomicU64>,
}

impl RuntimeCore {
    #[cfg(test)]
    fn new(client: Option<String>) -> Self {
        Self::new_with_endpoint(client, "http://127.0.0.1:50060")
    }

    fn new_with_endpoint(client: Option<String>, endpoint: &str) -> Self {
        let navigation_issue = client
            .is_none()
            .then_some(RuntimeStatus::NavigationUnavailable);
        let (notes_sender, notes_events) = mpsc::unbounded_channel();
        Self {
            app: App::new(
                ConnectionState::Connecting,
                Snapshot {
                    harold_version: None,
                    tts_muted: false,
                    messaging_paused: false,
                    through_event_version: 0,
                    server_time_ms: 0,
                    monitor_health: Vec::new(),
                    rows: Vec::new(),
                },
                SearchState {
                    query: String::new(),
                    editing: false,
                },
                None,
            ),
            client,
            first_snapshot_pending: true,
            backoff: Backoff::default(),
            navigation_issue,
            endpoint_display: sanitize_display(endpoint, ERROR_LIMIT),
            question_poll_interval: QUESTION_POLL_INTERVAL,
            drop_next_block_key: false,
            notes_backend: None,
            notes_events,
            notes_sender,
            notes_latest: Arc::default(),
        }
    }

    fn voice_result(&mut self, result: Result<(), SourceError>) {
        self.request_result(result, RuntimeStatus::VoiceFailed);
    }

    fn messaging_result(&mut self, result: Result<(), SourceError>) {
        self.request_result(result, RuntimeStatus::MessagingFailed);
    }

    /// Takes a poll's answer, and reports whether the call's view changed. A failed
    /// poll hides the button and sets no status, so an older Harold without the RPC
    /// shows nothing.
    fn call_polled(&mut self, result: Result<CallView, SourceError>) -> bool {
        let view = result.ok();
        let changed = self.app.call != view;
        self.app.observe_call(view);
        changed
    }

    /// Takes a read of the question queue. A failed read hides the block with no notice
    /// and sets no status, as a failed call poll does, so an older Harold without the
    /// tool shows nothing.
    fn questions_listed(&mut self, result: Result<Queue, SourceError>) -> QuestionsListed {
        let before = self.app.questions.clone();
        match result {
            Ok(queue) => self.app.observe_questions(queue),
            Err(_) => self.app.hide_questions(),
        }
        QuestionsListed {
            changed: self.app.questions != before,
            question_changed: self.app.questions.shown() != before.shown(),
        }
    }

    /// Takes a read no key asked for: the poll, or the read after an answer. If it changes the question in view while the cursor is on its rows, the
    /// next key for them is dropped, as it was typed for what was there before. A read that
    /// hides the rows moves the cursor to the chief's row, and the next key acts there. Reports
    /// whether the block changed at all.
    fn questions_refreshed(&mut self, result: Result<Queue, SourceError>) -> bool {
        let listed = self.questions_listed(result);
        if listed.question_changed && self.app.cursor_on_question() {
            self.drop_next_block_key = true;
        }
        listed.changed
    }

    /// Takes a read of the chief's Now item, and reports whether the line changed. A failed
    /// read hides the line and sets no status, as a failed question read does.
    fn now_listed(&mut self, result: Result<Option<String>, SourceError>) -> bool {
        self.app.observe_now(result.ok().flatten())
    }

    /// Asks whether Harold serves the knowledge tool; the answer says whether `s` does anything.
    fn probe_notes(&self) {
        let Some(backend) = self.notes_backend.clone() else {
            return;
        };
        let sender = self.notes_sender.clone();
        tokio::spawn(async move {
            let _ = sender.send(NotesEvent::Available(backend.available().await));
        });
    }

    /// Starts a notes read, or the opening of a vault note's file, without holding the
    /// stream: the answer comes back through `notes_events`.
    fn start_notes(&mut self, action: NotesAction) {
        let sender = self.notes_sender.clone();
        let Some(backend) = self.notes_backend.clone() else {
            let unavailable = "Notes are not available right now.".to_owned();
            let _ = sender.send(match action {
                NotesAction::Load { seq, .. } => NotesEvent::Loaded {
                    seq,
                    result: Err(unavailable),
                },
                NotesAction::Open { .. } => NotesEvent::File(Err(unavailable)),
            });
            return;
        };
        match action {
            NotesAction::Load {
                query,
                seq,
                debounce,
            } => {
                self.notes_latest.store(seq, Ordering::SeqCst);
                let latest = Arc::clone(&self.notes_latest);
                tokio::spawn(async move {
                    if debounce {
                        sleep(DEBOUNCE).await;
                        if latest.load(Ordering::SeqCst) != seq {
                            return;
                        }
                    }
                    let result: Result<Vec<NoteLine>, String> = if query.chars().count() < 2 {
                        backend.recent().await
                    } else {
                        backend.search(query).await
                    };
                    let _ = sender.send(NotesEvent::Loaded { seq, result });
                });
            }
            NotesAction::Open { name } => {
                tokio::spawn(async move {
                    let _ = sender.send(NotesEvent::File(backend.file(name).await));
                });
            }
        }
    }

    /// Takes an answer of the notes search: a list for the view, or a file to open.
    fn notes_arrived(&mut self, event: NotesEvent, navigator: &impl PaneNavigator) {
        let Some(file) = self.app.notes_event(event) else {
            return;
        };
        if let Err(error) = navigator.open_editor(&file) {
            let detail = sanitize_display(&error.to_string(), ERROR_LIMIT);
            if let Some(view) = self.app.notes.as_mut() {
                view.failed(detail);
            }
        }
    }

    fn open_link(&mut self, link: &str, navigator: &impl PaneNavigator) {
        if let Err(error) = navigator.open_link(link) {
            self.app
                .set_runtime_status(RuntimeStatus::NavigationFailed(sanitize_display(
                    &error.to_string(),
                    ERROR_LIMIT,
                )));
        }
    }

    /// Takes the answer to a `c` or `t` press. Talk now answers with the state from
    /// before the call handled it, so its answer is not shown; the next poll is.
    fn call_result(&mut self, action: CallAction, result: Result<CallView, SourceError>) {
        let result = result.map(|view| {
            if action != CallAction::TalkNow {
                self.app.observe_call(Some(view));
            }
        });
        self.request_result(result, RuntimeStatus::CallFailed);
    }

    fn request_result(
        &mut self,
        result: Result<(), SourceError>,
        failed: fn(String) -> RuntimeStatus,
    ) {
        match result {
            Ok(()) => self.app.clear_runtime_status(),
            Err(error) => self
                .app
                .set_runtime_status(failed(sanitize_display(&error.to_string(), ERROR_LIMIT))),
        }
        if let Some(status) = self
            .navigation_issue
            .clone()
            .filter(|_| self.app.runtime_status().is_none())
        {
            self.app.set_runtime_status(status);
        }
    }

    fn set_navigation_issue(&mut self, status: RuntimeStatus) {
        self.navigation_issue = Some(status.clone());
        self.app.set_runtime_status(status);
    }

    fn begin_stream(&mut self) {
        self.first_snapshot_pending = true;
        if self.app.connection == ConnectionState::Unavailable {
            self.app.begin_connection();
        }
    }

    fn accept_snapshot(&mut self, snapshot: Snapshot, received_at_ms: i64) -> StreamControl {
        if let Some(row) = snapshot.rows.first() {
            AGENTS_ARE_IN_REX.store(
                row.incarnation.pane_id.starts_with(REX_BLOCK_PREFIX),
                Ordering::Release,
            );
        }
        let result = if self.first_snapshot_pending {
            self.app.apply_first_snapshot(snapshot).map(|()| true)
        } else {
            self.app.apply_later_snapshot(snapshot)
        };

        match result {
            Ok(accepted) => {
                if self.first_snapshot_pending {
                    self.first_snapshot_pending = false;
                    self.backoff.reset();
                }
                if accepted {
                    self.app.record_snapshot_received_at(received_at_ms);
                    self.app.clear_runtime_status();
                    if let Some(status) = self.navigation_issue.clone() {
                        self.app.set_runtime_status(status);
                    }
                    if self.app.selected.is_none() {
                        self.app.select_first_row();
                    }
                }
                StreamControl::Continue
            }
            Err(error) => {
                self.protocol_failed(error);
                StreamControl::Reconnect
            }
        }
    }

    fn protocol_failed(&mut self, error: SnapshotError) {
        self.app.mark_disconnected();
        self.app
            .set_runtime_status(RuntimeStatus::SourceError(sanitize_display(
                &error.to_string(),
                ERROR_LIMIT,
            )));
    }

    fn source_failed(&mut self, error: SourceError) {
        self.app.mark_disconnected();
        self.app
            .set_runtime_status(RuntimeStatus::SourceError(sanitize_display(
                &error.to_string(),
                ERROR_LIMIT,
            )));
    }

    fn retry_after(&mut self, detail: &str) -> Duration {
        self.app.mark_disconnected();
        let delay = self.backoff.next();
        self.app.set_runtime_status(RuntimeStatus::Retrying {
            endpoint: self.endpoint_display.clone(),
            detail: sanitize_display(detail, ERROR_LIMIT),
            delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        });
        delay
    }

    fn handle_key(&mut self, key: impl Into<KeyEvent>, navigator: &impl PaneNavigator) -> Control {
        match self.app.handle_key(key) {
            Effect::None => Control::Continue,
            Effect::Retry => Control::Retry,
            Effect::Quit => Control::Quit,
            Effect::SetTtsMuted(muted) => Control::SetTtsMuted(muted),
            Effect::SetMessagingPaused(paused) => Control::SetMessagingPaused(paused),
            Effect::Call(action) => Control::Call(action),
            Effect::Answer(out) => Control::Answer(out),
            Effect::SavePicks { id, version, picks } => Control::SavePicks { id, version, picks },
            Effect::Tick {
                id,
                version,
                step,
                ticked,
            } => Control::Tick {
                id,
                version,
                step,
                ticked,
            },
            Effect::OpenLink(link) => Control::OpenLink(link),
            Effect::Notes(action) => {
                self.start_notes(action);
                Control::Continue
            }
            Effect::Navigate { pane_id } if pane_id.starts_with(REX_BLOCK_PREFIX) => {
                // A Rex block: the agent's session name comes from its row. No tmux client is needed.
                let session = self
                    .app
                    .snapshot
                    .rows
                    .iter()
                    .find(|row| row.incarnation.pane_id == pane_id)
                    .map(|row| row.session_name.clone())
                    .unwrap_or_default();
                match navigator.focus_rex_block(&session, &pane_id) {
                    Ok(()) => self.app.clear_runtime_status(),
                    Err(error) => self.app.set_runtime_status(RuntimeStatus::NavigationFailed(
                        sanitize_display(&error.to_string(), ERROR_LIMIT),
                    )),
                }
                Control::Continue
            }
            Effect::Navigate { pane_id } => {
                let Some(client) = self.client.as_deref() else {
                    self.app
                        .set_runtime_status(RuntimeStatus::NavigationUnavailable);
                    return Control::Continue;
                };
                match navigator.jump_to(client, &pane_id) {
                    Ok(()) => self.app.clear_runtime_status(),
                    Err(error) => self.app.set_runtime_status(RuntimeStatus::NavigationFailed(
                        sanitize_display(&error.to_string(), ERROR_LIMIT),
                    )),
                }
                Control::Continue
            }
        }
    }

    const fn shutdown() -> Control {
        Control::Quit
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::time::Duration;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    use super::{
        Backoff, BoxFuture, ClockPort, Control, FocusProbe, InputMessage, InputPump, OpenOutcome,
        QUESTION_POLL_INTERVAL, RedrawCadence, RuntimeCore, RuntimeInput, ScreenPort, ShutdownPort,
        SourcePort, StreamControl, StreamPort, TerminalInput, call_poll_interval, consume_stream,
        handle_input, merge_exit, open_stream, resolve_client, run_loop, spawn_focus_worker,
        wait_for_retry,
    };
    use crate::api::SourceError;
    use crate::app::{
        AgentIncarnation, AgentRow, AgentState, CallAction, CallState, CallView, ConnectionState,
        FocusReading, MonitorHealth, MonitorHealthState, RuntimeStatus, Snapshot,
    };
    use crate::navigation::{NavigationError, PaneNavigator};
    use crate::notes::NoteLine;
    use crate::questions::{
        AnswerKind, DecisionView, OptionView, Outgoing, PickView, QStop, QuestionView, Queue,
        SendResult,
    };
    use std::collections::VecDeque;

    #[test]
    fn retry_backoff_is_capped_and_resets_only_when_requested() {
        let mut backoff = Backoff::default();
        assert_eq!(
            (0..8).map(|_| backoff.next()).collect::<Vec<_>>(),
            [250, 500, 1_000, 2_000, 4_000, 5_000, 5_000, 5_000].map(Duration::from_millis)
        );
        backoff.reset();
        assert_eq!(backoff.next(), Duration::from_millis(250));
    }

    #[test]
    fn redraw_cadence_is_bounded_without_busy_loop() {
        let start = std::time::Instant::now();
        let mut cadence = RedrawCadence::new(start);

        assert!(!cadence.take_if_due(start + Duration::from_millis(99)));
        assert!(cadence.take_if_due(start + Duration::from_millis(100)));
        assert!(!cadence.take_if_due(start + Duration::from_millis(199)));
        assert!(cadence.take_if_due(start + Duration::from_millis(200)));
    }

    #[tokio::test]
    async fn resize_event_becomes_immediate_redraw_without_mutating_app() {
        let mut pump = InputPump::start_with(SequenceInput {
            events: vec![Event::Resize(160, 40)].into(),
        });

        assert_eq!(pump.recv().await, Some(Ok(RuntimeInput::Redraw)));
    }

    #[tokio::test]
    async fn a_key_reaches_the_app_with_its_modifiers() {
        let control_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let mut pump = InputPump::start_with(SequenceInput {
            events: vec![Event::Key(control_c)].into(),
        });

        let Some(Ok(RuntimeInput::Key(key))) = pump.recv().await else {
            panic!("expected a key");
        };
        assert_eq!(key.code, KeyCode::Char('c'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn first_snapshot_is_authoritative_records_local_time_and_resets_retry() {
        let mut core = RuntimeCore::new(Some("client-7".into()));
        assert_eq!(core.retry_after("unavailable"), Duration::from_millis(250));
        core.begin_stream();

        assert_eq!(
            core.accept_snapshot(snapshot(2, MonitorHealthState::Degraded), 90_000),
            StreamControl::Continue
        );

        assert_eq!(core.app.connection, ConnectionState::Live);
        assert_eq!(core.app.snapshot.through_event_version, 2);
        assert_eq!(core.app.snapshot.rows.len(), 1);
        assert_eq!(core.app.last_snapshot_received_at_ms(), Some(90_000));
        assert_eq!(core.app.degraded_health().count(), 1);
        assert_eq!(core.retry_after("again"), Duration::from_millis(250));
    }

    #[test]
    fn hostile_endpoint_is_sanitized_and_bounded_before_retry_status() {
        let endpoint = format!("http://host/\x1b[31m{}z", "e".repeat(600));
        let mut core = RuntimeCore::new_with_endpoint(Some("client-7".into()), &endpoint);

        core.retry_after("connection refused");

        let Some(RuntimeStatus::Retrying { endpoint, .. }) = core.app.runtime_status() else {
            panic!("retry status expected");
        };
        assert_eq!(endpoint.chars().count(), 512);
        assert!(!endpoint.chars().any(char::is_control));
        assert!(!endpoint.contains("[31m"));
    }

    #[test]
    fn later_duplicate_is_ignored_regression_stales_and_monitor_can_recover() {
        let mut core = RuntimeCore::new(None);
        core.begin_stream();
        core.accept_snapshot(snapshot(5, MonitorHealthState::Degraded), 10_000);
        assert_eq!(
            core.accept_snapshot(snapshot(5, MonitorHealthState::Healthy), 20_000),
            StreamControl::Continue
        );
        assert_eq!(core.app.degraded_health().count(), 1);
        assert_eq!(core.app.last_snapshot_received_at_ms(), Some(10_000));

        assert_eq!(
            core.accept_snapshot(snapshot(6, MonitorHealthState::Healthy), 30_000),
            StreamControl::Continue
        );
        assert_eq!(core.app.degraded_health().count(), 0);
        assert_eq!(core.app.last_snapshot_received_at_ms(), Some(30_000));

        assert_eq!(
            core.accept_snapshot(snapshot(4, MonitorHealthState::Healthy), 40_000),
            StreamControl::Reconnect
        );
        assert_eq!(core.app.connection, ConnectionState::Stale);
        assert_eq!(core.app.snapshot.through_event_version, 6);
        assert!(matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::SourceError(_))
        ));
    }

    #[test]
    fn disconnect_before_and_after_data_selects_unavailable_or_stale() {
        let mut before = RuntimeCore::new(None);
        before.source_failed(SourceError::Stream("closed".into()));
        assert_eq!(before.app.connection, ConnectionState::Unavailable);

        let mut after = RuntimeCore::new(None);
        after.begin_stream();
        after.accept_snapshot(snapshot(1, MonitorHealthState::Healthy), 123);
        after.source_failed(SourceError::Stream("closed".into()));
        assert_eq!(after.app.connection, ConnectionState::Stale);
        assert_eq!(after.app.snapshot.rows.len(), 1);
        assert_eq!(after.app.last_snapshot_received_at_ms(), Some(123));
    }

    #[test]
    fn keys_cover_retry_quit_search_and_navigation_status() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let navigator = FakeNavigator {
            calls: calls.clone(),
            fail: false,
        };
        let mut core = RuntimeCore::new(Some("client-7".into()));
        core.begin_stream();
        core.accept_snapshot(snapshot(1, MonitorHealthState::Healthy), 1);

        assert_eq!(
            core.handle_key(KeyCode::Char('f'), &navigator),
            Control::Continue
        );
        assert_eq!(
            core.handle_key(KeyCode::Char('q'), &navigator),
            Control::Continue
        );
        assert_eq!(core.app.search.query, "q");
        assert_eq!(core.handle_key(KeyCode::Esc, &navigator), Control::Continue);
        assert_eq!(core.app.search.query, "");
        assert!(!core.app.search.editing);
        assert_eq!(
            core.handle_key(KeyCode::Char('f'), &navigator),
            Control::Continue
        );
        assert_eq!(
            core.handle_key(KeyCode::Char('x'), &navigator),
            Control::Continue
        );
        assert_eq!(
            core.handle_key(KeyCode::Enter, &navigator),
            Control::Continue
        );
        assert_eq!(core.handle_key(KeyCode::Esc, &navigator), Control::Continue);
        assert_eq!(core.app.search.query, "");
        assert!(!core.app.search.editing);
        assert_eq!(
            core.handle_key(KeyCode::Char('f'), &navigator),
            Control::Continue
        );
        assert_eq!(core.handle_key(KeyCode::Esc, &navigator), Control::Continue);
        assert_eq!(core.app.search.query, "");
        assert!(!core.app.search.editing);
        assert_eq!(core.handle_key(KeyCode::Esc, &navigator), Control::Continue);
        assert_eq!(
            core.handle_key(
                KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
                &navigator
            ),
            Control::Retry
        );
        assert_eq!(
            core.handle_key(KeyCode::Enter, &navigator),
            Control::Continue
        );
        assert_eq!(
            calls.borrow().as_slice(),
            &[("client-7".into(), "%7".into())]
        );
        assert_eq!(
            core.handle_key(KeyCode::Char('q'), &navigator),
            Control::Quit
        );

        let mut outside = RuntimeCore::new(None);
        outside.begin_stream();
        outside.accept_snapshot(snapshot(1, MonitorHealthState::Healthy), 1);
        assert_eq!(
            outside.handle_key(KeyCode::Enter, &navigator),
            Control::Continue
        );
        assert_eq!(
            outside.app.runtime_status(),
            Some(&RuntimeStatus::NavigationUnavailable)
        );

        let failing = FakeNavigator { calls, fail: true };
        let mut failed = RuntimeCore::new(Some("client-7".into()));
        failed.begin_stream();
        failed.accept_snapshot(snapshot(1, MonitorHealthState::Healthy), 1);
        failed.handle_key(KeyCode::Enter, &failing);
        assert!(matches!(
            failed.app.runtime_status(),
            Some(RuntimeStatus::NavigationFailed(_))
        ));
        assert_eq!(failed.app.snapshot.rows.len(), 1);
    }

    #[test]
    fn focus_is_read_from_the_multiplexer_of_the_agents_ids() {
        let tmux = || Some("%1".to_owned());
        let rex = || Some("block:a".to_owned());

        assert_eq!(
            super::read_focus(true, tmux, rex),
            FocusReading::Active("block:a".into())
        );
        assert_eq!(
            super::read_focus(false, tmux, rex),
            FocusReading::Active("%1".into())
        );
        assert_eq!(
            super::read_focus(true, tmux, || None),
            FocusReading::Unavailable
        );
    }

    struct RexNavigator {
        calls: Rc<RefCell<Vec<(String, String)>>>,
        fail: bool,
    }

    impl PaneNavigator for RexNavigator {
        fn jump_to(&self, _: &str, _: &str) -> Result<(), NavigationError> {
            panic!("a Rex block must not switch a tmux client");
        }

        fn focus_rex_block(&self, session: &str, block: &str) -> Result<(), NavigationError> {
            self.calls.borrow_mut().push((session.into(), block.into()));
            if self.fail {
                return Err(NavigationError::RexNotFound);
            }
            Ok(())
        }
    }

    #[test]
    fn enter_on_a_rex_block_focuses_it_by_session_name_without_a_tmux_client() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let navigator = RexNavigator {
            calls: calls.clone(),
            fail: false,
        };
        let mut rex_snapshot = snapshot(1, MonitorHealthState::Healthy);
        rex_snapshot.rows[0].incarnation.pane_id = "block:abc".into();
        rex_snapshot.rows[0].session_name = "rexsup-dash".into();
        let mut core = RuntimeCore::new(None);
        core.begin_stream();
        core.accept_snapshot(rex_snapshot, 1);

        assert_eq!(
            core.handle_key(KeyCode::Enter, &navigator),
            Control::Continue
        );

        assert_eq!(
            calls.borrow().as_slice(),
            &[("rexsup-dash".into(), "block:abc".into())]
        );
        assert_eq!(core.app.runtime_status(), None);

        let failing = RexNavigator { calls, fail: true };
        core.handle_key(KeyCode::Enter, &failing);
        assert!(matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::NavigationFailed(_))
        ));
    }

    #[test]
    fn shutdown_signals_request_quit() {
        assert_eq!(RuntimeCore::shutdown(), Control::Quit);
    }

    #[test]
    fn tmux_discovery_failure_disables_navigation_without_aborting_startup() {
        let (client, status) = resolve_client(Err(NavigationError::CommandFailed {
            operation: "discover tmux client",
            detail: "hostile\x1b[31m failure".into(),
        }));

        assert_eq!(client, None);
        assert_eq!(
            status,
            Some(RuntimeStatus::NavigationFailed(
                "could not discover tmux client: hostile failure".into()
            ))
        );
    }

    #[test]
    fn cleanup_failure_is_reported_alongside_primary_exit_error() {
        let merged = merge_exit(
            super::AppError::Input("reader failed".into()),
            Err(super::AppError::Terminal(
                crate::terminal::TerminalError::new(
                    "restore terminal modes",
                    io::Error::other("leave failed"),
                ),
            )),
        );

        assert_eq!(
            merged.to_string(),
            "terminal input failed: reader failed; additionally, terminal error: could not restore terminal modes: leave failed"
        );
    }

    #[tokio::test]
    async fn open_failure_schedules_retry_instead_of_exiting() {
        let source = FakeSource::error(SourceError::Transport("refused".into()));
        let (_input_sender, mut input) = InputPump::channel();
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let navigator = FakeNavigator::successful();
        let mut screen = FakeScreen::default();
        let mut core = RuntimeCore::new(Some("client-7".into()));

        let outcome = open_stream(
            &source,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut screen,
            &navigator,
            &FakeClock::default(),
        )
        .await
        .unwrap();

        assert!(matches!(outcome, OpenOutcome::RetryLater));
        assert!(matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::Retrying { delay_ms: 250, .. })
        ));
    }

    #[tokio::test]
    async fn retry_key_cancels_open_delay_and_current_stream_with_awaited_close() {
        let open_dropped = Arc::new(AtomicBool::new(false));
        let source = FakeSource::pending(Arc::clone(&open_dropped));
        let (input_sender, mut input) = InputPump::channel();
        input_sender
            .try_send(InputMessage::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let navigator = FakeNavigator::successful();
        let mut screen = FakeScreen::default();
        let mut core = RuntimeCore::new(Some("client-7".into()));

        assert!(matches!(
            open_stream(
                &source,
                &mut core,
                &mut input,
                &mut shutdown,
                &mut screen,
                &navigator,
                &FakeClock::default(),
            )
            .await
            .unwrap(),
            OpenOutcome::RetryNow
        ));
        assert!(open_dropped.load(Ordering::SeqCst));

        core.retry_after("again");
        let (delay_sender, mut delay_input) = InputPump::channel();
        delay_sender
            .try_send(InputMessage::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        assert_eq!(
            wait_for_retry(
                &mut core,
                &mut delay_input,
                &mut shutdown,
                &mut screen,
                &navigator,
                &FakeClock::default(),
            )
            .await
            .unwrap(),
            Control::Retry
        );

        let closed = Arc::new(AtomicBool::new(false));
        let (stream_sender, mut stream_input) = InputPump::channel();
        stream_sender
            .try_send(InputMessage::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        assert_eq!(
            consume_stream(
                FakeStream::pending(Arc::clone(&closed)),
                &mut core,
                &mut stream_input,
                &mut shutdown,
                &mut screen,
                &navigator,
                &FakeClock::default(),
            )
            .await
            .unwrap(),
            Control::Retry
        );
        assert!(closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn shutdown_cancels_open_and_returns_quit() {
        let open_dropped = Arc::new(AtomicBool::new(false));
        let source = FakeSource::pending(Arc::clone(&open_dropped));
        let (_input_sender, mut input) = InputPump::channel();
        let (shutdown_sender, mut shutdown) = FakeShutdown::channel();
        shutdown_sender.try_send(()).unwrap();
        let mut core = RuntimeCore::new(Some("client-7".into()));

        let outcome = open_stream(
            &source,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();

        assert!(matches!(outcome, OpenOutcome::Quit));
        assert!(open_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn render_failure_exits_stream_only_after_awaited_close() {
        let closed = Arc::new(AtomicBool::new(false));
        let stream = FakeStream::one_snapshot(
            Arc::clone(&closed),
            snapshot(1, MonitorHealthState::Healthy),
        );
        let (_input_sender, mut input) = InputPump::channel();
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let mut core = RuntimeCore::new(Some("client-7".into()));
        core.begin_stream();
        let mut screen = FakeScreen {
            fail: true,
            ..FakeScreen::default()
        };

        let result = consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut screen,
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await;

        assert!(matches!(result, Err(super::AppError::Terminal(_))));
        assert!(closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn quiet_stream_redraw_tick_advances_screen_without_mutating_app() {
        let closed = Arc::new(AtomicBool::new(false));
        let (input_sender, mut input) = InputPump::channel();
        input_sender.try_send(InputMessage::Redraw).unwrap();
        input_sender
            .try_send(InputMessage::Key(KeyEvent::new(
                KeyCode::Char('l'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let mut core = RuntimeCore::new(Some("client-7".into()));
        core.begin_stream();
        core.accept_snapshot(snapshot(1, MonitorHealthState::Healthy), 1);
        let before = core.app.clone();
        let mut screen = FakeScreen::default();
        let clock = FakeClock::starting_at(100);

        let result = consume_stream(
            FakeStream::pending(closed),
            &mut core,
            &mut input,
            &mut shutdown,
            &mut screen,
            &FakeNavigator::successful(),
            &clock,
        )
        .await
        .unwrap();

        assert_eq!(result, Control::Retry);
        assert_eq!(screen.draws, 2);
        assert_eq!(screen.times, vec![100, 200]);
        assert_eq!(core.app, before);
    }

    #[test]
    fn input_worker_never_blocks_when_bounded_channel_is_full() {
        let reads = Arc::new(AtomicUsize::new(0));
        let pump = InputPump::start_with(BurstInput {
            reads: Arc::clone(&reads),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while reads.load(Ordering::Acquire) < 64 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(reads.load(Ordering::Acquire) >= 64);
        drop(pump);
    }

    async fn run_palette_mute(fail: bool) -> (RuntimeCore, Vec<bool>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.mute = Some(MuteProbe {
            calls: Arc::clone(&calls),
            fail,
        });
        let (input_sender, mut input) = InputPump::channel();
        for key in "/voice"
            .chars()
            .map(KeyCode::Char)
            .chain([KeyCode::Enter, KeyCode::Char('q')])
        {
            input_sender
                .try_send(InputMessage::Key(key.into()))
                .unwrap();
        }
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let mut core = live_core(false);
        let outcome = consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Control::Quit);
        let recorded = calls.lock().unwrap().clone();
        (core, recorded)
    }

    async fn run_palette_pause(fail: bool) -> (RuntimeCore, Vec<bool>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.pause = Some(MuteProbe {
            calls: Arc::clone(&calls),
            fail,
        });
        let (input_sender, mut input) = InputPump::channel();
        for key in "/messaging"
            .chars()
            .map(KeyCode::Char)
            .chain([KeyCode::Enter, KeyCode::Char('q')])
        {
            input_sender
                .try_send(InputMessage::Key(key.into()))
                .unwrap();
        }
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let mut core = live_core(false);
        let outcome = consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Control::Quit);
        let recorded = calls.lock().unwrap().clone();
        (core, recorded)
    }

    #[tokio::test]
    async fn consume_stream_sends_palette_pause_request_and_clears_status_on_success() {
        let (core, calls) = run_palette_pause(false).await;
        assert_eq!(calls, vec![true]);
        assert!(!matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::MessagingFailed(_))
        ));
        // Confirmed state is still driven by the snapshot, not the RPC.
        assert!(!core.app.messaging_paused());
    }

    #[tokio::test]
    async fn consume_stream_reports_messaging_failed_when_the_pause_request_fails() {
        let (core, calls) = run_palette_pause(true).await;
        assert_eq!(calls, vec![true]);
        assert!(matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::MessagingFailed(detail)) if detail.contains("pause refused")
        ));
    }

    #[test]
    fn palette_messaging_action_becomes_a_runtime_control() {
        let navigator = FakeNavigator::successful();
        let mut core = live_core(false);
        core.handle_key(KeyCode::Char('/'), &navigator);
        for character in "messaging".chars() {
            core.handle_key(KeyCode::Char(character), &navigator);
        }
        assert_eq!(
            core.handle_key(KeyCode::Enter, &navigator),
            Control::SetMessagingPaused(true)
        );
    }

    #[test]
    fn messaging_result_failure_sets_status_and_leaves_the_confirmed_state() {
        let mut core = live_core(false);
        core.messaging_result(Err(SourceError::Transport("boom".into())));
        assert!(matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::MessagingFailed(detail)) if detail.contains("boom")
        ));
        assert!(!core.app.messaging_paused());

        // `RuntimeCore::new(None)` carries a standing navigation issue, which
        // is restored once the messaging error clears.
        core.messaging_result(Ok(()));
        assert_eq!(
            core.app.runtime_status(),
            Some(&RuntimeStatus::NavigationUnavailable)
        );
    }

    #[tokio::test]
    async fn consume_stream_sends_palette_mute_request_and_clears_status_on_success() {
        let (core, calls) = run_palette_mute(false).await;
        assert_eq!(calls, vec![true]);
        assert!(!matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::VoiceFailed(_))
        ));
        // Confirmed state is still driven by the snapshot, not the RPC.
        assert_eq!(core.app.voice(), crate::app::VoiceState::On);
    }

    #[tokio::test]
    async fn consume_stream_reports_voice_failed_when_the_mute_request_fails() {
        let (core, calls) = run_palette_mute(true).await;
        assert_eq!(calls, vec![true]);
        assert!(matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::VoiceFailed(detail)) if detail.contains("mute refused")
        ));
    }

    fn live_core(muted: bool) -> RuntimeCore {
        let mut core = RuntimeCore::new(None);
        let mut live = snapshot(1, MonitorHealthState::Healthy);
        live.tts_muted = muted;
        core.accept_snapshot(live, 1_000);
        core
    }

    /// The knowledge tool as the notes search sees it; every call is logged.
    #[derive(Default)]
    struct FakeNotes {
        log: std::sync::Mutex<Vec<String>>,
        fail_search: bool,
    }

    impl crate::notes::NotesBackend for FakeNotes {
        fn recent(&self) -> crate::notes::BoxFuture<Result<Vec<NoteLine>, String>> {
            self.log.lock().unwrap().push("recent".into());
            Box::pin(async {
                Ok(vec![NoteLine {
                    name: "recent-note".into(),
                    title: "Recent".into(),
                    kind: "idea".into(),
                }])
            })
        }

        fn search(&self, query: String) -> crate::notes::BoxFuture<Result<Vec<NoteLine>, String>> {
            self.log.lock().unwrap().push(format!("search {query}"));
            let fail = self.fail_search;
            Box::pin(async move {
                if fail {
                    return Err("Search is resting: the model didn't load.".into());
                }
                Ok(vec![NoteLine {
                    name: format!("found-{query}"),
                    title: "Found".into(),
                    kind: "reference".into(),
                }])
            })
        }

        fn file(&self, name: String) -> crate::notes::BoxFuture<Result<String, String>> {
            self.log.lock().unwrap().push(format!("file {name}"));
            Box::pin(async move { Ok(format!("/vault/{name}.md")) })
        }
    }

    struct EditorNavigator {
        opened: Rc<RefCell<Vec<String>>>,
        fail: bool,
    }

    impl PaneNavigator for EditorNavigator {
        fn jump_to(&self, _: &str, _: &str) -> Result<(), NavigationError> {
            Ok(())
        }

        fn open_editor(&self, file: &str) -> Result<(), NavigationError> {
            self.opened.borrow_mut().push(file.into());
            if self.fail {
                return Err(NavigationError::CommandFailed {
                    operation: "open editor",
                    detail: "no tmux".into(),
                });
            }
            Ok(())
        }
    }

    async fn next_notes_event(core: &mut RuntimeCore, navigator: &impl PaneNavigator) {
        let event = tokio::time::timeout(Duration::from_secs(5), core.notes_events.recv())
            .await
            .expect("an answer comes")
            .expect("the channel is open");
        core.notes_arrived(event, navigator);
    }

    #[tokio::test]
    async fn the_notes_search_lists_searches_after_a_pause_and_opens_the_file_in_the_editor() {
        let notes = Arc::new(FakeNotes::default());
        let opened = Rc::new(RefCell::new(Vec::new()));
        let navigator = EditorNavigator {
            opened: Rc::clone(&opened),
            fail: false,
        };
        let mut core = live_core(false);
        core.notes_backend = Some(notes.clone());
        core.app.notes_available = true;

        assert_eq!(
            core.handle_key(KeyCode::Char('s'), &navigator),
            Control::Continue
        );
        next_notes_event(&mut core, &navigator).await;
        let view = core.app.notes.as_ref().unwrap();
        assert_eq!(view.rows[0].name, "recent-note");

        // Three keys typed in a burst: only the last search runs, after the pause.
        core.handle_key(KeyCode::Char('a'), &navigator);
        core.handle_key(KeyCode::Char('b'), &navigator);
        core.handle_key(KeyCode::Char('c'), &navigator);
        next_notes_event(&mut core, &navigator).await; // the one-letter query's recent list
        next_notes_event(&mut core, &navigator).await; // the search
        assert_eq!(
            notes.log.lock().unwrap().clone(),
            vec!["recent", "recent", "search abc"],
            "the typing that was overtaken never searched"
        );
        assert_eq!(core.app.notes.as_ref().unwrap().rows[0].name, "found-abc");

        core.handle_key(KeyCode::Enter, &navigator);
        next_notes_event(&mut core, &navigator).await;
        assert_eq!(*opened.borrow(), vec!["/vault/found-abc.md".to_owned()]);
        assert!(core.app.notes.is_some(), "the dash stays on the list");
    }

    #[tokio::test]
    async fn a_failed_editor_open_and_a_resting_search_are_shown_in_the_list() {
        let notes = Arc::new(FakeNotes {
            fail_search: true,
            ..Default::default()
        });
        let opened = Rc::new(RefCell::new(Vec::new()));
        let navigator = EditorNavigator {
            opened: Rc::clone(&opened),
            fail: true,
        };
        let mut core = live_core(false);
        core.notes_backend = Some(notes);
        core.app.notes_available = true;
        core.handle_key(KeyCode::Char('s'), &navigator);
        next_notes_event(&mut core, &navigator).await;
        core.handle_key(KeyCode::Enter, &navigator);
        next_notes_event(&mut core, &navigator).await;
        let message = core.app.notes.as_ref().unwrap().message.clone().unwrap();
        assert!(message.contains("could not open editor"), "{message}");
        core.handle_key(KeyCode::Char('x'), &navigator);
        core.handle_key(KeyCode::Char('y'), &navigator);
        next_notes_event(&mut core, &navigator).await;
        next_notes_event(&mut core, &navigator).await;
        let message = core.app.notes.as_ref().unwrap().message.clone().unwrap();
        assert!(message.contains("Search is resting"), "{message}");
    }

    #[tokio::test]
    async fn without_a_knowledge_tool_the_list_says_notes_are_not_available() {
        let navigator = FakeNavigator::successful();
        let mut core = live_core(false);
        core.app.notes_available = true;
        core.handle_key(KeyCode::Char('s'), &navigator);
        next_notes_event(&mut core, &navigator).await;
        let message = core.app.notes.as_ref().unwrap().message.clone().unwrap();
        assert_eq!(message, "Notes are not available right now.");
    }

    #[test]
    fn palette_voice_action_becomes_a_runtime_control() {
        let navigator = FakeNavigator::successful();
        let mut core = live_core(false);
        core.handle_key(KeyCode::Char('/'), &navigator);
        for character in "voice".chars() {
            core.handle_key(KeyCode::Char(character), &navigator);
        }
        assert_eq!(
            core.handle_key(KeyCode::Enter, &navigator),
            Control::SetTtsMuted(true)
        );
    }

    #[test]
    fn voice_result_failure_sets_status_and_leaves_the_confirmed_state() {
        let mut core = live_core(false);
        core.voice_result(Err(SourceError::Transport("boom".into())));
        assert!(matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::VoiceFailed(detail)) if detail.contains("boom")
        ));
        assert_eq!(core.app.voice(), crate::app::VoiceState::On);

        // `RuntimeCore::new(None)` carries a standing navigation issue, which
        // is restored once the voice error clears.
        core.voice_result(Ok(()));
        assert_eq!(
            core.app.runtime_status(),
            Some(&RuntimeStatus::NavigationUnavailable)
        );
    }

    fn chief_view(state: CallState) -> CallView {
        CallView {
            state,
            chief_pane_id: "%7".into(),
            last_transcript: String::new(),
            last_result: None,
        }
    }

    /// Runs `consume_stream` over `keys` then `q`, with a stream whose `call`
    /// answers the status with `view` and fails every press when `fail_presses`.
    async fn run_call_stream(
        core: &mut RuntimeCore,
        view: Option<CallView>,
        fail_presses: bool,
        keys: &str,
    ) -> Vec<CallAction> {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.call = Some(CallProbe {
            calls: Arc::clone(&calls),
            view,
            fail_presses,
        });
        let (input_sender, mut input) = InputPump::channel();
        for key in keys.chars().chain(['q']).map(KeyCode::Char) {
            input_sender
                .try_send(InputMessage::Key(key.into()))
                .unwrap();
        }
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let outcome = consume_stream(
            stream,
            core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Control::Quit);
        calls.lock().unwrap().clone()
    }

    fn connected_core() -> RuntimeCore {
        let mut core = RuntimeCore::new(Some("client-7".into()));
        core.begin_stream();
        core.accept_snapshot(snapshot(1, MonitorHealthState::Healthy), 1_000);
        core
    }

    #[test]
    fn call_poll_interval_is_half_a_second_during_a_call() {
        assert_eq!(call_poll_interval(None), Duration::from_secs(5));
        for (state, interval) in [
            (CallState::NotSetUp, Duration::from_secs(5)),
            (CallState::Off, Duration::from_secs(5)),
            (CallState::Listening, Duration::from_millis(500)),
            (CallState::Thinking, Duration::from_millis(500)),
            (CallState::Speaking, Duration::from_millis(500)),
        ] {
            assert_eq!(
                call_poll_interval(Some(&chief_view(state))),
                interval,
                "{state:?}"
            );
        }
    }

    #[tokio::test]
    async fn consume_stream_asks_for_the_call_state_when_the_stream_opens() {
        let mut core = connected_core();
        let calls = run_call_stream(&mut core, Some(chief_view(CallState::Off)), false, "").await;
        assert_eq!(calls.first(), Some(&CallAction::Status));
        assert_eq!(core.app.call, Some(chief_view(CallState::Off)));
        assert_eq!(core.app.chief_row().unwrap().incarnation.pane_id, "%7");
    }

    #[tokio::test]
    async fn a_failed_poll_hides_the_button_without_a_status() {
        let mut core = connected_core();
        core.app
            .observe_call(Some(chief_view(CallState::Listening)));
        let calls = run_call_stream(&mut core, None, false, "").await;
        assert_eq!(calls.first(), Some(&CallAction::Status));
        assert_eq!(core.app.call, None);
        assert_eq!(core.app.runtime_status(), None);
    }

    #[tokio::test]
    async fn a_failed_call_press_shows_call_failed() {
        let mut core = connected_core();
        let calls = run_call_stream(&mut core, Some(chief_view(CallState::Off)), true, "c").await;
        assert_eq!(calls, [CallAction::Status, CallAction::Start]);
        assert!(matches!(
            core.app.runtime_status(),
            Some(RuntimeStatus::CallFailed(detail)) if detail.contains("call refused")
        ));
        // The last known state stays until the next poll.
        assert_eq!(core.app.call, Some(chief_view(CallState::Off)));
    }

    #[tokio::test]
    async fn a_call_press_shows_harolds_answer_except_for_talk_now() {
        let mut core = connected_core();
        let calls = run_call_stream(&mut core, Some(chief_view(CallState::Off)), false, "c").await;
        assert_eq!(calls, [CallAction::Status, CallAction::Start]);
        assert_eq!(core.app.call, Some(chief_view(CallState::Listening)));
        assert_eq!(core.app.runtime_status(), None);

        // Talk now answers with the state from before the call handled it.
        let mut core = connected_core();
        let calls =
            run_call_stream(&mut core, Some(chief_view(CallState::Speaking)), false, "t").await;
        assert_eq!(calls, [CallAction::Status, CallAction::TalkNow]);
        assert_eq!(core.app.call, Some(chief_view(CallState::Speaking)));
    }

    fn question(id: &str, version: u32, decisions: usize) -> Queue {
        Queue {
            questions: vec![question_view(id, version, decisions)],
            waiting: 1,
            dismissed: Vec::new(),
        }
    }

    fn question_view(id: &str, version: u32, decisions: usize) -> QuestionView {
        let option = |key: &str, suggested: bool| OptionView {
            key: key.into(),
            label: format!("label {key}"),
            impact: format!("impact {key}"),
            suggested,
            free: false,
        };
        QuestionView {
            id: id.into(),
            product: "harold".into(),
            spec: "spec".into(),
            sha: "abc".into(),
            link: "https://example.com/spec".into(),
            intro: "intro".into(),
            version,
            title: String::new(),
            steps: vec![],
            ticked: vec![],
            picks: vec![],
            decisions: (1..=decisions)
                .map(|n| DecisionView {
                    ask: format!("Ask {n}?"),
                    options: vec![option("a", false), option("b", true)],
                })
                .collect(),
        }
    }

    fn question_probe(queue: Option<Queue>) -> QuestionProbe {
        Arc::new(std::sync::Mutex::new(QuestionState {
            queue,
            ..QuestionState::default()
        }))
    }

    /// Runs `consume_stream` over `keys` then `q`, with `probe` as the question tool,
    /// and returns the tool's call log.
    async fn run_question_stream(
        core: &mut RuntimeCore,
        probe: &QuestionProbe,
        keys: &[KeyCode],
    ) -> Vec<String> {
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(probe));
        let (input_sender, mut input) = InputPump::channel();
        for key in keys
            .iter()
            .map(|code| KeyEvent::from(*code))
            .chain([ctrl_q()])
        {
            input_sender.try_send(InputMessage::Key(key)).unwrap();
        }
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let outcome = consume_stream(
            stream,
            core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Control::Quit);
        probe.lock().unwrap().log.clone()
    }

    /// Ends a run of keys: Ctrl+Q quits wherever the cursor is, and is never read for.
    fn ctrl_q() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
    }

    /// `k` from the agent row to the question's header (the question's rows are at the
    /// top of the list here, as no chief is named), `l` to open it, then `j` three times
    /// to `send`, for a question with one decision of two options.
    const TO_SEND: [KeyCode; 5] = [
        KeyCode::Char('k'),
        KeyCode::Char('l'),
        KeyCode::Char('j'),
        KeyCode::Char('j'),
        KeyCode::Char('j'),
    ];

    /// The picked option key of each decision of the question in view.
    fn picked(core: &RuntimeCore) -> Vec<Option<String>> {
        core.app
            .questions
            .view()
            .unwrap()
            .decisions
            .iter()
            .map(|d| d.options.iter().find(|o| o.picked).map(|o| o.key.clone()))
            .collect()
    }

    fn lists(n: usize) -> Vec<String> {
        vec!["list".to_string(); n]
    }

    #[tokio::test]
    async fn the_queue_is_polled_every_two_seconds() {
        assert_eq!(QUESTION_POLL_INTERVAL, Duration::from_secs(2));
        let mut core = connected_core();
        assert_eq!(core.question_poll_interval, QUESTION_POLL_INTERVAL);
        core.question_poll_interval = Duration::from_millis(20);
        let probe = question_probe(Some(question("Q1", 1, 1)));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(&probe));
        let (_input_sender, mut input) = InputPump::channel();
        let (shutdown_sender, mut shutdown) = FakeShutdown::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = shutdown_sender.send(()).await;
        });
        let outcome = consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Control::Quit);
        let listed = probe.lock().unwrap().log.len();
        assert!(listed >= 4, "listed {listed} times");
        assert!(core.app.questions.is_visible());
    }

    /// Runs `consume_stream` for `run_for` with the question poll every 20 ms, the
    /// question tool as `questions` and the backlog tool
    /// answering `answers` in order, and returns how often the backlog tool was read.
    async fn run_now_stream(
        core: &mut RuntimeCore,
        questions: QuestionProbe,
        answers: Vec<Result<Option<String>, SourceError>>,
        run_for: Duration,
    ) -> usize {
        core.question_poll_interval = Duration::from_millis(20);
        let probe = Arc::new(std::sync::Mutex::new(NowState {
            answers: answers.into(),
            reads: 0,
        }));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(questions);
        stream.now = Some(Arc::clone(&probe));
        let (_input_sender, mut input) = InputPump::channel();
        let (shutdown_sender, mut shutdown) = FakeShutdown::channel();
        tokio::spawn(async move {
            tokio::time::sleep(run_for).await;
            let _ = shutdown_sender.send(()).await;
        });
        let outcome = consume_stream(
            stream,
            core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Control::Quit);
        probe.lock().unwrap().reads
    }

    #[tokio::test]
    async fn the_now_line_follows_the_question_poll() {
        let mut core = connected_core();
        let reads = run_now_stream(
            &mut core,
            question_probe(Some(Queue::default())),
            vec![Ok(Some("a".into())), Ok(Some("b".into()))],
            Duration::from_millis(100),
        )
        .await;
        assert!(reads >= 2, "read {reads} times");
        assert_eq!(core.app.now_line.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn an_unimplemented_backlog_hides_the_now_line() {
        let mut core = connected_core();
        let reads = run_now_stream(
            &mut core,
            question_probe(Some(Queue::default())),
            vec![
                Ok(Some("a".into())),
                Err(SourceError::Transport("status: Unimplemented".into())),
            ],
            Duration::from_millis(100),
        )
        .await;
        assert!(reads >= 2, "read {reads} times");
        assert_eq!(core.app.now_line, None);

        // The read as the stream opens shows the line, and a failed read hides it.
        let mut core = connected_core();
        assert!(core.now_listed(Ok(Some("a".into()))));
        assert_eq!(core.app.now_line.as_deref(), Some("a"));
        assert!(core.now_listed(Err(SourceError::Transport("status: Unimplemented".into()))));
        assert_eq!(core.app.now_line, None);
        assert!(!core.now_listed(Ok(None)), "no Now item keeps it hidden");
    }

    #[tokio::test]
    async fn a_timed_out_question_read_skips_the_now_read_and_hides_the_line() {
        let mut core = connected_core();
        core.app.observe_now(Some("a".into()));
        let questions = question_probe(None);
        questions.lock().unwrap().times_out = true;
        let reads = run_now_stream(
            &mut core,
            questions,
            vec![Ok(Some("a".into()))],
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(reads, 0, "no Now read follows a timed-out question read");
        assert_eq!(core.app.now_line, None);
    }

    #[tokio::test]
    async fn an_unimplemented_question_tool_still_shows_the_now_line() {
        let mut core = connected_core();
        // `None` answers every list with `status: Unimplemented`, at once.
        let reads = run_now_stream(
            &mut core,
            question_probe(None),
            vec![Ok(Some("a".into()))],
            Duration::from_millis(100),
        )
        .await;
        assert!(reads >= 2, "read {reads} times");
        assert_eq!(core.app.now_line.as_deref(), Some("a"));
        assert!(!core.app.questions.is_visible());
    }

    #[tokio::test]
    async fn a_successful_question_read_is_followed_by_the_now_read() {
        let mut core = connected_core();
        let reads = run_now_stream(
            &mut core,
            question_probe(Some(Queue::default())),
            vec![Ok(Some("a".into()))],
            Duration::from_millis(100),
        )
        .await;
        assert!(reads >= 2, "read {reads} times");
        assert_eq!(core.app.now_line.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn a_stream_without_the_backlog_tool_shows_no_now_line() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 1)));
        run_question_stream(&mut core, &probe, &[]).await;
        assert_eq!(core.app.now_line, None);
    }

    #[tokio::test]
    async fn a_key_on_a_question_row_lists_first() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 2)));
        let log = run_question_stream(
            &mut core,
            &probe,
            &[
                KeyCode::Char('j'),
                KeyCode::Char('k'),
                KeyCode::Char('l'),
                KeyCode::Char('j'),
            ],
        )
        .await;
        // Once as the stream opens, then before `l` and the second `j`; not before the
        // first `j` or the `k` that brings the cursor onto the question, nor Ctrl+Q.
        assert_eq!(log, lists(3));
        assert_eq!(
            core.app.question_stop,
            Some(QStop::Option {
                decision: 0,
                option: 0
            })
        );
    }

    #[tokio::test]
    async fn a_key_that_meets_a_changed_question_is_dropped() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 2)));
        probe.lock().unwrap().on_list = VecDeque::from([
            Some(question("Q1", 1, 2)),
            Some(question("Q1", 1, 2)),
            Some(question("Q1", 1, 2)),
            Some(question("Q1", 2, 2)),
        ]);
        // Space on the first option would pick `a`; its read finds version 2.
        run_question_stream(
            &mut core,
            &probe,
            &[
                KeyCode::Char('k'),
                KeyCode::Char('l'),
                KeyCode::Char('j'),
                KeyCode::Char(' '),
            ],
        )
        .await;
        let view = core.app.questions.view().unwrap();
        assert_eq!(
            picked(&core),
            [Some("b".to_string()), Some("b".to_string())],
            "Space was not applied"
        );
        assert_eq!(
            view.notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
    }

    #[tokio::test]
    async fn enter_on_send_is_dropped_when_the_read_finds_a_higher_version() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 2, 1)));
        {
            let mut state = probe.lock().unwrap();
            // The stream's open and the four keys before Enter that read, `l` and three
            // `j`, find version 2 as listed;
            // the read before Enter finds version 3.
            state.on_list = VecDeque::from(vec![Some(question("Q1", 2, 1)); 5]);
            state.on_list.push_back(Some(question("Q1", 3, 1)));
        }
        let mut keys = TO_SEND.to_vec();
        keys.push(KeyCode::Enter);
        let log = run_question_stream(&mut core, &probe, &keys).await;
        assert_eq!(log, lists(6));
        let state = probe.lock().unwrap();
        assert!(state.sent.is_empty(), "nothing was answered");
        assert!(!state.log.iter().any(|call| call.starts_with("answer")));
        drop(state);
        assert_eq!(
            core.app.questions.view().unwrap().notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
        assert_eq!(core.app.question_stop, Some(QStop::Action(0)));
    }

    #[tokio::test]
    async fn enter_on_send_calls_answer_question() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 3, 1)));
        probe
            .lock()
            .unwrap()
            .answers
            .push_back((SendResult::Recorded, Some(Queue::default())));
        let mut keys = TO_SEND.to_vec();
        keys.push(KeyCode::Enter);
        let log = run_question_stream(&mut core, &probe, &keys).await;
        let mut expected = lists(6);
        expected.extend(["answer Q1 v3".to_string(), "list".to_string()]);
        assert_eq!(log, expected);
        let sent = probe.lock().unwrap().sent.clone();
        assert_eq!(
            sent,
            vec![Outgoing {
                id: "Q1".into(),
                version: 3,
                kind: AnswerKind::Answer,
                picks: vec![PickView {
                    decision: 0,
                    key: "b".into(),
                    text: String::new(),
                }],
                note: String::new(),
                send_now: true,
            }]
        );
        assert!(!core.app.questions.is_visible());
        assert!(
            !core.app.cursor_on_question(),
            "the cursor is on a row of the list"
        );
    }

    #[tokio::test]
    async fn a_tick_calls_the_source_and_relists() {
        let mut core = connected_core();
        let mut view = question_view("I1", 2, 0);
        view.title = "Do these".into();
        view.steps = vec![
            crate::questions::StepView {
                text: "one".into(),
                detail: String::new(),
            },
            crate::questions::StepView {
                text: "two".into(),
                detail: String::new(),
            },
        ];
        let queue = Queue {
            questions: vec![view],
            waiting: 1,
            dismissed: Vec::new(),
        };
        let probe = question_probe(Some(queue));
        probe.lock().unwrap().ticks.push_back(SendResult::Recorded);
        // k to the header, l opens it, j to the first step, space ticks it.
        let keys = [
            KeyCode::Char('k'),
            KeyCode::Char('l'),
            KeyCode::Char('j'),
            KeyCode::Char(' '),
        ];
        let log = run_question_stream(&mut core, &probe, &keys).await;
        let mut expected = lists(4);
        expected.extend(["tick I1 v2 s1 true".to_string(), "list".to_string()]);
        assert_eq!(log, expected);
    }

    #[tokio::test]
    async fn a_pick_saves_without_relisting_and_a_refusal_relists() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 1)));
        probe.lock().unwrap().saves.push_back(SendResult::Recorded);
        probe
            .lock()
            .unwrap()
            .saves
            .push_back(SendResult::VersionChanged);
        // k to the header, l opens it, j to the first option, space picks it; again.
        let keys = [
            KeyCode::Char('k'),
            KeyCode::Char('l'),
            KeyCode::Char('j'),
            KeyCode::Char(' '),
            KeyCode::Char(' '),
        ];
        let log = run_question_stream(&mut core, &probe, &keys).await;
        let mut expected = lists(4);
        // Every key that reaches the question reads the list first; a recorded save
        // adds none after it, the refused one does.
        expected.extend([
            "save Q1 v1 n1".to_string(),
            "list".to_string(),
            "save Q1 v1 n1".to_string(),
            "list".to_string(),
        ]);
        assert_eq!(log, expected);
        let saved = probe.lock().unwrap().saved.clone();
        assert_eq!(saved[0][0].decision, 0);
    }

    #[tokio::test]
    async fn the_relegate_row_opens_a_note_and_enter_on_it_answers_with_relegate() {
        let mut core = connected_core();
        let mut queue = question("Q1", 1, 1);
        queue.questions.push(question_view("Q2", 1, 1));
        queue.waiting = 2;
        let probe = question_probe(Some(queue));
        let mut after = question("Q2", 1, 1);
        after.waiting = 1;
        probe
            .lock()
            .unwrap()
            .answers
            .push_back((SendResult::Recorded, Some(after)));
        let log = run_question_stream(
            &mut core,
            &probe,
            &[
                KeyCode::Char('k'),
                KeyCode::Char('l'),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                // The row opens the note, and Enter on the empty note sends.
                KeyCode::Enter,
                KeyCode::Enter,
            ],
        )
        .await;
        assert!(log.contains(&"answer Q1 v1".to_string()), "{log:?}");
        let sent = probe.lock().unwrap().sent.clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            (sent[0].kind, sent[0].note.as_str(), sent[0].send_now),
            (crate::questions::AnswerKind::Relegate, "", true)
        );
        assert_eq!(sent[0].picks.len(), 1, "the picks so far go with it");
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.id, "Q2", "Q1 left the todo rows");
    }

    #[tokio::test]
    async fn a_version_changed_answer_shows_the_new_version() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 2)));
        let mut changed = question("Q1", 2, 2);
        changed.questions[0].decisions[0].ask = "Changed?".into();
        probe
            .lock()
            .unwrap()
            .answers
            .push_back((SendResult::VersionChanged, Some(changed)));
        let mut keys = TO_SEND.to_vec();
        // Two decisions: two more options before `send`.
        keys.extend([KeyCode::Char('j'), KeyCode::Char('j'), KeyCode::Enter]);
        let log = run_question_stream(&mut core, &probe, &keys).await;
        assert!(log.contains(&"answer Q1 v1".to_string()));
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.decisions[0].ask, "Changed?");
        assert_eq!(
            picked(&core),
            [Some("b".to_string()), Some("b".to_string())]
        );
        assert_eq!(view.mode, crate::questions::Mode::Choosing);
        assert_eq!(
            view.notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );

        // The next answer carries the new version.
        probe
            .lock()
            .unwrap()
            .answers
            .push_back((SendResult::Recorded, Some(Queue::default())));
        let log = run_question_stream(&mut core, &probe, &[KeyCode::Enter]).await;
        assert!(log.contains(&"answer Q1 v2".to_string()), "{log:?}");
    }

    #[tokio::test]
    async fn an_unavailable_answer_keeps_the_picks() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 1)));
        probe
            .lock()
            .unwrap()
            .answers
            .push_back((SendResult::Unavailable, None));
        let mut keys = TO_SEND.to_vec();
        keys.push(KeyCode::Enter);
        let log = run_question_stream(&mut core, &probe, &keys).await;
        assert!(log.contains(&"answer Q1 v1".to_string()));
        let view = core.app.questions.view().unwrap();
        assert_eq!(picked(&core), [Some("b".to_string())]);
        assert_eq!(
            view.notice.as_deref(),
            Some("Harold can't save answers right now")
        );
        assert_eq!(core.app.question_stop, Some(QStop::Action(0)));
    }

    #[tokio::test]
    async fn a_poll_found_change_drops_the_next_question_key() {
        let mut core = connected_core();
        core.question_poll_interval = Duration::from_millis(10);
        let probe = question_probe(Some(question("Q1", 1, 2)));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(&probe));
        let (input_sender, mut input) = InputPump::channel();
        let key = |code: KeyCode| InputMessage::Key(code.into());
        // The cursor goes to the first option of the open question.
        for code in [KeyCode::Char('k'), KeyCode::Char('l'), KeyCode::Char('j')] {
            input_sender.try_send(key(code)).unwrap();
        }
        let tool = Arc::clone(&probe);
        tokio::spawn(async move {
            // The question changes while no note is open, so no keys are held. Well
            // after a poll has found version 2, the user presses Space: only the flag
            // keeps it from picking the first option.
            tokio::time::sleep(Duration::from_millis(50)).await;
            tool.lock().unwrap().queue = Some(question("Q1", 2, 2));
            tokio::time::sleep(Duration::from_millis(150)).await;
            for message in [key(KeyCode::Char(' ')), InputMessage::Key(ctrl_q())] {
                input_sender.send(message).await.unwrap();
            }
        });
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let outcome = consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Control::Quit);
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.mode, crate::questions::Mode::Choosing);
        assert_eq!(
            picked(&core),
            [Some("b".to_string()), Some("b".to_string())],
            "Space picked nothing"
        );
        assert_eq!(
            view.notice.as_deref(),
            Some("Q1 changed, your picks were cleared"),
            "the dropped key did not clear the notice"
        );
    }

    #[tokio::test]
    async fn a_note_knocked_out_by_a_poll_swallows_until_esc() {
        let mut core = connected_core();
        core.question_poll_interval = Duration::from_millis(10);
        let probe = question_probe(Some(question("Q1", 1, 2)));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(&probe));
        let (input_sender, mut input) = InputPump::channel();
        let key = |code: KeyCode| InputMessage::Key(code.into());
        // Open the question and the revise note: two above the agent row after them.
        for code in [
            KeyCode::Char('k'),
            KeyCode::Char('l'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Enter,
        ] {
            input_sender.try_send(key(code)).unwrap();
        }
        let tool = Arc::clone(&probe);
        tokio::spawn(async move {
            // A poll finds version 2 while the user is typing the note "later".
            tokio::time::sleep(Duration::from_millis(50)).await;
            tool.lock().unwrap().queue = Some(question("Q1", 2, 2));
            tokio::time::sleep(Duration::from_millis(150)).await;
            let typed = "later".chars().map(KeyCode::Char).chain([KeyCode::Enter]);
            for code in typed.chain([KeyCode::Tab, KeyCode::Esc]) {
                input_sender.send(key(code)).await.unwrap();
            }
            input_sender
                .send(InputMessage::Key(ctrl_q()))
                .await
                .unwrap();
        });
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let outcome = consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Control::Quit);
        let log = probe.lock().unwrap().log.clone();
        assert!(
            !log.iter().any(|call| call.starts_with("answer")),
            "{log:?}"
        );
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.mode, crate::questions::Mode::Choosing);
        assert_eq!(
            picked(&core),
            [Some("b".to_string()), Some("b".to_string())],
            "the typed letters and Enter changed nothing"
        );
        assert!(!core.app.holding_block_keys(), "Esc ended the hold");
        assert_eq!(view.notice, None, "Esc read the change");
        assert!(core.app.cursor_on_question());
    }

    #[test]
    fn a_second_version_bump_is_flagged() {
        let mut core = connected_core();
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 2));
        core.handle_key(KeyCode::Char('k'), &navigator);
        assert!(core.app.cursor_on_question());
        assert!(core.questions_refreshed(Ok(question("Q1", 2, 2))));
        assert!(core.drop_next_block_key);
        core.drop_next_block_key = false;
        // The notice reads the same after a second bump, but the version differs.
        assert!(core.questions_refreshed(Ok(question("Q1", 3, 2))));
        assert!(core.drop_next_block_key, "the second bump is flagged too");
        core.drop_next_block_key = false;
        assert!(!core.questions_refreshed(Ok(question("Q1", 3, 2))));
        assert!(!core.drop_next_block_key, "an unchanged read flags nothing");
        // With the cursor on the list there is nothing to drop.
        core.handle_key(KeyCode::Char('j'), &navigator);
        assert!(!core.app.cursor_on_question());
        assert!(core.questions_refreshed(Ok(question("Q1", 4, 2))));
        assert!(!core.drop_next_block_key);
    }

    #[test]
    fn a_failed_list_keeps_the_picks_for_the_next_list() {
        let mut core = connected_core();
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 2));
        // Open the question and pick `a` on its first decision.
        for code in [
            KeyCode::Char('k'),
            KeyCode::Char('l'),
            KeyCode::Char('j'),
            KeyCode::Char(' '),
        ] {
            core.handle_key(code, &navigator);
        }
        assert_eq!(
            picked(&core),
            [Some("a".to_string()), Some("b".to_string())]
        );
        core.questions_listed(Err(SourceError::Transport("unavailable".into())));
        assert!(!core.app.questions.is_visible());
        assert_eq!(core.app.runtime_status(), None);
        core.questions_listed(Ok(question("Q1", 1, 2)));
        let view = core.app.questions.view().unwrap();
        assert_eq!(
            picked(&core),
            [Some("a".to_string()), Some("b".to_string())]
        );
        assert!(view.open);
        assert_eq!(view.notice, None, "a failed list leaves no notice");
    }

    #[test]
    fn a_failed_list_then_a_new_version_clears_the_picks() {
        let mut core = connected_core();
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 2));
        for code in [
            KeyCode::Char('k'),
            KeyCode::Char('l'),
            KeyCode::Char('j'),
            KeyCode::Char(' '),
        ] {
            core.handle_key(code, &navigator);
        }
        core.questions_listed(Err(SourceError::Transport("unavailable".into())));
        core.questions_listed(Ok(question("Q1", 2, 2)));
        let view = core.app.questions.view().unwrap();
        assert_eq!(
            picked(&core),
            [Some("b".to_string()), Some("b".to_string())]
        );
        assert_eq!(
            view.notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );

        // A question gone by then shows its dismissal.
        core.questions_listed(Err(SourceError::Transport("unavailable".into())));
        let gone = Queue {
            dismissed: vec![("Q1".into(), "no longer needed".into())],
            ..Queue::default()
        };
        core.questions_listed(Ok(gone));
        assert_eq!(
            core.app.questions.view().unwrap().notice.as_deref(),
            Some("Q1 dismissed: no longer needed")
        );
    }

    #[tokio::test]
    async fn a_half_typed_note_survives_a_reconnect() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 2)));
        let keys = [
            KeyCode::Char('k'),
            KeyCode::Char('l'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Enter,
            KeyCode::Char('a'),
            KeyCode::Char('b'),
            KeyCode::Char('c'),
        ];
        run_question_stream(&mut core, &probe, &keys).await;
        // The stream ends: run_async hides the questions.
        core.app.hide_questions();
        assert!(!core.app.questions.is_visible());
        let log = run_question_stream(&mut core, &probe, &[KeyCode::Char('d')]).await;
        // Read as each stream opens, and before each key on the question rows.
        assert_eq!(log, lists(15));
        assert_eq!(
            core.app.questions.view().unwrap().mode,
            crate::questions::Mode::Note {
                kind: crate::questions::NoteKind::Revise,
                text: "abcd".into(),
            }
        );
    }

    #[tokio::test]
    async fn a_question_key_whose_read_withdraws_the_question_is_dropped() {
        let mut core = connected_core();
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 1));
        core.handle_key(KeyCode::Char('k'), &navigator);
        assert!(core.app.cursor_on_question());
        // The read as the stream opens still lists Q1. The read before `f` finds it gone:
        // `f` was meant for the question row, so it is dropped. On the list it would
        // start a search and take the closing Ctrl+Q's place.
        let probe = question_probe(Some(Queue::default()));
        probe.lock().unwrap().on_list =
            VecDeque::from([Some(question("Q1", 1, 1)), Some(Queue::default())]);
        let log = tokio::time::timeout(
            Duration::from_secs(5),
            run_question_stream(&mut core, &probe, &[KeyCode::Char('f')]),
        )
        .await
        .expect("Ctrl+Q quit");
        assert_eq!(log, lists(2));
        assert!(!core.app.search.editing, "f was dropped");
        assert!(
            !core.app.cursor_on_question(),
            "the cursor went to the list"
        );
    }

    #[tokio::test]
    async fn a_poll_that_withdraws_the_question_under_the_cursor_keeps_the_next_key() {
        let mut core = connected_core();
        core.accept_snapshot(two_rows(2), 2_000);
        core.question_poll_interval = Duration::from_millis(10);
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 1));
        core.handle_key(KeyCode::Char('k'), &navigator);
        assert!(core.app.cursor_on_question());
        core.app.question_sent("Q1", SendResult::NotWaiting);
        assert!(core.app.questions.shows_notice_only());
        assert!(
            !core.app.cursor_on_question(),
            "the cursor is on the list now"
        );
        let first = core.app.selected.clone();
        let probe = question_probe(Some(Queue::default()));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(&probe));
        let (input_sender, mut input) = InputPump::channel();
        // Kept open, so the input does not close before `q` quits.
        let _input_open = input_sender.clone();
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        tokio::spawn(async move {
            // The read as the stream opens keeps the notice; the next poll, 10 ms on,
            // clears it. The cursor is on the list, so `j` moves the selection and `q`
            // quits: neither is dropped.
            tokio::time::sleep(Duration::from_millis(150)).await;
            for code in [KeyCode::Char('j'), KeyCode::Char('q')] {
                input_sender
                    .send(InputMessage::Key(code.into()))
                    .await
                    .unwrap();
            }
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            consume_stream(
                stream,
                &mut core,
                &mut input,
                &mut shutdown,
                &mut FakeScreen::default(),
                &FakeNavigator::successful(),
                &FakeClock::default(),
            ),
        )
        .await
        .expect("q quit")
        .unwrap();
        assert_eq!(outcome, Control::Quit);
        assert!(!core.app.questions.is_visible());
        assert!(!core.app.cursor_on_question());
        assert_ne!(core.app.selected, first, "j moved the selection");
        assert!(!core.drop_next_block_key);
    }

    /// Runs `consume_stream` over `keys` alone, with `drop_next_block_key` set and the
    /// cursor on the question's header, or on the list, and returns how it ended.
    async fn run_flagged(core: &mut RuntimeCore, keys: &[KeyEvent], on_question: bool) -> Control {
        let probe = question_probe(Some(question("Q1", 1, 1)));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(&probe));
        core.app.observe_questions(question("Q1", 1, 1));
        if on_question {
            core.handle_key(KeyCode::Char('k'), &FakeNavigator::successful());
            assert!(core.app.cursor_on_question());
        }
        core.drop_next_block_key = true;
        let (input_sender, mut input) = InputPump::channel();
        let _input_open = input_sender.clone();
        for key in keys {
            input_sender.try_send(InputMessage::Key(*key)).unwrap();
        }
        let (shutdown_sender, mut shutdown) = FakeShutdown::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = shutdown_sender.send(()).await;
        });
        consume_stream(
            stream,
            core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn the_guard_never_drops_quit_keys_or_a_key_on_the_list() {
        let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        for chord in ['q', 'c'] {
            let mut core = connected_core();
            let started = std::time::Instant::now();
            assert_eq!(
                run_flagged(&mut core, &[ctrl(chord)], true).await,
                Control::Quit
            );
            assert!(
                started.elapsed() < Duration::from_millis(250),
                "Ctrl+{chord} quit before the shutdown"
            );
        }

        let mut core = connected_core();
        assert_eq!(
            run_flagged(&mut core, &[ctrl('l')], true).await,
            Control::Retry
        );

        // A key for the question row is dropped.
        let mut core = connected_core();
        run_flagged(&mut core, &[KeyCode::Char('l').into()], true).await;
        assert!(!core.app.questions.is_open(), "l was dropped");
        assert!(!core.drop_next_block_key);

        // On the list, a key acts whatever the flag says: `gg` goes to the question.
        let mut core = connected_core();
        run_flagged(
            &mut core,
            &[KeyCode::Char('g').into(), KeyCode::Char('g').into()],
            false,
        )
        .await;
        assert_eq!(
            core.app.question_stop,
            Some(QStop::Header),
            "g was not dropped"
        );
    }

    #[tokio::test]
    async fn the_flow_answers_q25_then_revises_q26() {
        let mut core = connected_core();
        let options = |suggested: &str| {
            ["a", "b", "c"]
                .into_iter()
                .map(|key| OptionView {
                    key: key.into(),
                    label: format!("label {key}"),
                    impact: format!("impact {key}"),
                    suggested: key == suggested,
                    free: false,
                })
                .collect::<Vec<_>>()
        };
        let mut q25 = question_view("Q25", 1, 2);
        q25.decisions[0].options = options("a");
        q25.decisions[1].options = options("b");
        let q26 = question_view("Q26", 1, 1);
        let queue = Queue {
            questions: vec![q25, q26.clone()],
            waiting: 2,
            dismissed: Vec::new(),
        };
        let probe = question_probe(Some(queue));
        let after_q25 = Queue {
            questions: vec![q26],
            waiting: 1,
            dismissed: Vec::new(),
        };
        probe.lock().unwrap().answers.extend([
            (SendResult::Recorded, Some(after_q25)),
            (SendResult::Recorded, Some(Queue::default())),
        ]);
        // k puts the cursor on Q25's header and l opens it. Both decisions start on their
        // suggestions, a and b. j, j, j go to the option c of decision 1, and Space picks
        // it. j goes to decision 2's first option, and Space picks a there. j, j, j go to
        // send; Enter sends.
        //
        // After Q25 is recorded, the probe's queue drops it. The cursor goes to Q26's
        // header, the question open, and `n` with one question listed does nothing. j five times
        // go to Q26's revise row; Enter opens the note, the note is typed, and Enter
        // sends it.
        let mut keys = vec![
            KeyCode::Char('k'),
            KeyCode::Char('l'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char(' '),
            KeyCode::Char('j'),
            KeyCode::Char(' '),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Enter,
            KeyCode::Char('n'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Char('j'),
            KeyCode::Enter,
        ];
        keys.extend("redo it".chars().map(KeyCode::Char));
        keys.push(KeyCode::Enter);
        let log = run_question_stream(&mut core, &probe, &keys).await;
        let answers: Vec<&String> = log.iter().filter(|c| c.starts_with("answer")).collect();
        assert_eq!(answers, ["answer Q25 v1", "answer Q26 v1"]);
        let sent = probe.lock().unwrap().sent.clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].id, "Q25");
        assert_eq!(sent[0].kind, AnswerKind::Answer);
        let picks: Vec<(u32, &str)> = sent[0]
            .picks
            .iter()
            .map(|pick| (pick.decision, pick.key.as_str()))
            .collect();
        assert_eq!(picks, [(0, "c"), (1, "a")]);
        assert!(sent[0].send_now);
        assert_eq!(sent[1].id, "Q26");
        assert_eq!(sent[1].kind, AnswerKind::Revise);
        assert_eq!(sent[1].note, "redo it");
        assert!(sent[1].send_now);
        assert!(!core.app.questions.is_visible());
    }

    #[tokio::test]
    async fn ctrl_l_retries_the_stream() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 2)));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(&probe));
        let reopened = Arc::new(AtomicBool::new(false));
        let source = FakeSource::sequence([
            FakeOpen::Stream(Box::new(stream)),
            FakeOpen::Pending(Arc::clone(&reopened)),
        ]);
        let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let (input_sender, mut input) = InputPump::channel();
        let opened = Arc::clone(&probe);
        tokio::spawn(async move {
            // The keys go once the stream has opened and read the queue, so none of
            // them meets the open. The revise row is two above the last row; the note
            // holds `abc` when Ctrl+L retries. Ctrl+Q then quits while the
            // stream reopens.
            while opened.lock().unwrap().log.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let keys = [
                KeyCode::Char('k'),
                KeyCode::Char('l'),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                KeyCode::Enter,
                KeyCode::Char('a'),
                KeyCode::Char('b'),
                KeyCode::Char('c'),
            ]
            .map(KeyEvent::from);
            for key in keys.into_iter().chain([ctrl('l'), ctrl('q')]) {
                input_sender.send(InputMessage::Key(key)).await.unwrap();
            }
        });
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let mut screen = FakeScreen::default();
        tokio::time::timeout(
            Duration::from_secs(5),
            run_loop(
                &source,
                &mut core,
                &mut input,
                &mut shutdown,
                &mut screen,
                &FakeNavigator::successful(),
                &FakeClock::default(),
            ),
        )
        .await
        .expect("Ctrl+Q quit")
        .unwrap();
        assert!(
            reopened.load(Ordering::SeqCst),
            "Ctrl+L reopened the stream"
        );
        assert_eq!(screen.clears, 1);
        assert!(screen.drawn_since_clear, "a draw followed the clear");

        // The stream reopens, as in a_half_typed_note_survives_a_reconnect.
        run_question_stream(&mut core, &probe, &[]).await;
        assert_eq!(
            core.app.questions.view().unwrap().mode,
            crate::questions::Mode::Note {
                kind: crate::questions::NoteKind::Revise,
                text: "abc".into(),
            }
        );
    }

    #[test]
    fn a_first_snapshot_with_search_typing_leaves_the_query() {
        let mut core = RuntimeCore::new(Some("client-7".into()));
        core.begin_stream();
        let navigator = FakeNavigator::successful();
        for code in ['f', 'a', 'b'] {
            core.handle_key(KeyCode::Char(code), &navigator);
        }
        assert!(core.app.search.editing);
        assert_eq!(core.app.search.query, "ab");
        core.accept_snapshot(two_rows(1), 1_000);
        assert_eq!(core.app.search.query, "ab");
        assert!(core.app.search.editing);
        let second = two_rows(1).rows[1].incarnation.clone();
        assert_eq!(core.app.selected, Some(second));
    }

    #[tokio::test]
    async fn an_answer_puts_off_the_poll_so_a_dismissal_notice_stays() {
        let mut core = connected_core();
        core.question_poll_interval = Duration::from_millis(1_000);
        let probe = question_probe(Some(question("Q1", 1, 1)));
        probe
            .lock()
            .unwrap()
            .answers
            .push_back((SendResult::NotWaiting, Some(Queue::default())));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(&probe));
        let (input_sender, mut input) = InputPump::channel();
        // Kept open, so the input does not close before the shutdown.
        let _input_open = input_sender.clone();
        let (shutdown_sender, mut shutdown) = FakeShutdown::channel();
        tokio::spawn(async move {
            // The answer goes just before the first poll is due, at 1 s. Its read keeps
            // the notice; a poll right after it would clear it at once.
            tokio::time::sleep(Duration::from_millis(900)).await;
            for code in TO_SEND.into_iter().chain([KeyCode::Enter]) {
                input_sender
                    .send(InputMessage::Key(code.into()))
                    .await
                    .unwrap();
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            let _ = shutdown_sender.send(()).await;
        });
        consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &FakeNavigator::successful(),
            &FakeClock::default(),
        )
        .await
        .unwrap();
        let log = probe.lock().unwrap().log.clone();
        let answered = log
            .iter()
            .position(|call| call == "answer Q1 v1")
            .unwrap_or_else(|| panic!("{log:?}"));
        assert_eq!(
            log[answered + 1..],
            lists(1),
            "only the read after the answer, no poll: {log:?}"
        );
        assert!(core.app.questions.shows_notice_only(), "the notice stays");
        assert!(core.app.questions.view().unwrap().notice.is_some());
    }

    #[tokio::test]
    async fn unimplemented_hides_the_block() {
        let mut core = connected_core();
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 1));
        core.handle_key(KeyCode::Char('k'), &navigator);
        assert!(core.app.cursor_on_question());
        // An older Harold answers Unimplemented; Harold down fails the same way.
        let probe = question_probe(None);
        // The keys are swallowed until Esc, so `j` does nothing.
        let log = run_question_stream(&mut core, &probe, &[KeyCode::Char('j'), KeyCode::Esc]).await;
        assert!(!core.app.swallowing_keys(), "Esc ended it");
        assert_eq!(log, lists(1));
        assert!(
            !core.app.questions.is_visible(),
            "no withdrawn notice either"
        );
        assert!(!core.app.cursor_on_question());
        assert_eq!(core.app.runtime_status(), None);
    }

    #[tokio::test]
    async fn o_opens_the_link_through_the_navigator() {
        struct LinkNavigator(RefCell<Vec<String>>);
        impl PaneNavigator for LinkNavigator {
            fn jump_to(&self, _client: &str, _pane_id: &str) -> Result<(), NavigationError> {
                Ok(())
            }
            fn open_link(&self, link: &str) -> Result<(), NavigationError> {
                self.0.borrow_mut().push(link.into());
                Ok(())
            }
        }
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 1)));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(&probe));
        let (input_sender, mut input) = InputPump::channel();
        for key in [
            KeyEvent::from(KeyCode::Char('k')),
            KeyEvent::from(KeyCode::Char('o')),
            ctrl_q(),
        ] {
            input_sender.try_send(InputMessage::Key(key)).unwrap();
        }
        let (_shutdown_sender, mut shutdown) = FakeShutdown::channel();
        let navigator = LinkNavigator(RefCell::new(Vec::new()));
        consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            &mut FakeScreen::default(),
            &navigator,
            &FakeClock::default(),
        )
        .await
        .unwrap();
        assert_eq!(*navigator.0.borrow(), ["https://example.com/spec"]);
    }

    #[test]
    fn no_mouse_capture_is_enabled() {
        for (name, source) in [
            ("terminal.rs", include_str!("terminal.rs")),
            (
                "terminal/fault_harness.rs",
                include_str!("terminal/fault_harness.rs"),
            ),
        ] {
            assert!(!source.contains("EnableMouseCapture"), "{name}");
        }
    }

    struct FakeNavigator {
        calls: Rc<RefCell<Vec<(String, String)>>>,
        fail: bool,
    }

    impl FakeNavigator {
        fn successful() -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                fail: false,
            }
        }
    }

    impl PaneNavigator for FakeNavigator {
        fn jump_to(&self, client: &str, pane_id: &str) -> Result<(), NavigationError> {
            self.calls
                .borrow_mut()
                .push((client.into(), pane_id.into()));
            if self.fail {
                return Err(NavigationError::CommandFailed {
                    operation: "switch tmux client",
                    detail: "pane disappeared".into(),
                });
            }
            Ok(())
        }
    }

    fn snapshot(revision: u64, health_state: MonitorHealthState) -> Snapshot {
        Snapshot {
            harold_version: None,
            tts_muted: false,
            messaging_paused: false,
            through_event_version: revision,
            server_time_ms: 999_999_999,
            monitor_health: vec![MonitorHealth {
                component: "inventory".into(),
                state: health_state,
                reason_code: "test".into(),
                observed_at_ms: 1,
            }],
            rows: vec![AgentRow {
                incarnation: AgentIncarnation {
                    pane_id: "%7".into(),
                    pane_pid: 70,
                    agent_pid: 71,
                    agent_started_at_ms: 72,
                    provider_id: "codex".into(),
                },
                provider_display_name: "Codex".into(),
                tmux_target: "agents:1.7".into(),
                session_name: "agents".into(),
                window_index: 1,
                pane_index: 7,
                working_directory: "/work".into(),
                work_summary: Some("runtime work".into()),
                state: AgentState::Busy,
                last_transition_at_ms: 73,
            }],
        }
    }

    /// The usual snapshot with a second row, whose work summary holds `ab`.
    fn two_rows(revision: u64) -> Snapshot {
        let mut snapshot = snapshot(revision, MonitorHealthState::Healthy);
        let mut row = snapshot.rows[0].clone();
        row.incarnation = AgentIncarnation {
            pane_id: "%8".into(),
            pane_pid: 80,
            agent_pid: 81,
            agent_started_at_ms: 82,
            provider_id: "codex".into(),
        };
        row.tmux_target = "agents:1.8".into();
        row.pane_index = 8;
        row.work_summary = Some("tab work".into());
        snapshot.rows.push(row);
        snapshot
    }

    #[derive(Default)]
    struct FakeScreen {
        draws: usize,
        fail: bool,
        times: Vec<i64>,
        clears: usize,
        /// Whether a draw came after the last clear.
        drawn_since_clear: bool,
    }

    impl ScreenPort for FakeScreen {
        fn draw_screen(
            &mut self,
            _app: &crate::app::App,
            now_ms: i64,
        ) -> Result<(), crate::terminal::TerminalError> {
            self.draws += 1;
            self.drawn_since_clear = true;
            self.times.push(now_ms);
            if self.fail {
                return Err(crate::terminal::TerminalError::new(
                    "render dashboard",
                    io::Error::other("injected render failure"),
                ));
            }
            Ok(())
        }

        fn clear_screen(&mut self) -> Result<(), crate::terminal::TerminalError> {
            self.clears += 1;
            self.drawn_since_clear = false;
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeClock {
        next: AtomicI64,
    }

    impl FakeClock {
        fn starting_at(now_ms: i64) -> Self {
            Self {
                next: AtomicI64::new(now_ms),
            }
        }
    }

    impl ClockPort for FakeClock {
        fn now_ms(&self) -> i64 {
            self.next.fetch_add(100, Ordering::SeqCst)
        }
    }

    enum FakeOpen {
        Error(SourceError),
        Pending(Arc<AtomicBool>),
        Stream(Box<FakeStream>),
    }

    /// Answers each open with the next of `opens`, in order.
    struct FakeSource {
        opens: std::sync::Mutex<VecDeque<FakeOpen>>,
    }

    impl FakeSource {
        fn error(error: SourceError) -> Self {
            Self::sequence([FakeOpen::Error(error)])
        }

        fn pending(dropped: Arc<AtomicBool>) -> Self {
            Self::sequence([FakeOpen::Pending(dropped)])
        }

        fn sequence(opens: impl IntoIterator<Item = FakeOpen>) -> Self {
            Self {
                opens: std::sync::Mutex::new(opens.into_iter().collect()),
            }
        }
    }

    impl SourcePort for FakeSource {
        type Stream = FakeStream;

        fn open_stream(&self) -> BoxFuture<'_, Result<Self::Stream, SourceError>> {
            let behavior = self.opens.lock().unwrap().pop_front().unwrap();
            match behavior {
                FakeOpen::Error(error) => Box::pin(async move { Err(error) }),
                FakeOpen::Stream(stream) => Box::pin(async move { Ok(*stream) }),
                FakeOpen::Pending(dropped) => Box::pin(PendingOpen {
                    _guard: DropFlag(dropped),
                }),
            }
        }
    }

    struct FakeStream {
        closed: Arc<AtomicBool>,
        receive: FakeReceive,
        mute: Option<MuteProbe>,
        pause: Option<MuteProbe>,
        call: Option<CallProbe>,
        questions: Option<QuestionProbe>,
        now: Option<NowProbe>,
    }

    /// The backlog tool's Now reads, answered in order; the last answer repeats.
    struct NowState {
        answers: VecDeque<Result<Option<String>, SourceError>>,
        reads: usize,
    }

    type NowProbe = Arc<std::sync::Mutex<NowState>>;

    /// The question tool as a test sees it; every call is logged.
    #[derive(Default)]
    struct QuestionState {
        log: Vec<String>,
        /// What a list answers; `None` fails it, as an older Harold without the tool does.
        queue: Option<Queue>,
        /// Every list times out, as a hung Harold's does.
        times_out: bool,
        /// Answers for the next lists, in order; each one also becomes `queue`.
        on_list: VecDeque<Option<Queue>>,
        /// Each answer takes the next result, and the queue lists show after it.
        answers: VecDeque<(SendResult, Option<Queue>)>,
        sent: Vec<Outgoing>,
        /// Each tick takes the next result.
        ticks: VecDeque<SendResult>,
        /// Each save takes the next result; with none left it is recorded.
        saves: VecDeque<SendResult>,
        /// The picks of every save.
        saved: Vec<Vec<PickView>>,
    }

    type QuestionProbe = Arc<std::sync::Mutex<QuestionState>>;

    struct CallProbe {
        calls: Arc<std::sync::Mutex<Vec<CallAction>>>,
        /// The status answer; `None` fails every poll.
        view: Option<CallView>,
        fail_presses: bool,
    }

    struct MuteProbe {
        calls: Arc<std::sync::Mutex<Vec<bool>>>,
        fail: bool,
    }

    enum FakeReceive {
        Pending,
        Item(Option<Result<Snapshot, SourceError>>),
    }

    impl FakeStream {
        fn pending(closed: Arc<AtomicBool>) -> Self {
            Self {
                closed,
                receive: FakeReceive::Pending,
                mute: None,
                pause: None,
                call: None,
                questions: None,
                now: None,
            }
        }

        fn one_snapshot(closed: Arc<AtomicBool>, snapshot: Snapshot) -> Self {
            Self {
                closed,
                receive: FakeReceive::Item(Some(Ok(snapshot))),
                mute: None,
                pause: None,
                call: None,
                questions: None,
                now: None,
            }
        }
    }

    impl StreamPort for FakeStream {
        fn receive(&mut self) -> BoxFuture<'_, Option<Result<Snapshot, SourceError>>> {
            match &mut self.receive {
                FakeReceive::Pending => Box::pin(std::future::pending()),
                FakeReceive::Item(item) => Box::pin(async move { item.take() }),
            }
        }

        fn set_tts_muted(&mut self, muted: bool) -> BoxFuture<'_, Result<(), SourceError>> {
            let Some(probe) = &self.mute else {
                return Box::pin(async { Ok(()) });
            };
            probe.calls.lock().unwrap().push(muted);
            let fail = probe.fail;
            Box::pin(async move {
                if fail {
                    Err(SourceError::Transport("mute refused".into()))
                } else {
                    Ok(())
                }
            })
        }

        fn set_messaging_paused(&mut self, paused: bool) -> BoxFuture<'_, Result<(), SourceError>> {
            let Some(probe) = &self.pause else {
                return Box::pin(async { Ok(()) });
            };
            probe.calls.lock().unwrap().push(paused);
            let fail = probe.fail;
            Box::pin(async move {
                if fail {
                    Err(SourceError::Transport("pause refused".into()))
                } else {
                    Ok(())
                }
            })
        }

        fn call(&mut self, action: CallAction) -> BoxFuture<'_, Result<CallView, SourceError>> {
            let Some(probe) = &self.call else {
                return Box::pin(async { Err(SourceError::Transport("no call RPC".into())) });
            };
            probe.calls.lock().unwrap().push(action);
            let answer = match (action, &probe.view) {
                (_, None) => Err(SourceError::Transport("no call RPC".into())),
                (CallAction::Status, Some(view)) => Ok(view.clone()),
                _ if probe.fail_presses => Err(SourceError::Transport("call refused".into())),
                (CallAction::Stop, Some(view)) => Ok(CallView {
                    state: CallState::Off,
                    ..view.clone()
                }),
                (_, Some(view)) => Ok(CallView {
                    state: CallState::Listening,
                    ..view.clone()
                }),
            };
            Box::pin(async move { answer })
        }

        fn list_questions(&mut self) -> BoxFuture<'_, Result<Queue, SourceError>> {
            let Some(probe) = &self.questions else {
                return Box::pin(async { Err(SourceError::Transport("no question tool".into())) });
            };
            let mut state = probe.lock().unwrap();
            state.log.push("list".into());
            if let Some(next) = state.on_list.pop_front() {
                state.queue = next;
            }
            if state.times_out {
                return Box::pin(async {
                    Err(SourceError::TimedOut("question request timed out".into()))
                });
            }
            let answer = state
                .queue
                .clone()
                .ok_or_else(|| SourceError::Transport("status: Unimplemented".into()));
            Box::pin(async move { answer })
        }

        fn list_queue_now(&mut self) -> BoxFuture<'_, Result<Option<String>, SourceError>> {
            let Some(probe) = &self.now else {
                return Box::pin(async { Err(SourceError::Transport("no backlog tool".into())) });
            };
            let mut state = probe.lock().unwrap();
            state.reads += 1;
            let answer = if state.answers.len() > 1 {
                state.answers.pop_front().unwrap()
            } else {
                state.answers.front().cloned().unwrap()
            };
            Box::pin(async move { answer })
        }

        fn answer_question<'a>(&'a mut self, out: &'a Outgoing) -> BoxFuture<'a, SendResult> {
            let Some(probe) = &self.questions else {
                return Box::pin(async { SendResult::Unavailable });
            };
            let mut state = probe.lock().unwrap();
            state
                .log
                .push(format!("answer {} v{}", out.id, out.version));
            state.sent.push(out.clone());
            let (result, next) = state
                .answers
                .pop_front()
                .unwrap_or((SendResult::Unavailable, None));
            if let Some(next) = next {
                state.queue = Some(next);
            }
            Box::pin(async move { result })
        }

        fn tick_step<'a>(
            &'a mut self,
            id: &'a str,
            version: u32,
            step: u32,
            ticked: bool,
        ) -> BoxFuture<'a, SendResult> {
            let Some(probe) = &self.questions else {
                return Box::pin(async { SendResult::Unavailable });
            };
            let mut state = probe.lock().unwrap();
            state
                .log
                .push(format!("tick {id} v{version} s{step} {ticked}"));
            let result = state.ticks.pop_front().unwrap_or(SendResult::Unavailable);
            Box::pin(async move { result })
        }

        fn save_picks<'a>(
            &'a mut self,
            id: &'a str,
            version: u32,
            picks: &'a [PickView],
        ) -> BoxFuture<'a, SendResult> {
            let Some(probe) = &self.questions else {
                return Box::pin(async { SendResult::Unavailable });
            };
            let mut state = probe.lock().unwrap();
            state
                .log
                .push(format!("save {id} v{version} n{}", picks.len()));
            state.saved.push(picks.to_vec());
            let result = state.saves.pop_front().unwrap_or(SendResult::Recorded);
            Box::pin(async move { result })
        }

        fn close_stream(self) -> BoxFuture<'static, ()> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                self.closed.store(true, Ordering::SeqCst);
            })
        }
    }

    struct DropFlag(Arc<AtomicBool>);

    struct PendingOpen {
        _guard: DropFlag,
    }

    impl Future for PendingOpen {
        type Output = Result<FakeStream, SourceError>;

        fn poll(
            self: Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            std::task::Poll::Pending
        }
    }

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct FakeShutdown {
        receiver: mpsc::Receiver<()>,
    }

    impl FakeShutdown {
        fn channel() -> (mpsc::Sender<()>, Self) {
            let (sender, receiver) = mpsc::channel(1);
            (sender, Self { receiver })
        }
    }

    impl ShutdownPort for FakeShutdown {
        fn recv_shutdown(&mut self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                let _ = self.receiver.recv().await;
            })
        }
    }

    struct BurstInput {
        reads: Arc<AtomicUsize>,
    }

    struct SequenceInput {
        events: std::collections::VecDeque<Event>,
    }

    impl TerminalInput for SequenceInput {
        fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
            Ok(!self.events.is_empty())
        }

        fn read(&mut self) -> io::Result<Event> {
            self.events
                .pop_front()
                .ok_or_else(|| io::Error::other("no scripted event"))
        }
    }

    impl TerminalInput for BurstInput {
        fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
            Ok(true)
        }

        fn read(&mut self) -> io::Result<Event> {
            self.reads.fetch_add(1, Ordering::Release);
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('j'),
                KeyModifiers::NONE,
            )))
        }
    }

    struct ScriptedProbe(std::collections::VecDeque<FocusReading>);

    impl FocusProbe for ScriptedProbe {
        fn read(&mut self) -> FocusReading {
            if self.0.len() > 1 {
                self.0.pop_front().unwrap()
            } else {
                self.0.front().cloned().unwrap_or(FocusReading::Unavailable)
            }
        }
    }

    fn pump_with_focus(
        probe: impl FocusProbe,
        interval: Duration,
    ) -> (
        mpsc::Sender<InputMessage>,
        InputPump,
        Arc<AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        let (input, mut pump) = InputPump::channel();
        let (sender, receiver) = tokio::sync::watch::channel(None);
        pump.focus = Some(receiver);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker = spawn_focus_worker(probe, sender, Arc::clone(&cancelled), interval);
        (input, pump, cancelled, worker)
    }

    #[tokio::test]
    async fn focus_worker_reports_changes_only_and_stops_on_cancel() {
        let (_input, mut pump, cancelled, worker) = pump_with_focus(
            ScriptedProbe(
                [
                    FocusReading::Active("%1".into()),
                    FocusReading::Active("%1".into()),
                    FocusReading::Active("%2".into()),
                ]
                .into(),
            ),
            Duration::from_millis(5),
        );
        // A watch keeps only the latest value, so %1 may be skipped, but no reading is
        // repeated and the final state is always delivered.
        let mut seen = Vec::new();
        while seen.last() != Some(&FocusReading::Active("%2".into())) {
            let Some(Ok(RuntimeInput::Focus(reading))) = pump.recv().await else {
                panic!("expected a focus reading");
            };
            seen.push(reading);
        }
        assert!(seen.windows(2).all(|pair| pair[0] != pair[1]), "{seen:?}");
        // Unchanged readings publish nothing further.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), pump.recv())
                .await
                .is_err()
        );
        cancelled.store(true, Ordering::Release);
        worker.join().unwrap();
    }

    #[test]
    fn focus_input_updates_the_app_without_changing_control_flow() {
        let navigator = FakeNavigator::successful();
        let mut core = RuntimeCore::new(None);
        let control = handle_input(
            Some(Ok(RuntimeInput::Focus(FocusReading::Active("%7".into())))),
            &mut core,
            &navigator,
        )
        .unwrap();
        assert!(matches!(control, Control::Continue));
        assert!(core.app.focus_tracking());

        handle_input(
            Some(Ok(RuntimeInput::Focus(FocusReading::Unavailable))),
            &mut core,
            &navigator,
        )
        .unwrap();
        assert!(!core.app.focus_tracking());
    }

    #[tokio::test]
    async fn pending_input_is_delivered_first_and_the_latest_focus_is_never_lost() {
        let (input, mut pump, cancelled, worker) = pump_with_focus(
            ScriptedProbe([FocusReading::Active("%1".into())].into()),
            Duration::from_millis(5),
        );
        for _ in 0..super::INPUT_CHANNEL_CAPACITY {
            input.try_send(InputMessage::Redraw).unwrap();
        }
        std::thread::sleep(Duration::from_millis(40));
        for _ in 0..super::INPUT_CHANNEL_CAPACITY {
            assert_eq!(pump.recv().await, Some(Ok(RuntimeInput::Redraw)));
        }
        assert_eq!(
            pump.recv().await,
            Some(Ok(RuntimeInput::Focus(FocusReading::Active("%1".into()))))
        );
        cancelled.store(true, Ordering::Release);
        worker.join().unwrap();
    }

    struct PanickingInput;

    impl TerminalInput for PanickingInput {
        fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
            panic!("input worker dies");
        }

        fn read(&mut self) -> io::Result<Event> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn a_dead_input_worker_ends_the_stream_even_with_a_live_focus_worker() {
        let mut pump = InputPump::start_with_focus(
            PanickingInput,
            ScriptedProbe([FocusReading::Active("%1".into())].into()),
        );
        let ended = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if pump.recv().await.is_none() {
                    return;
                }
            }
        })
        .await;
        assert!(ended.is_ok(), "recv never reported the dead input worker");
    }

    struct BlockingProbe(std::sync::mpsc::Receiver<()>);

    impl FocusProbe for BlockingProbe {
        fn read(&mut self) -> FocusReading {
            let _ = self.0.recv();
            FocusReading::Unavailable
        }
    }

    #[test]
    fn dropping_the_pump_does_not_wait_for_a_hung_focus_probe() {
        let (release, blocked) = std::sync::mpsc::channel();
        let pump = InputPump::start_with_focus(
            SequenceInput {
                events: Vec::new().into(),
            },
            BlockingProbe(blocked),
        );
        std::thread::sleep(Duration::from_millis(50));
        let started = std::time::Instant::now();
        drop(pump);
        assert!(started.elapsed() < Duration::from_millis(500));
        release.send(()).unwrap();
    }
}
