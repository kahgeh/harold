//! Fakes for tests. `FakeEnvironment` is a `NotifyEnvironment` with a clock the test
//! moves, a screen lock and tmux answers the test sets, and sends that are recorded
//! instead of made. `CapturedLogs` collects the log lines a test writes.

use std::cell::RefCell;
use std::io::Write;
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{DeliveryOutcome, NotifyEnvironment, OutboundChannel};
use crate::agent::domain::CompletionSummaryUpdate;
use crate::inbound::AgentAddress;
use crate::settings::{Channel, NotifySettings};
use crate::store::{AwayMessageRequested, TurnCompleted};

pub(crate) fn turn(pane_id: &str, prompt: &str) -> TurnCompleted {
    TurnCompleted {
        pane_id: pane_id.into(),
        pane_label: format!("label-{pane_id}"),
        last_user_prompt: prompt.into(),
        assistant_message: "message".into(),
        main_context: "ctx".into(),
        agent_incarnation: None,
        work_summary: CompletionSummaryUpdate::Unchanged,
    }
}

pub(crate) struct FakeEnvironment {
    started: Instant,
    elapsed: Mutex<Duration>,
    settings: NotifySettings,
    screen_locked: AtomicBool,
    fail_sends: AtomicBool,
    /// The error the next turn-end send fails with, once.
    fail_next: Mutex<Option<String>>,
    delivered: Mutex<Vec<(OutboundChannel, String)>>,
    away_messages: Mutex<Vec<String>>,
    reply_targets: Mutex<Vec<String>>,
    spoken_lines: Mutex<Vec<String>>,
    /// The channels the test has marked not set up.
    not_set_up: Mutex<Vec<Channel>>,
}

impl FakeEnvironment {
    /// The shipped default settings with `overrides` (TOML) laid over them, except that each
    /// channel list the overrides leave out is `["*"]`: every pane may use it, so a test
    /// that is not about the lists need not name a folder. The screen starts unlocked and sends succeed.
    pub(crate) fn new(overrides: &str) -> Self {
        let mut notify = crate::settings::settings_with(overrides).notify;
        for (key, list) in [
            ("imessage", &mut notify.imessage),
            ("web_app", &mut notify.web_app),
            ("machine_audio_output", &mut notify.machine_audio_output),
        ] {
            if !overrides.contains(&format!("{key} =")) {
                *list = vec!["*".to_string()];
            }
        }
        Self {
            started: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
            settings: notify,
            screen_locked: AtomicBool::new(false),
            fail_sends: AtomicBool::new(false),
            fail_next: Mutex::new(None),
            delivered: Mutex::new(Vec::new()),
            away_messages: Mutex::new(Vec::new()),
            reply_targets: Mutex::new(Vec::new()),
            spoken_lines: Mutex::new(Vec::new()),
            not_set_up: Mutex::new(Vec::new()),
        }
    }

    /// As `new`, with the screen locked.
    pub(crate) fn locked(overrides: &str) -> Self {
        let environment = Self::new(overrides);
        environment.set_screen_locked(true);
        environment
    }

    pub(crate) fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
    }

    pub(crate) fn set_screen_locked(&self, locked: bool) {
        self.screen_locked.store(locked, Ordering::SeqCst);
    }

    pub(crate) fn set_fail_sends(&self, fail: bool) {
        self.fail_sends.store(fail, Ordering::SeqCst);
    }

    /// Makes the next turn-end send fail with `error`, and the ones after it succeed.
    pub(crate) fn fail_next_send_with(&self, error: &str) {
        *self.fail_next.lock().unwrap() = Some(error.to_string());
    }

    /// Each turn-end update delivered: the channel and the pane.
    pub(crate) fn delivered(&self) -> Vec<(OutboundChannel, String)> {
        self.delivered.lock().unwrap().clone()
    }

    /// Marks `channel` as not set up (every channel starts set up).
    pub(crate) fn set_not_set_up(&self, channel: Channel) {
        self.not_set_up.lock().unwrap().push(channel);
    }

    /// The text of each requested message sent.
    pub(crate) fn away_messages(&self) -> Vec<String> {
        self.away_messages.lock().unwrap().clone()
    }

    /// The pane of each reply target recorded, oldest first.
    pub(crate) fn reply_targets(&self) -> Vec<String> {
        self.reply_targets.lock().unwrap().clone()
    }

    /// Each spoken line said at the desk.
    pub(crate) fn spoken_lines(&self) -> Vec<String> {
        self.spoken_lines.lock().unwrap().clone()
    }
}

impl NotifyEnvironment for FakeEnvironment {
    fn now(&self) -> Instant {
        self.started + *self.elapsed.lock().unwrap()
    }

    fn notify_settings(&self) -> &NotifySettings {
        &self.settings
    }

    fn screen_locked(&self) -> bool {
        self.screen_locked.load(Ordering::SeqCst)
    }

    fn deliver(
        &self,
        channel: &OutboundChannel,
        turn: &TurnCompleted,
        _trace_id: &str,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String> {
        if let Some(error) = self.fail_next.lock().unwrap().take() {
            return Err(error);
        }
        if self.fail_sends.load(Ordering::SeqCst) {
            return Err("channel down".into());
        }
        self.delivered
            .lock()
            .unwrap()
            .push((*channel, turn.pane_id.clone()));
        let source = match channel {
            OutboundChannel::Away => AgentAddress::TmuxPane {
                pane_id: turn.pane_id.clone(),
                label: turn.pane_label.clone(),
            },
        };
        Ok((DeliveryOutcome::Delivered, Some(source)))
    }

    fn send_away_message(&self, message: &AwayMessageRequested) -> Result<(), String> {
        if self.fail_sends.load(Ordering::SeqCst) {
            return Err("channel down".into());
        }
        self.away_messages
            .lock()
            .unwrap()
            .push(message.text.clone());
        Ok(())
    }

    fn speak_line(&self, line: &str) -> Result<(), String> {
        if self.fail_sends.load(Ordering::SeqCst) {
            return Err("TTS command failed".into());
        }
        self.spoken_lines.lock().unwrap().push(line.to_string());
        Ok(())
    }

    fn record_reply_target(&self, agent: AgentAddress) {
        self.reply_targets
            .lock()
            .unwrap()
            .push(agent.pane_id().to_string());
    }

    fn channel_set_up(&self, channel: Channel) -> bool {
        !self.not_set_up.lock().unwrap().contains(&channel)
    }
}

thread_local! {
    /// The lines captured on this thread. `Some` while a `CapturedLogs` is alive on it.
    static CAPTURED: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// Appends to the current thread's capture, and discards what is written while the thread
/// has none.
pub(crate) struct CapturedLogWriter;

impl Write for CapturedLogWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        CAPTURED.with(|captured| {
            if let Some(bytes) = captured.borrow_mut().as_mut() {
                bytes.extend(buffer);
            }
        });
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs the test binary's one log subscriber, once. Every thread writes through it,
/// and `CapturedLogWriter` keeps only the lines of a thread that is capturing. The same
/// subscriber also exports to the in-memory telemetry exporters.
fn install_capturing_subscriber() {
    crate::telemetry::test_support::install();
}

/// The log lines written on the current thread from `start` until this is dropped. Code
/// that runs on another thread, such as a `spawn_blocking` task, is not captured.
///
/// All captures share one process-wide subscriber. A subscriber for each capture, set for
/// its thread alone, raced with the other test threads setting and dropping theirs, and
/// now and then a line was not written.
pub(crate) struct CapturedLogs {
    /// Keeps this on the thread whose lines it holds: a raw pointer is not `Send`.
    _on_this_thread: PhantomData<*const ()>,
}

impl CapturedLogs {
    pub(crate) fn start() -> Self {
        install_capturing_subscriber();
        CAPTURED.with(|captured| *captured.borrow_mut() = Some(Vec::new()));
        Self {
            _on_this_thread: PhantomData,
        }
    }

    /// Everything logged so far, one line for each event: the level, the module, the
    /// message and then the fields, as in `INFO harold::outbound: text pane_id=%2`.
    pub(crate) fn text(&self) -> String {
        CAPTURED.with(|captured| {
            String::from_utf8_lossy(captured.borrow().as_deref().unwrap_or_default()).into_owned()
        })
    }
}

impl Drop for CapturedLogs {
    fn drop(&mut self) {
        CAPTURED.with(|captured| *captured.borrow_mut() = None);
    }
}
