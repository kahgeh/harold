//! The work record and its parts.

/// The longest name, in characters.
pub(super) const NAME_MAX: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Priority {
    Focus,
    Next,
    Later,
}

impl Priority {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Priority::Focus => "focus",
            Priority::Next => "next",
            Priority::Later => "later",
        }
    }

    pub(super) fn parse(word: &str) -> Option<Self> {
        match word {
            "focus" => Some(Priority::Focus),
            "next" => Some(Priority::Next),
            "later" => Some(Priority::Later),
            _ => None,
        }
    }

    /// 0 for `focus`, so a sort by it lists focus first.
    // Only the tests use it so far.
    #[allow(dead_code)]
    pub(super) fn ordinal(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Idea,
    Discovery,
    SpecDrafted,
    ArchitectChecked,
    WaitingForTheUser,
    Approved,
    Building,
    Merged,
    Released,
    CheckedByTheUser,
}

const STATUSES: [(Status, &str); 10] = [
    (Status::Idea, "idea"),
    (Status::Discovery, "discovery"),
    (Status::SpecDrafted, "spec drafted"),
    (Status::ArchitectChecked, "architect checked"),
    (Status::WaitingForTheUser, "waiting for the user"),
    (Status::Approved, "approved"),
    (Status::Building, "building"),
    (Status::Merged, "merged"),
    (Status::Released, "released"),
    (Status::CheckedByTheUser, "checked by the user"),
];

impl Status {
    /// The place in the order from `idea` (0) to `checked by the user` (9).
    pub(super) fn ordinal(self) -> u8 {
        self as u8
    }

    /// A work record is open before `checked by the user`.
    pub(super) fn is_open(self) -> bool {
        self != Status::CheckedByTheUser
    }

    pub(super) fn as_str(self) -> &'static str {
        STATUSES[self as usize].1
    }

    pub(super) fn parse(words: &str) -> Option<Self> {
        STATUSES
            .iter()
            .find(|(_, name)| *name == words)
            .map(|(status, _)| *status)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    Agreed,
    Draft,
    Dropped,
    Open,
}

impl Mark {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Mark::Agreed => "agreed",
            Mark::Draft => "draft",
            Mark::Dropped => "dropped",
            Mark::Open => "open",
        }
    }

    pub(super) fn parse(word: &str) -> Option<Self> {
        match word {
            "agreed" => Some(Mark::Agreed),
            "draft" => Some(Mark::Draft),
            "dropped" => Some(Mark::Dropped),
            "open" => Some(Mark::Open),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Decision {
    /// The date of the decision.
    pub(super) at: String,
    /// `the user`, `the chief` or a session name.
    pub(super) who: String,
    pub(super) mark: Mark,
    pub(super) text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Record {
    pub(super) name: String,
    pub(super) title: String,
    pub(super) priority: Priority,
    pub(super) status: Status,
    pub(super) waits_for: String,
    pub(super) next_step: String,
    /// The name of the work file that first held the record.
    pub(super) created_in: String,
    pub(super) created_at_ms: i64,
    pub(super) updated_at_ms: i64,
    /// When the status last changed. A rotation keeps it.
    pub(super) status_since_ms: i64,
    pub(super) decisions: Vec<Decision>,
    pub(super) links: Vec<String>,
}

/// A name has 1 to 48 characters of `a-z`, `0-9` and `-`.
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A link is `spec:<product>/<id>`, `question:<id>` or `note:<text>`.
pub(super) fn valid_link(link: &str) -> bool {
    if link.contains(['\n', '\r']) {
        return false;
    }
    let plain = |part: &str| !part.is_empty() && !part.contains(char::is_whitespace);
    if let Some(rest) = link.strip_prefix("spec:") {
        return match rest.split_once('/') {
            Some((product, id)) => plain(product) && plain(id) && !id.contains('/'),
            None => false,
        };
    }
    if let Some(rest) = link.strip_prefix("question:") {
        return plain(rest);
    }
    if let Some(rest) = link.strip_prefix("note:") {
        return !rest.trim().is_empty();
    }
    false
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
