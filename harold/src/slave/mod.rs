//! The link between a master Harold and a slave Harold on another machine.

use std::time::Instant;

use crate::inbound::AgentAddress;
use crate::outbound::{DeliveryOutcome, NotifyEnvironment, OutboundChannel};
use crate::settings::{Channel, NotifySettings, Settings};
use crate::store::{AwayMessageRequested, TurnCompleted};

pub(crate) mod backlog;
pub(crate) mod master;
pub(crate) mod pairing;
#[allow(clippy::module_inception)]
pub(crate) mod slave;

/// Whether this Harold tells the user itself about a turn end or a question.
///
/// A paired slave (a `[master]` table) does not: only the master tells the user. It still
/// records the turn in its own store and sends it up as a `Turn`. An unpaired Harold keeps its
/// stand-alone behaviour.
pub(crate) fn delivers_locally(settings: &Settings) -> bool {
    settings.master.is_none()
}

/// A `NotifyEnvironment` that sends nothing when `local` is false: no phone message, no
/// push, no spoken line. The decision and the records before the send are the same.
pub(crate) struct DeliveryGate<E> {
    inner: E,
    local: bool,
}

impl<E> DeliveryGate<E> {
    pub(crate) fn new(inner: E, local: bool) -> Self {
        Self { inner, local }
    }
}

impl<E: NotifyEnvironment> NotifyEnvironment for DeliveryGate<E> {
    fn now(&self) -> Instant {
        self.inner.now()
    }

    fn notify_settings(&self) -> &NotifySettings {
        self.inner.notify_settings()
    }

    fn screen_locked(&self) -> bool {
        self.inner.screen_locked()
    }

    fn deliver(
        &self,
        channel: &OutboundChannel,
        turn: &TurnCompleted,
        trace_id: &str,
    ) -> Result<(DeliveryOutcome, Option<AgentAddress>), String> {
        if !self.local {
            return Ok((DeliveryOutcome::Skipped, None));
        }
        self.inner.deliver(channel, turn, trace_id)
    }

    fn send_away_message(&self, message: &AwayMessageRequested) -> Result<(), String> {
        if !self.local {
            return Ok(());
        }
        self.inner.send_away_message(message)
    }

    fn speak_line(&self, line: &str) -> Result<(), String> {
        if !self.local {
            return Ok(());
        }
        self.inner.speak_line(line)
    }

    fn record_reply_target(&self, agent: AgentAddress) {
        if self.local {
            self.inner.record_reply_target(agent);
        }
    }

    fn channel_set_up(&self, channel: Channel) -> bool {
        self.local && self.inner.channel_set_up(channel)
    }
}

#[cfg(test)]
#[path = "gate_tests.rs"]
mod gate_tests;
