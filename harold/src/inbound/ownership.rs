//! Which Mac owns a reply.
//!
//! Several Macs can read the same reply. Each message Harold sends names its Mac. When the
//! `machines` setting lists your Macs, each Harold keeps the name from the latest
//! notification it saw, and a reply that names no Mac belongs to the Mac that notified
//! last.

use std::sync::{LazyLock, Mutex};

use super::parse_tag;
use crate::settings::{get_settings, is_valid_machine_name};

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

/// The `machines` setting, and the Mac that sent the latest notification.
///
/// Multi-Mac routing is on only when `machines` is set. While it is empty the record
/// accepts nothing: it keeps no latest notifier and knows no machine name, so this Mac owns
/// every untagged reply, as a single Mac always has.
#[derive(Debug)]
pub(crate) struct NotifierRecord {
    /// The `machines` setting. Only these names are machine names.
    machines: Vec<String>,
    latest: Option<Latest>,
}

/// The latest notification recorded.
#[derive(Debug)]
struct Latest {
    machine: String,
    /// The `date` of the row it was read from. `None` when it was recorded by name with
    /// `record_notification`, which has no row.
    date: Option<i64>,
}

impl NotifierRecord {
    pub(crate) const fn new() -> Self {
        Self {
            machines: Vec::new(),
            latest: None,
        }
    }

    /// The record of a Mac whose `machines` setting is `machines`.
    pub(crate) fn for_machines(machines: &[String]) -> Self {
        Self {
            machines: machines.to_vec(),
            ..Self::new()
        }
    }

    /// Record a message Harold sent, as read back from the conversation, with the `date`
    /// Messages stored with its row. Only a notification from a listed machine counts.
    ///
    /// The latest notifier is the one with the greatest `date`; two rows with the same
    /// `date` are ordered by machine name, ignoring case, and the greater name wins. So rows
    /// can be fed in any order and give the same latest notifier. A row replaces a latest
    /// notifier recorded by name (`record_notification`), which has no date.
    pub(crate) fn record_marker(&mut self, text: &str, date: i64) {
        let Some(marker) = parse_marker(text) else {
            return;
        };
        if !marker.is_notification || !self.is_listed(marker.machine) {
            return;
        }
        let is_later = match &self.latest {
            Some(Latest {
                machine,
                date: Some(latest_date),
            }) => (date, marker.machine.to_lowercase()) > (*latest_date, machine.to_lowercase()),
            _ => true,
        };
        if is_later {
            self.latest = Some(Latest {
                machine: marker.machine.to_string(),
                date: Some(date),
            });
        }
    }

    /// Record that `machine` sent a notification just now. A name that is not listed or is
    /// not a valid machine name changes nothing, and so does every name while `machines` is
    /// empty.
    ///
    /// It has no row and so no `date`: it is ordered by arrival. It replaces any latest
    /// notifier, from a row or by name, and the next row recorded replaces it.
    pub(crate) fn record_notification(&mut self, machine: &str) {
        if !is_valid_machine_name(machine) || !self.is_listed(machine) {
            return;
        }
        self.latest = Some(Latest {
            machine: machine.to_string(),
            date: None,
        });
    }

    pub(crate) fn latest_notifier(&self) -> Option<&str> {
        self.latest.as_ref().map(|latest| latest.machine.as_str())
    }

    /// The `machines` setting. This Mac's own name is not added: `decide_ownership`
    /// recognises it as `this_machine`.
    pub(crate) fn known_machine_names(&self) -> &[String] {
        &self.machines
    }

    fn is_listed(&self, machine: &str) -> bool {
        self.machines
            .iter()
            .any(|listed| same_machine(listed, machine))
    }
}

/// The record of this process, built from the `machines` setting when it is first used.
static NOTIFIERS: LazyLock<Mutex<NotifierRecord>> =
    LazyLock::new(|| Mutex::new(NotifierRecord::for_machines(&get_settings().machines)));

/// Every marker fed to `record_marker`, so a test can see that a caller feeds the
/// process-wide record while the test settings list no machines and the record keeps none.
#[cfg(test)]
static FED_MARKERS: Mutex<Vec<(String, i64)>> = Mutex::new(Vec::new());

/// Record a `🤖 [...]` message read from the conversation, with the `date` of its row.
pub(crate) fn record_marker(text: &str, date: i64) {
    #[cfg(test)]
    FED_MARKERS.lock().unwrap().push((text.to_string(), date));
    NOTIFIERS.lock().unwrap().record_marker(text, date);
}

/// Every name passed to `record_notification`, so a test can see that a caller reaches the
/// process-wide record while the test settings list no machines and the record keeps none.
#[cfg(test)]
static RECORDED_NOTIFICATIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record a notification by the name of the Mac that sent it. Only for a channel one Mac
/// reads: the hub calls it for its own notifications and for each member's report.
pub(crate) fn record_notification(machine: &str) {
    #[cfg(test)]
    RECORDED_NOTIFICATIONS
        .lock()
        .unwrap()
        .push(machine.to_string());
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

/// The known machine names: the `machines` setting. This Mac's own name is not added.
pub(crate) fn known_machine_names() -> Vec<String> {
    NOTIFIERS.lock().unwrap().known_machine_names().to_vec()
}

/// The markers fed to the process-wide record so far, in order.
#[cfg(test)]
pub(crate) fn fed_markers() -> Vec<(String, i64)> {
    FED_MARKERS.lock().unwrap().clone()
}

/// The names passed to `record_notification` so far, in order.
#[cfg(test)]
pub(crate) fn recorded_notifications() -> Vec<String> {
    RECORDED_NOTIFICATIONS.lock().unwrap().clone()
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
