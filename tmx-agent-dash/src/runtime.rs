use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use tokio::runtime::Builder;
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;

use crate::api::{AgentStateSource, SourceError, SourceStream};
use crate::app::{
    App, CallAction, CallView, ConnectionState, Effect, Focus, FocusReading, RuntimeStatus,
    SearchState, Snapshot, SnapshotError,
};
use crate::cli::Options;
use crate::navigation::{NavigationError, PaneNavigator, TmuxNavigator};
use crate::questions::{Outgoing, Queue, SendResult, UNAVAILABLE_NOTICE};
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
    let clock = SystemClock;
    draw(terminal, &core.app, &clock)?;
    let mut retry_immediately = true;

    loop {
        if !retry_immediately {
            match wait_for_retry(
                &mut core,
                &mut input,
                &mut shutdown,
                terminal,
                navigator,
                &clock,
            )
            .await?
            {
                Control::Quit => return Ok(()),
                Control::Retry
                | Control::Continue
                | Control::SetTtsMuted(_)
                | Control::SetMessagingPaused(_)
                | Control::Call(_)
                | Control::Answer(_)
                | Control::Defer(_)
                | Control::OpenLink(_) => {}
            }
        }
        retry_immediately = false;

        let stream = match open_stream(
            source,
            &mut core,
            &mut input,
            &mut shutdown,
            terminal,
            navigator,
            &clock,
        )
        .await?
        {
            OpenOutcome::Stream(stream) => stream,
            OpenOutcome::RetryNow => {
                retry_immediately = true;
                continue;
            }
            OpenOutcome::RetryLater => continue,
            OpenOutcome::Quit => return Ok(()),
        };
        core.begin_stream();

        let outcome = consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            terminal,
            navigator,
            &clock,
        )
        .await;
        // The block lives only while a stream does: without Harold nothing can be
        // answered, so it hides until a stream opens and the queue is read again.
        core.app.hide_questions();
        match outcome {
            Ok(Control::Quit) => return Ok(()),
            Ok(Control::Retry) => {
                retry_immediately = true;
            }
            Ok(
                Control::Continue
                | Control::SetTtsMuted(_)
                | Control::SetMessagingPaused(_)
                | Control::Call(_)
                | Control::Answer(_)
                | Control::Defer(_)
                | Control::OpenLink(_),
            ) => {
                let detail = core
                    .app
                    .runtime_status()
                    .map(runtime_status_detail)
                    .unwrap_or_else(|| "Harold stream closed".to_owned());
                core.retry_after(&detail);
                draw(terminal, &core.app, &clock)?;
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
    fn answer_question<'a>(&'a mut self, _out: &'a Outgoing) -> BoxFuture<'a, SendResult> {
        Box::pin(async { SendResult::Unavailable })
    }
    fn defer_question<'a>(&'a mut self, _id: &'a str) -> BoxFuture<'a, Result<(), SourceError>> {
        Box::pin(async { Err(SourceError::Transport("question tool unavailable".into())) })
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

    fn answer_question<'a>(&'a mut self, out: &'a Outgoing) -> BoxFuture<'a, SendResult> {
        Box::pin(SourceStream::answer_question(self, out))
    }

    fn defer_question<'a>(&'a mut self, id: &'a str) -> BoxFuture<'a, Result<(), SourceError>> {
        Box::pin(SourceStream::defer_question(self, id))
    }
}

trait ScreenPort {
    fn draw_screen(&mut self, app: &App, now_ms: i64) -> Result<(), TerminalError>;
}

impl ScreenPort for TerminalGuard {
    fn draw_screen(&mut self, app: &App, now_ms: i64) -> Result<(), TerminalError> {
        self.draw(app, now_ms)
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
                    | Control::Defer(_)
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
        // QUESTION_POLL_INTERVAL, before each key that goes to the block (and before
        // Tab), and after each answer or deferral. Each read is awaited inline and
        // bounded by VOICE_REQUEST_TIMEOUT.
        let polled = stream.call(CallAction::Status).await;
        if core.call_polled(polled) {
            draw(terminal, &core.app, clock)?;
        }
        let listed = stream.list_questions().await;
        if core.questions_listed(listed).changed {
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
                if core.questions_refreshed(listed) {
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
            item = input.recv() => {
                if let Some(Ok(RuntimeInput::Key(key))) = &item {
                    // A key meant for a question that just changed, went or hid is
                    // dropped: the user has not seen what it would act on. The change
                    // is found by this key's own read, or was flagged by an earlier one.
                    let flagged = std::mem::take(&mut core.drop_next_block_key);
                    if core.app.wants_fresh_questions(key) {
                        let reaches_block = core.app.key_reaches_block(key);
                        let listed = stream.list_questions().await;
                        let changed = core.questions_listed(listed).question_changed;
                        if reaches_block && (flagged || changed) {
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
                    draw(terminal, &core.app, clock)?;
                    continue;
                }
                if let Control::Defer(id) = &control {
                    if stream.defer_question(id).await.is_err() {
                        core.app.questions.set_notice(UNAVAILABLE_NOTICE.into());
                    }
                    let listed = stream.list_questions().await;
                    core.questions_refreshed(listed);
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
                        | Control::Defer(_)
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

struct TmuxFocusProbe;

impl FocusProbe for TmuxFocusProbe {
    fn read(&mut self) -> FocusReading {
        // Built per read: TmuxNavigator's ports are not Send, and construction is free.
        match TmuxNavigator::new().focused_pane() {
            Ok(Some(pane_id)) => FocusReading::Active(pane_id),
            Ok(None) | Err(_) => FocusReading::Unavailable,
        }
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
    Defer(String),
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
    /// A read no key asked for changed the question under the focus: the next key, if
    /// it goes to the block, is dropped. Any key clears it.
    drop_next_block_key: bool,
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
        Self {
            app: App::new(
                ConnectionState::Connecting,
                Snapshot {
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

    /// Takes a read no key asked for: the poll, or the read after an answer or a
    /// deferral. If it changes the question in view while the block has the focus, the
    /// next block key is dropped, as it was typed for what was there before. Reports
    /// whether the block changed at all.
    fn questions_refreshed(&mut self, result: Result<Queue, SourceError>) -> bool {
        let listed = self.questions_listed(result);
        if listed.question_changed && self.app.focus == Focus::Block {
            self.drop_next_block_key = true;
        }
        listed.changed
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
                        self.app.handle_key(KeyCode::Char('g'));
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
            Effect::Defer(id) => Control::Defer(id),
            Effect::OpenLink(link) => Control::OpenLink(link),
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
        handle_input, merge_exit, open_stream, resolve_client, spawn_focus_worker, wait_for_retry,
    };
    use crate::api::SourceError;
    use crate::app::Focus;
    use crate::app::{
        AgentIncarnation, AgentRow, AgentState, CallAction, CallState, CallView, ConnectionState,
        FocusReading, MonitorHealth, MonitorHealthState, RuntimeStatus, Snapshot,
    };
    use crate::navigation::{NavigationError, PaneNavigator};
    use crate::questions::{
        AnswerKind, DecisionView, OptionView, Outgoing, PickView, QuestionView, Queue, SendResult,
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
            core.handle_key(KeyCode::Char('r'), &navigator),
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
            .try_send(InputMessage::Key(KeyCode::Char('r').into()))
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
            .try_send(InputMessage::Key(KeyCode::Char('r').into()))
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
            .try_send(InputMessage::Key(KeyCode::Char('r').into()))
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
            draws: 0,
            fail: true,
            times: Vec::new(),
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
            .try_send(InputMessage::Key(KeyCode::Char('r').into()))
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
            deferred: false,
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
        for key in keys.iter().copied().chain([KeyCode::Char('q')]) {
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
        probe.lock().unwrap().log.clone()
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

    #[tokio::test]
    async fn a_block_key_lists_first() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 1, 2)));
        let log = run_question_stream(
            &mut core,
            &probe,
            &[KeyCode::Char('j'), KeyCode::Tab, KeyCode::Up, KeyCode::Tab],
        )
        .await;
        // Once as the stream opens, then before Tab, Up and Tab; not before j or q.
        assert_eq!(log, lists(4));
        assert_eq!(core.app.questions.view().unwrap().highlighted, 0);
        assert_eq!(core.app.focus, Focus::List);
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
        run_question_stream(
            &mut core,
            &probe,
            &[KeyCode::Tab, KeyCode::Enter, KeyCode::Enter, KeyCode::Tab],
        )
        .await;
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.ask.as_deref(), Some("Ask 1?"));
        assert!(view.picks.is_empty(), "the second Enter was not applied");
        assert_eq!(
            view.notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
    }

    #[tokio::test]
    async fn enter_on_the_summary_calls_answer_question() {
        let mut core = connected_core();
        let probe = question_probe(Some(question("Q1", 3, 1)));
        probe
            .lock()
            .unwrap()
            .answers
            .push_back((SendResult::Recorded, Some(Queue::default())));
        let log = run_question_stream(
            &mut core,
            &probe,
            &[KeyCode::Tab, KeyCode::Enter, KeyCode::Enter],
        )
        .await;
        let mut expected = lists(4);
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
            }]
        );
        assert!(!core.app.questions.is_visible());
        assert_eq!(core.app.focus, Focus::List);
    }

    #[tokio::test]
    async fn l_calls_defer_question() {
        let mut core = connected_core();
        let mut queue = question("Q1", 1, 1);
        queue.questions.push(question_view("Q2", 1, 1));
        queue.waiting = 2;
        let probe = question_probe(Some(queue));
        let log = run_question_stream(
            &mut core,
            &probe,
            &[KeyCode::Tab, KeyCode::Char('l'), KeyCode::Tab],
        )
        .await;
        let mut expected = lists(3);
        expected.extend([
            "defer Q1".to_string(),
            "list".to_string(),
            "list".to_string(),
        ]);
        assert_eq!(log, expected);
        let view = core.app.questions.view().unwrap();
        assert_eq!((view.id.as_str(), view.position), ("Q2", 1));
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
        let enter3 = [KeyCode::Enter, KeyCode::Enter, KeyCode::Enter];
        let keys: Vec<KeyCode> = [KeyCode::Tab]
            .into_iter()
            .chain(enter3)
            .chain([KeyCode::Tab])
            .collect();
        let log = run_question_stream(&mut core, &probe, &keys).await;
        assert!(log.contains(&"answer Q1 v1".to_string()));
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.ask.as_deref(), Some("Changed?"));
        assert!(view.picks.is_empty());
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
        let keys: Vec<KeyCode> = [KeyCode::Tab].into_iter().chain(enter3).collect();
        let log = run_question_stream(&mut core, &probe, &keys).await;
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
        let log = run_question_stream(
            &mut core,
            &probe,
            &[KeyCode::Tab, KeyCode::Enter, KeyCode::Enter, KeyCode::Tab],
        )
        .await;
        assert!(log.contains(&"answer Q1 v1".to_string()));
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.mode, crate::questions::Mode::Summary);
        assert_eq!(view.picks.len(), 1);
        assert_eq!(
            view.notice.as_deref(),
            Some("Harold can't save answers right now")
        );
    }

    #[tokio::test]
    async fn a_poll_found_change_drops_the_next_block_key() {
        let mut core = connected_core();
        core.question_poll_interval = Duration::from_millis(10);
        let probe = question_probe(Some(question("Q1", 1, 2)));
        let mut stream = FakeStream::pending(Arc::new(AtomicBool::new(false)));
        stream.questions = Some(Arc::clone(&probe));
        let (input_sender, mut input) = InputPump::channel();
        let key = |code: KeyCode| InputMessage::Key(code.into());
        input_sender.try_send(key(KeyCode::Tab)).unwrap();
        input_sender.try_send(key(KeyCode::Char('r'))).unwrap();
        let tool = Arc::clone(&probe);
        tokio::spawn(async move {
            // The question changes; well after a poll has found version 2, the user,
            // still typing the note, presses `l`: as a block key it would defer.
            tokio::time::sleep(Duration::from_millis(50)).await;
            tool.lock().unwrap().queue = Some(question("Q1", 2, 2));
            tokio::time::sleep(Duration::from_millis(150)).await;
            for code in [KeyCode::Char('l'), KeyCode::Tab, KeyCode::Char('q')] {
                input_sender.send(key(code)).await.unwrap();
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
        let log = probe.lock().unwrap().log.clone();
        assert!(!log.iter().any(|call| call.starts_with("defer")), "{log:?}");
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.mode, crate::questions::Mode::Choosing);
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
        input_sender.try_send(key(KeyCode::Tab)).unwrap();
        input_sender.try_send(key(KeyCode::Char('r'))).unwrap();
        let tool = Arc::clone(&probe);
        tokio::spawn(async move {
            // A poll finds version 2 while the user is typing the note "later".
            tokio::time::sleep(Duration::from_millis(50)).await;
            tool.lock().unwrap().queue = Some(question("Q1", 2, 2));
            tokio::time::sleep(Duration::from_millis(150)).await;
            let typed = "later".chars().map(KeyCode::Char).chain([KeyCode::Enter]);
            for code in typed.chain([KeyCode::Tab, KeyCode::Char('q')]) {
                input_sender.send(key(code)).await.unwrap();
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
        let log = probe.lock().unwrap().log.clone();
        assert!(!log.iter().any(|call| call.starts_with("defer")), "{log:?}");
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.mode, crate::questions::Mode::Choosing);
        assert!(view.picks.is_empty(), "Enter picked nothing");
        assert_eq!(
            view.notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
        assert!(!core.app.holding_block_keys(), "Tab ended the hold");
    }

    #[test]
    fn a_second_version_bump_is_flagged() {
        let mut core = connected_core();
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 2));
        core.handle_key(KeyCode::Tab, &navigator);
        assert!(core.questions_refreshed(Ok(question("Q1", 2, 2))));
        assert!(core.drop_next_block_key);
        core.drop_next_block_key = false;
        // The notice reads the same after a second bump, but the version differs.
        assert!(core.questions_refreshed(Ok(question("Q1", 3, 2))));
        assert!(core.drop_next_block_key, "the second bump is flagged too");
        core.drop_next_block_key = false;
        assert!(!core.questions_refreshed(Ok(question("Q1", 3, 2))));
        assert!(!core.drop_next_block_key, "an unchanged read flags nothing");
        // Without the focus there is nothing to drop.
        core.handle_key(KeyCode::Tab, &navigator);
        assert!(core.questions_refreshed(Ok(question("Q1", 4, 2))));
        assert!(!core.drop_next_block_key);
    }

    #[test]
    fn a_failed_list_keeps_the_picks_for_the_next_list() {
        let mut core = connected_core();
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 2));
        core.handle_key(KeyCode::Tab, &navigator);
        core.handle_key(KeyCode::Enter, &navigator);
        core.questions_listed(Err(SourceError::Transport("unavailable".into())));
        assert!(!core.app.questions.is_visible());
        assert_eq!(core.app.runtime_status(), None);
        core.questions_listed(Ok(question("Q1", 1, 2)));
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(view.picks, vec!["label b".to_string()]);
        assert_eq!(view.notice, None, "a failed list leaves no notice");
    }

    #[test]
    fn a_failed_list_then_a_new_version_clears_the_picks() {
        let mut core = connected_core();
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 2));
        core.handle_key(KeyCode::Tab, &navigator);
        core.handle_key(KeyCode::Enter, &navigator);
        core.questions_listed(Err(SourceError::Transport("unavailable".into())));
        core.questions_listed(Ok(question("Q1", 2, 2)));
        let view = core.app.questions.view().unwrap();
        assert_eq!(view.ask.as_deref(), Some("Ask 1?"));
        assert!(view.picks.is_empty());
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
            KeyCode::Tab,
            KeyCode::Char('r'),
            KeyCode::Char('a'),
            KeyCode::Char('b'),
            KeyCode::Char('c'),
            KeyCode::Tab,
        ];
        run_question_stream(&mut core, &probe, &keys).await;
        // The stream ends: run_async hides the block.
        core.app.hide_questions();
        assert!(!core.app.questions.is_visible());
        let log = run_question_stream(
            &mut core,
            &probe,
            &[KeyCode::Tab, KeyCode::Char('d'), KeyCode::Tab],
        )
        .await;
        assert_eq!(log, lists(7 + 4), "every key went to the block or was Tab");
        assert_eq!(
            core.app.questions.view().unwrap().mode,
            crate::questions::Mode::Note {
                kind: crate::questions::NoteKind::Revise,
                text: "abcd".into(),
            }
        );
    }

    #[tokio::test]
    async fn unimplemented_hides_the_block() {
        let mut core = connected_core();
        let navigator = FakeNavigator::successful();
        core.app.observe_questions(question("Q1", 1, 1));
        core.handle_key(KeyCode::Tab, &navigator);
        assert_eq!(core.app.focus, Focus::Block);
        // An older Harold answers Unimplemented; Harold down fails the same way.
        let probe = question_probe(None);
        // The keys are swallowed until Esc, so `j` (and `q`, before Esc) do nothing.
        let log = run_question_stream(&mut core, &probe, &[KeyCode::Char('j'), KeyCode::Esc]).await;
        assert_eq!(log, lists(1));
        assert!(
            !core.app.questions.is_visible(),
            "no withdrawn notice either"
        );
        assert_eq!(core.app.focus, Focus::List);
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
            KeyCode::Tab,
            KeyCode::Char('o'),
            KeyCode::Tab,
            KeyCode::Char('q'),
        ] {
            input_sender
                .try_send(InputMessage::Key(key.into()))
                .unwrap();
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

    #[derive(Default)]
    struct FakeScreen {
        draws: usize,
        fail: bool,
        times: Vec<i64>,
    }

    impl ScreenPort for FakeScreen {
        fn draw_screen(
            &mut self,
            _app: &crate::app::App,
            now_ms: i64,
        ) -> Result<(), crate::terminal::TerminalError> {
            self.draws += 1;
            self.times.push(now_ms);
            if self.fail {
                return Err(crate::terminal::TerminalError::new(
                    "render dashboard",
                    io::Error::other("injected render failure"),
                ));
            }
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
    }

    struct FakeSource {
        open: std::sync::Mutex<Option<FakeOpen>>,
    }

    impl FakeSource {
        fn error(error: SourceError) -> Self {
            Self {
                open: std::sync::Mutex::new(Some(FakeOpen::Error(error))),
            }
        }

        fn pending(dropped: Arc<AtomicBool>) -> Self {
            Self {
                open: std::sync::Mutex::new(Some(FakeOpen::Pending(dropped))),
            }
        }
    }

    impl SourcePort for FakeSource {
        type Stream = FakeStream;

        fn open_stream(&self) -> BoxFuture<'_, Result<Self::Stream, SourceError>> {
            let behavior = self.open.lock().unwrap().take().unwrap();
            match behavior {
                FakeOpen::Error(error) => Box::pin(async move { Err(error) }),
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
    }

    /// The question tool as a test sees it; every call is logged.
    #[derive(Default)]
    struct QuestionState {
        log: Vec<String>,
        /// What a list answers; `None` fails it, as an older Harold without the tool does.
        queue: Option<Queue>,
        /// Answers for the next lists, in order; each one also becomes `queue`.
        on_list: VecDeque<Option<Queue>>,
        /// Each answer takes the next result, and the queue lists show after it.
        answers: VecDeque<(SendResult, Option<Queue>)>,
        sent: Vec<Outgoing>,
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
            let answer = state
                .queue
                .clone()
                .ok_or_else(|| SourceError::Transport("status: Unimplemented".into()));
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

        fn defer_question<'a>(&'a mut self, id: &'a str) -> BoxFuture<'a, Result<(), SourceError>> {
            let Some(probe) = &self.questions else {
                return Box::pin(async { Err(SourceError::Transport("no question tool".into())) });
            };
            let mut state = probe.lock().unwrap();
            state.log.push(format!("defer {id}"));
            if let Some(queue) = state.queue.as_mut()
                && let Some(index) = queue.questions.iter().position(|q| q.id == id)
            {
                let mut question = queue.questions.remove(index);
                question.deferred = true;
                queue.questions.push(question);
            }
            Box::pin(async { Ok(()) })
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
