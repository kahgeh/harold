use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use tokio::runtime::Builder;
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::api::{AgentStateSource, SourceError, SourceStream};
use crate::app::{
    App, ConnectionState, Effect, RuntimeStatus, SearchState, Snapshot, SnapshotError,
};
use crate::cli::Options;
use crate::navigation::{NavigationError, PaneNavigator, TmuxNavigator};
use crate::terminal::{TerminalError, TerminalGuard};
use crate::text::sanitize_display;

const ERROR_LIMIT: usize = 512;
const RETRY_DELAYS_MS: [u64; 6] = [250, 500, 1_000, 2_000, 4_000, 5_000];
const INPUT_CHANNEL_CAPACITY: usize = 32;
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);
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
                Control::Retry | Control::Continue => {}
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

        match consume_stream(
            stream,
            &mut core,
            &mut input,
            &mut shutdown,
            terminal,
            navigator,
            &clock,
        )
        .await
        {
            Ok(Control::Quit) => return Ok(()),
            Ok(Control::Retry) => {
                retry_immediately = true;
            }
            Ok(Control::Continue) => {
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
                    Control::Continue => draw(terminal, &core.app, clock)?,
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
        loop {
            tokio::select! {
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
                let control = handle_input(item, core, navigator)?;
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
                if control != Control::Continue {
                    return Ok(control);
                }
            }
            () = shutdown.recv_shutdown() => return Ok(RuntimeCore::shutdown()),
        }
    }
}

fn handle_input(
    item: Option<Result<RuntimeInput, String>>,
    core: &mut RuntimeCore,
    navigator: &impl PaneNavigator,
) -> Result<Control, AppError> {
    match item {
        Some(Ok(RuntimeInput::Key(key))) => Ok(core.handle_key(key, navigator)),
        Some(Ok(RuntimeInput::Redraw)) => Ok(Control::Continue),
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
    Key(KeyCode),
    Redraw,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeInput {
    Key(KeyCode),
    Redraw,
}

struct InputPump {
    receiver: mpsc::Receiver<InputMessage>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
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
        Self::start_with(CrosstermInput)
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
                            if sender.try_send(InputMessage::Key(key.code)).is_err()
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
                redraw_pending: Arc::new(AtomicBool::new(false)),
            },
        )
    }

    async fn recv(&mut self) -> Option<Result<RuntimeInput, String>> {
        self.receiver.recv().await.map(|message| match message {
            InputMessage::Key(key) => Ok(RuntimeInput::Key(key)),
            InputMessage::Redraw => {
                self.redraw_pending.store(false, Ordering::Release);
                Ok(RuntimeInput::Redraw)
            }
            InputMessage::Error(detail) => Err(detail),
        })
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Continue,
    Retry,
    Quit,
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

    fn handle_key(&mut self, key: KeyCode, navigator: &impl PaneNavigator) -> Control {
        match self.app.handle_key(key) {
            Effect::None => Control::Continue,
            Effect::Retry => Control::Retry,
            Effect::Quit => Control::Quit,
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
        Backoff, BoxFuture, ClockPort, Control, InputMessage, InputPump, OpenOutcome,
        RedrawCadence, RuntimeCore, RuntimeInput, ScreenPort, ShutdownPort, SourcePort,
        StreamControl, StreamPort, TerminalInput, consume_stream, merge_exit, open_stream,
        resolve_client, wait_for_retry,
    };
    use crate::api::SourceError;
    use crate::app::{
        AgentIncarnation, AgentRow, AgentState, ConnectionState, MonitorHealth, MonitorHealthState,
        RuntimeStatus, Snapshot,
    };
    use crate::navigation::{NavigationError, PaneNavigator};

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
            core.handle_key(KeyCode::Char('/'), &navigator),
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
            core.handle_key(KeyCode::Char('/'), &navigator),
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
            core.handle_key(KeyCode::Char('/'), &navigator),
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
            .try_send(InputMessage::Key(KeyCode::Char('r')))
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
            .try_send(InputMessage::Key(KeyCode::Char('r')))
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
            .try_send(InputMessage::Key(KeyCode::Char('r')))
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
            .try_send(InputMessage::Key(KeyCode::Char('r')))
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
            }
        }

        fn one_snapshot(closed: Arc<AtomicBool>, snapshot: Snapshot) -> Self {
            Self {
                closed,
                receive: FakeReceive::Item(Some(Ok(snapshot))),
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
}
