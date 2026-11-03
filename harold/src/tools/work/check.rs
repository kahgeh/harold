//! The work check: it compares each work record with the facts outside it.
//! `check` is a pure function over a `Facts` port.

use super::schema::{Mark, Record, Status};

/// What the pod files say about one spec.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SpecFacts {
    pub(super) exists: bool,
    pub(super) merged: bool,
    pub(super) withdrawn: bool,
    pub(super) approved: bool,
    /// False when the registry or the product folder cannot be read.
    pub(super) registry_ok: bool,
}

impl SpecFacts {
    /// The spec has a file in `specs/`, `merged/`, `withdrawn/` or `approvals/`.
    fn present(&self) -> bool {
        self.exists || self.merged || self.withdrawn || self.approved
    }
}

/// What the question tool says about one question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuestionFact {
    Waiting,
    Answered,
    /// The question does not exist.
    Missing,
    /// The question tool cannot be read. This gives no line.
    Unreadable,
}

pub(super) trait Facts: Send + Sync {
    fn spec(&self, product: &str, id: &str) -> SpecFacts;
    fn question(&self, id: &str) -> QuestionFact;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mismatch {
    pub(super) record: String,
    pub(super) kind: &'static str,
    /// One short sentence that starts with the record name.
    pub(super) line: String,
}

/// Returns the mismatches, record by record, each in the order of the spec.
pub(super) fn check(records: &[Record], facts: &dyn Facts) -> Vec<Mismatch> {
    let mut found = Vec::new();
    for record in records {
        check_record(record, facts, &mut found);
    }
    found
}

fn check_record(record: &Record, facts: &dyn Facts, found: &mut Vec<Mismatch>) {
    let name = record.name.as_str();
    let mut add = |kind: &'static str, text: String| {
        found.push(Mismatch {
            record: name.to_string(),
            kind,
            line: format!("{name} {text}"),
        });
    };
    for link in &record.links {
        if let Some(rest) = link.strip_prefix("spec:")
            && !rest.contains('/')
        {
            add(
                "missing_link",
                format!("links {link}, which names no spec."),
            );
        }
    }
    let specs: Vec<(&str, &str, SpecFacts)> = record
        .links
        .iter()
        .filter_map(|link| link.strip_prefix("spec:"))
        .filter_map(|rest| rest.split_once('/'))
        .map(|(product, id)| (product, id, facts.spec(product, id)))
        .collect();
    let questions: Vec<(&str, QuestionFact)> = record
        .links
        .iter()
        .filter_map(|link| link.strip_prefix("question:"))
        .map(|id| (id, facts.question(id)))
        .collect();

    if record.status.ordinal() >= Status::Approved.ordinal() {
        for (id, fact) in &questions {
            if *fact == QuestionFact::Waiting {
                add(
                    "waiting_question",
                    format!(
                        "is {} but question {id} still waits.",
                        record.status.as_str()
                    ),
                );
            }
        }
    }
    if record.status.ordinal() < Status::Merged.ordinal() {
        for (product, id, spec) in &specs {
            if spec.present() && spec.merged {
                add(
                    "merged_status_behind",
                    format!(
                        "is {} but spec {product}/{id} is merged.",
                        record.status.as_str()
                    ),
                );
            }
        }
    }
    if record.status.is_open() {
        for (product, id, spec) in &specs {
            if spec.present() && spec.withdrawn {
                add(
                    "withdrawn_open",
                    format!("is open but spec {product}/{id} is withdrawn."),
                );
            }
        }
    }
    if record.status.ordinal() < Status::Approved.ordinal() {
        for (product, id, spec) in &specs {
            if spec.present() && spec.approved {
                add(
                    "approved_status_behind",
                    format!(
                        "is {} but spec {product}/{id} is approved.",
                        record.status.as_str()
                    ),
                );
            }
        }
    }
    if !record.status.is_open() {
        for (number, decision) in record.decisions.iter().enumerate() {
            if decision.mark == Mark::Open {
                add(
                    "closed_open_decision",
                    format!("is closed but decision {} is still open.", number + 1),
                );
            }
        }
    }
    for (product, id, spec) in &specs {
        if spec.registry_ok && !spec.present() {
            add(
                "missing_link",
                format!("links spec {product}/{id}, which does not exist."),
            );
        }
    }
    for (id, fact) in &questions {
        if *fact == QuestionFact::Missing {
            add(
                "missing_link",
                format!("links question {id}, which does not exist."),
            );
        }
    }
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;
