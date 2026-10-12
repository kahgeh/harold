//! A `NotifyEnvironment` for tests: a clock the test moves, a screen lock and tmux answers
//! the test sets, and sends that are recorded instead of made.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{DeliveryOutcome, NotifyEnvironment, OutboundChannel};
use crate::agent::domain::CompletionSummaryUpdate;
use crate::inbound::AgentAddress;
use crate::settings::NotifySettings;
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
    session_attached: AtomicBool,
    active_pane: Mutex<Option<String>>,
    fail_sends: AtomicBool,
    delivered: Mutex<Vec<(OutboundChannel, String)>>,
    away_messages: Mutex<Vec<String>>,
    reply_targets: Mutex<Vec<String>>,
}

impl FakeEnvironment {
    /// The shipped default settings with `overrides` (TOML) laid over them. The screen
    /// starts unlocked, no tmux client is attached, and sends succeed.
    pub(crate) fn new(overrides: &str) -> Self {
        Self {
            started: Instant::now(),
            elapsed: Mutex::new(Duration::ZERO),
            settings: crate::peers::settings_with(overrides).notify,
            screen_locked: AtomicBool::new(false),
            session_attached: AtomicBool::new(false),
            active_pane: Mutex::new(None),
            fail_sends: AtomicBool::new(false),
            delivered: Mutex::new(Vec::new()),
            away_messages: Mutex::new(Vec::new()),
            reply_targets: Mutex::new(Vec::new()),
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

    pub(crate) fn set_session_attached(&self, attached: bool) {
        self.session_attached.store(attached, Ordering::SeqCst);
    }

    pub(crate) fn set_active_pane(&self, pane_id: Option<&str>) {
        *self.active_pane.lock().unwrap() = pane_id.map(str::to_string);
    }

    pub(crate) fn set_fail_sends(&self, fail: bool) {
        self.fail_sends.store(fail, Ordering::SeqCst);
    }

    /// Each turn-end update delivered: the channel and the pane.
    pub(crate) fn delivered(&self) -> Vec<(OutboundChannel, String)> {
        self.delivered.lock().unwrap().clone()
    }

    /// The text of each requested message sent.
    pub(crate) fn away_messages(&self) -> Vec<String> {
        self.away_messages.lock().unwrap().clone()
    }

    /// The pane of each reply target recorded, oldest first.
    pub(crate) fn reply_targets(&self) -> Vec<String> {
        self.reply_targets.lock().unwrap().clone()
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

    fn session_attached(&self, _pane_id: &str) -> bool {
        self.session_attached.load(Ordering::SeqCst)
    }

    fn active_pane(&self, _pane_id: &str) -> Option<String> {
        self.active_pane.lock().unwrap().clone()
    }

    fn deliver(
        &self,
        channel: &OutboundChannel,
        turn: &TurnCompleted,
        _trace_id: &str,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String> {
        if self.fail_sends.load(Ordering::SeqCst) {
            return Err("channel down".into());
        }
        self.delivered
            .lock()
            .unwrap()
            .push((*channel, turn.pane_id.clone()));
        let source = match channel {
            OutboundChannel::Tts => None,
            OutboundChannel::Away => Some(AgentAddress::TmuxPane {
                pane_id: turn.pane_id.clone(),
                label: turn.pane_label.clone(),
            }),
        };
        Ok((DeliveryOutcome::Delivered, source))
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

    fn record_reply_target(&self, agent: AgentAddress) {
        self.reply_targets
            .lock()
            .unwrap()
            .push(agent.pane_id().to_string());
    }
}
