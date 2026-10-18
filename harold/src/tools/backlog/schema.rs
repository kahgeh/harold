//! A backlog's items and notes, the chief's queue, and the limits they must
//! meet. Every limit
//! counts characters, not bytes, and "under N" means fewer than N.

/// At most this many items in one product's backlog.
pub(super) const MAX_ITEMS: usize = 5;
/// An item's title and its text fields are under this.
pub(super) const FIELD_UNDER: usize = 2_000;
/// An item has 1 to this many evidence pointers.
pub(super) const EVIDENCE_MAX: usize = 20;
/// Each evidence pointer is under this.
pub(super) const POINTER_UNDER: usize = 500;
/// A note's body is under this.
pub(super) const BODY_UNDER: usize = 20_000;
/// A queue item's text and the queue's note are under this.
pub(super) const QUEUE_TEXT_UNDER: usize = 4_000;

const MAX_ID: usize = 48;
const SIZES: [&str; 4] = ["", "small", "medium", "large"];

/// One ranked item in a product's backlog. `rank` and `updated_at_ms` are the
/// store's; a put ignores them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub id: String,
    pub title: String,
    pub value: String,
    pub size: String,
    pub risk: String,
    pub reason: String,
    pub paths: String,
    pub after: String,
    pub evidence: Vec<String>,
    pub rank: u32,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoteKind {
    /// The user's standing rules at the top of the backlog.
    Rule,
    /// The dated sections; append only.
    History,
    /// "In flight with another session".
    Held,
    /// "Looked at and left out", one note per entry.
    LeftOut,
}

impl NoteKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            NoteKind::Rule => "rule",
            NoteKind::History => "history",
            NoteKind::Held => "held",
            NoteKind::LeftOut => "left_out",
        }
    }

    pub(super) fn parse(kind: &str) -> Option<Self> {
        match kind {
            "rule" => Some(NoteKind::Rule),
            "history" => Some(NoteKind::History),
            "held" => Some(NoteKind::Held),
            "left_out" => Some(NoteKind::LeftOut),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Note {
    pub id: i64,
    pub kind: NoteKind,
    pub title: String,
    pub body: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueState {
    /// The one item the chief is on; there is at most one.
    Now,
    /// Waiting, at positions 1..n with no gaps.
    Parked,
    /// Finished; kept, never removed.
    Done,
}

impl QueueState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            QueueState::Now => "now",
            QueueState::Parked => "parked",
            QueueState::Done => "done",
        }
    }

    pub(super) fn parse(state: &str) -> Option<Self> {
        match state {
            "now" => Some(QueueState::Now),
            "parked" => Some(QueueState::Parked),
            "done" => Some(QueueState::Done),
            _ => None,
        }
    }
}

/// One item in the chief's queue. `position` is 1..n for a parked item and 0
/// otherwise; `done_at_ms` is 0 until the item is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QueueItem {
    pub id: String,
    pub state: QueueState,
    pub position: u32,
    pub text: String,
    pub done_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Where a new queue item goes: last of the parked, Now, or parked at a
/// position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Place {
    End,
    Now,
    At(u32),
}

/// The queue as it is listed: the Now item, the parked by position, the done
/// oldest first, and the queue's note (`""` when there is none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QueueListing {
    pub now: Option<QueueItem>,
    pub parked: Vec<QueueItem>,
    pub done: Vec<QueueItem>,
    pub note: String,
}

fn under(text: &str, limit: usize) -> bool {
    text.chars().count() < limit
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A title is required, one line, and under `FIELD_UNDER`.
fn valid_title(title: &str) -> bool {
    !title.is_empty() && !title.contains(['\n', '\r']) && under(title, FIELD_UNDER)
}

/// Checks an item against the limits. The `Err` is the first field that fails.
pub(super) fn validate_item(item: &Item) -> Result<(), String> {
    if !valid_id(&item.id) {
        return Err("id".into());
    }
    if !valid_title(&item.title) {
        return Err("title".into());
    }
    if !SIZES.contains(&item.size.as_str()) {
        return Err("size".into());
    }
    let fields = [
        ("value", &item.value),
        ("risk", &item.risk),
        ("reason", &item.reason),
        ("paths", &item.paths),
        ("after", &item.after),
    ];
    if let Some((name, _)) = fields.iter().find(|(_, text)| !under(text, FIELD_UNDER)) {
        return Err((*name).into());
    }
    let evidence_ok = (1..=EVIDENCE_MAX).contains(&item.evidence.len())
        && item
            .evidence
            .iter()
            .all(|pointer| under(pointer, POINTER_UNDER));
    if !evidence_ok {
        return Err("evidence".into());
    }
    Ok(())
}

/// Checks a note's title and body. The `Err` is the field that fails.
pub(super) fn validate_note(title: &str, body: &str) -> Result<(), String> {
    if !valid_title(title) {
        return Err("title".into());
    }
    if !under(body, BODY_UNDER) {
        return Err("body".into());
    }
    Ok(())
}

/// Checks a queue item's text or the queue's note.
pub(super) fn validate_queue_text(text: &str) -> Result<(), String> {
    if !under(text, QUEUE_TEXT_UNDER) {
        return Err("text".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
