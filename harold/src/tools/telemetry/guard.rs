//! Who may write a prefix, and how fast. The chief's pane writes `chief.` and any
//! product; a pane writes its own product's prefix only. Each prefix may write
//! `WRITES_PER_MINUTE` times in a sliding minute.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::name::{Refused, WRITES_PER_MINUTE};
use crate::tools::backlog::guard::{Guard as BacklogGuard, Refused as BacklogRefused};
use crate::tools::{ChiefPane, PaneFolder, Products};

const WINDOW_MS: i64 = 60_000;
/// The prefix of the chief's own signals.
pub(crate) const CHIEF: &str = "chief";
/// The text of a refusal when the product registry cannot be read.
pub(crate) const REGISTRY_UNREADABLE: &str = "the product registry cannot be read: try again later";

/// The time, in milliseconds. A fake stands in for it in tests.
pub(crate) trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The wall clock.
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}

pub(crate) struct Guard {
    inner: BacklogGuard,
    chief_pane: Arc<dyn ChiefPane>,
    clock: Arc<dyn Clock>,
    writes: Mutex<HashMap<String, VecDeque<i64>>>,
}

impl Guard {
    pub(crate) fn new(
        pane_folder: Arc<dyn PaneFolder>,
        products: Arc<dyn Products>,
        chief_pane: Arc<dyn ChiefPane>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: BacklogGuard::new(pane_folder, products, chief_pane.clone()),
            chief_pane,
            clock,
            writes: Mutex::new(HashMap::new()),
        }
    }

    /// The pane may write the prefix.
    pub(crate) fn check_prefix(&self, pane: &str, prefix: &str) -> Result<(), Refused> {
        if pane.is_empty() {
            return Err(Refused::NoPane);
        }
        let wrong = || Refused::WrongPrefix {
            prefix: prefix.into(),
        };
        if prefix == CHIEF {
            return match self.chief_pane.chief_pane().as_deref() == Some(pane) {
                true => Ok(()),
                false => Err(wrong()),
            };
        }
        self.inner
            .may_write_product(pane, prefix)
            .map_err(|refused| match refused {
                BacklogRefused::NoPane => Refused::NoPane,
                BacklogRefused::UnknownProduct => Refused::UnknownPrefix {
                    prefix: prefix.into(),
                },
                BacklogRefused::RegistryUnreadable => Refused::Invalid(REGISTRY_UNREADABLE),
                _ => wrong(),
            })
    }

    /// Counts one write for the prefix, or refuses when the last minute holds
    /// `WRITES_PER_MINUTE` writes already. A refused write is not counted.
    pub(crate) fn take_write(&self, prefix: &str) -> Result<(), Refused> {
        let now = self.clock.now_ms();
        let mut writes = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let times = writes.entry(prefix.to_string()).or_default();
        while times.front().is_some_and(|t| now - t >= WINDOW_MS) {
            times.pop_front();
        }
        if times.len() as u64 >= WRITES_PER_MINUTE {
            return Err(Refused::RateLimited {
                prefix: prefix.into(),
            });
        }
        times.push_back(now);
        Ok(())
    }

    /// The registry's product names, and no more when it cannot be read.
    pub(crate) fn registry_prefixes(&self) -> Vec<String> {
        self.inner
            .products()
            .map(|products| products.into_iter().map(|(name, _)| name).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
