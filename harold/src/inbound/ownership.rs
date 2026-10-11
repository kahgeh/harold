//! Which Mac owns a reply.
//!
//! Several Macs can read the same reply. Each message Harold sends names its Mac, and
//! each Harold keeps the name from the last notification it saw. A reply that names no
//! Mac belongs to the Mac that notified last.

use std::sync::Mutex;

use super::parse_tag;
use crate::settings::is_valid_machine_name;

/// Starts every message Harold sends.
const HAROLD_MARKER: char = '🤖';

// ---------------------------------------------------------------------------
// Markers — messages Harold sent that name the Mac they came from
// ---------------------------------------------------------------------------

/// The machine named at the start of a message Harold sent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Marker<'a> {
    pub(crate) machine: &'a str,
    /// True for `🤖 [machine/pane] …`, the form only a notification has. An error reply
    /// is `🤖 [machine] …`.
    pub(crate) is_notification: bool,
}

/// The marker in `text`, if it is a message Harold sent that names its Mac. A tag whose
/// machine part cannot be a machine name is a pane label from before machine names
/// (`🤖 [harold:0.1] …`) and is not a marker.
pub(crate) fn parse_marker(text: &str) -> Option<Marker<'_>> {
    let rest = text.trim_start().strip_prefix(HAROLD_MARKER)?.trim_start();
    let (tag, _) = rest.strip_prefix('[')?.split_once(']')?;
    let (machine, is_notification) = match tag.split_once('/') {
        Some((machine, _)) => (machine, true),
        None => (tag, false),
    };
    let machine = machine.trim();
    is_valid_machine_name(machine).then_some(Marker {
        machine,
        is_notification,
    })
}

pub(crate) fn same_machine(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

// ---------------------------------------------------------------------------
// Latest notifier — which Mac sent the last notification
// ---------------------------------------------------------------------------

/// The machine names seen in markers, and the one from the last notification.
#[derive(Debug)]
pub(crate) struct NotifierRecord {
    known: Vec<String>,
    latest: Option<String>,
}

impl NotifierRecord {
    pub(crate) const fn new() -> Self {
        Self {
            known: Vec::new(),
            latest: None,
        }
    }

    /// Record a message Harold sent, as read back from the conversation. Text that is
    /// not a marker changes nothing.
    pub(crate) fn record_marker(&mut self, text: &str) {
        let Some(marker) = parse_marker(text) else {
            return;
        };
        self.learn(marker.machine);
        if marker.is_notification {
            self.latest = Some(marker.machine.to_string());
        }
    }

    /// Record that `machine` sent a notification just now.
    pub(crate) fn record_notification(&mut self, machine: &str) {
        if !is_valid_machine_name(machine) {
            return;
        }
        self.learn(machine);
        self.latest = Some(machine.to_string());
    }

    pub(crate) fn latest_notifier(&self) -> Option<&str> {
        self.latest.as_deref()
    }

    pub(crate) fn known_machine_names(&self) -> &[String] {
        &self.known
    }

    fn learn(&mut self, machine: &str) {
        if !self.known.iter().any(|known| same_machine(known, machine)) {
            self.known.push(machine.to_string());
        }
    }
}

static NOTIFIERS: Mutex<NotifierRecord> = Mutex::new(NotifierRecord::new());

/// Record a `🤖 [...]` message read from the conversation.
#[allow(
    dead_code,
    reason = "called by the iMessage listener once it reads marked rows (piece 2)"
)]
pub(crate) fn record_marker(text: &str) {
    NOTIFIERS.lock().unwrap().record_marker(text);
}

/// Record a notification by the name of the Mac that sent it.
#[allow(
    dead_code,
    reason = "called by the hub for its own and its members' notifications (piece 3)"
)]
pub(crate) fn record_notification(machine: &str) {
    NOTIFIERS.lock().unwrap().record_notification(machine);
}

/// The Mac that sent the latest notification, if any has been recorded.
pub(crate) fn latest_notifier() -> Option<String> {
    NOTIFIERS
        .lock()
        .unwrap()
        .latest_notifier()
        .map(String::from)
}

/// Every machine name recorded so far. This Mac's own name is not added here.
pub(crate) fn known_machine_names() -> Vec<String> {
    NOTIFIERS.lock().unwrap().known_machine_names().to_vec()
}

// ---------------------------------------------------------------------------
// Ownership — does this Mac route the reply?
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ownership<'a> {
    /// This Mac routes the reply: `pane_tag` picks the pane and `body` is relayed.
    Owned {
        pane_tag: Option<&'a str>,
        body: &'a str,
    },
    /// The Mac named `owner` owns the reply. This Mac does not route it.
    NotOwned { owner: String },
}

/// Decide whether this Mac routes `text`.
///
/// A tag that names a machine (`[machine/pane]` or `[machine]`) settles it. Any other
/// reply belongs to the Mac that sent the latest notification, and to every Mac when none
/// is recorded. A forwarded reply is always owned: the hub decided before it forwarded.
pub(crate) fn decide_ownership<'a>(
    text: &'a str,
    forwarded: bool,
    this_machine: &str,
    known_machines: &[String],
    latest_notifier: Option<&str>,
) -> Ownership<'a> {
    let (tag, body) = parse_tag(text);
    let follows_latest_notifier = |pane_tag: Option<&'a str>| match latest_notifier {
        Some(latest) if !forwarded && !same_machine(latest, this_machine) => Ownership::NotOwned {
            owner: latest.to_string(),
        },
        _ => Ownership::Owned { pane_tag, body },
    };

    let Some(tag) = tag else {
        return follows_latest_notifier(None);
    };
    let (machine, pane) = match tag.split_once('/') {
        Some((machine, pane)) => (machine.trim(), Some(pane.trim())),
        None => (tag.trim(), None),
    };
    let pane_tag = pane.filter(|pane| !pane.is_empty());

    if same_machine(machine, this_machine) {
        return Ownership::Owned { pane_tag, body };
    }
    let named = known_machines
        .iter()
        .find(|known| same_machine(known, machine));
    if let Some(owner) = named {
        return if forwarded {
            Ownership::Owned { pane_tag, body }
        } else {
            Ownership::NotOwned {
                owner: owner.clone(),
            }
        };
    }
    // The tag names no machine: it is a pane tag, as before machine names.
    follows_latest_notifier(Some(tag))
}

// ---------------------------------------------------------------------------
// Text that names this Mac. The channel adds the 🤖 marker when it sends.
// ---------------------------------------------------------------------------

/// The full address of a pane. Pasted at the start of a reply, it reaches that pane.
pub(crate) fn pane_address(machine: &str, pane_label: &str) -> String {
    format!("[{machine}/{pane_label}]")
}

#[allow(
    dead_code,
    reason = "called by the channels when they build a notification (Task 6)"
)]
pub(crate) fn notification_text(
    machine: &str,
    pane_label: &str,
    body: &str,
    context: &str,
) -> String {
    format!(
        "{} {} ({context})",
        pane_address(machine, pane_label),
        body.trim()
    )
}

pub(crate) fn delivered_text(machine: &str, pane_label: &str) -> String {
    format!("✓ Delivered to {}", pane_address(machine, pane_label))
}

/// An error reply, prefixed with the Mac that sent it.
pub(crate) fn error_text(machine: &str, detail: &str) -> String {
    format!("[{machine}] {detail}")
}
#[cfg(test)]
#[path = "ownership_tests.rs"]
mod tests;
