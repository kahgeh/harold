//! Helpers shared by the work tool's tests.

use std::path::PathBuf;

use super::schema::{Decision, Mark, Priority, Record, Status};

/// A folder under the system temp folder, removed when dropped.
pub(super) struct TempFolder(pub PathBuf);

impl TempFolder {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-work-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    /// The path of a work file of the given name in this folder.
    pub(super) fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A new record: priority `later`, status `idea`, no decision, no link. The
/// store sets `created_in` and the times.
pub(super) fn record(name: &str) -> Record {
    Record {
        name: name.into(),
        title: format!("Title {name}"),
        priority: Priority::Later,
        status: Status::Idea,
        waits_for: String::new(),
        next_step: String::new(),
        created_in: String::new(),
        created_at_ms: 0,
        updated_at_ms: 0,
        status_since_ms: 0,
        decisions: Vec::new(),
        links: Vec::new(),
    }
}

pub(super) fn decision(text: &str) -> Decision {
    Decision {
        at: "2026-11-03".into(),
        who: "the chief".into(),
        mark: Mark::Draft,
        text: text.into(),
    }
}

/// A clock that moves only when a test sets it.
pub(super) struct SetClock(std::sync::atomic::AtomicI64);

impl SetClock {
    pub(super) fn at(ms: i64) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(std::sync::atomic::AtomicI64::new(ms)))
    }

    pub(super) fn set(&self, ms: i64) {
        self.0.store(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl super::rotation::Clock for SetClock {
    fn now_ms(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Noon UTC of `YYYY-MM-DD`, in milliseconds.
pub(super) fn noon_ms(day: &str) -> i64 {
    let mut parts = day.split('-').map(|part| part.parse::<i64>().unwrap());
    let (year, month, date) = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    let month = time::Month::try_from(u8::try_from(month).unwrap()).unwrap();
    let date = time::Date::from_calendar_date(
        i32::try_from(year).unwrap(),
        month,
        u8::try_from(date).unwrap(),
    )
    .unwrap();
    date.with_hms(12, 0, 0)
        .unwrap()
        .assume_utc()
        .unix_timestamp()
        * 1000
}

/// One hour in milliseconds.
pub(super) const HOUR_MS: i64 = 3_600_000;
