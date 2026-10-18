//! One batch back to the chief: the line, and the task that types it.
//!
//! When no question is waiting and some answers are unsent, the sender
//! records a batch as started, with its text, then types it into the chief's
//! pane and marks it typed. A started batch never marked typed, from before
//! a restart, is typed again first, once.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tracing::{info, warn};

use super::schema::{Answer, Kind, Question};
use super::store::{QuestionStore, Ready, StartedBatch, StoreError};
use super::{NAME, now_ms};
use crate::tools::{ChiefPane, PaneTyping, Typing};

/// How long a batch waits before the next try.
pub(super) const RETRY: Duration = Duration::from_secs(5);

const ESC: char = '\u{1b}';

/// Makes a field safe to type on one line: `\r\n` and `\n` become a space,
/// escape sequences (`ESC`, and for `ESC [` everything up to and including
/// its final byte in `0x40..=0x7E`) and every other control character, a
/// lone `\r` included, are removed.
pub(super) fn clean(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => out.push(' '),
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                out.push(' ');
            }
            ESC if chars.peek() == Some(&'[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// `ANSWERS <n> | <answer> | ... | later: <id>, <id>`, every field cleaned.
pub(super) fn batch_line(ready: &Ready) -> String {
    let mut line = format!("ANSWERS {}", ready.answers.len());
    for (id, _, question, answer) in &ready.answers {
        line.push_str(" | ");
        line.push_str(&answer_part(id, question, answer));
    }
    if !ready.later.is_empty() {
        let later: Vec<String> = ready.later.iter().map(|id| clean(id)).collect();
        line.push_str(" | later: ");
        line.push_str(&later.join(", "));
    }
    line
}

fn answer_part(id: &str, question: &Question, answer: &Answer) -> String {
    let id = clean(id);
    let spec = clean(&question.spec);
    match answer.kind {
        Kind::Revise => format!("{id} {spec}: revise \"{}\"", clean(&answer.note)),
        Kind::Reject => format!("{id} {spec}: reject \"{}\"", clean(&answer.note)),
        Kind::Answer | Kind::FreeText => {
            let picks: Vec<String> = answer
                .picks
                .iter()
                .map(|pick| {
                    let ask = clean(&pick.ask);
                    let label = clean(&pick.label);
                    if pick.free {
                        format!("{ask} \"{label}\"")
                    } else {
                        format!("{ask} {label}")
                    }
                })
                .collect();
            format!(
                "{id} {spec} sha {}: {}",
                clean(&question.sha),
                picks.join("; ")
            )
        }
    }
}

/// What the sender does after one round.
enum Next {
    /// Nothing to send: wait for a write.
    Idle,
    /// A batch is pending: try again after `RETRY`, or sooner on a write.
    Retry,
    /// A batch was just typed, or could not start because the answers changed:
    /// look again at once.
    Again,
}

/// What the sender found to type.
enum Found {
    Batch(StartedBatch),
    Nothing,
    /// The answers changed between `ready` and `start_batch`; nothing was started.
    Changed,
}

struct Sender<S> {
    store: Arc<S>,
    typing: Arc<dyn PaneTyping>,
    chief: Arc<dyn ChiefPane>,
    /// A batch typed but not yet marked typed; it is marked, never typed again.
    typed: Option<(i64, Typing)>,
    /// The batch whose wait was logged (`None` when no batch could be read).
    waiting_logged: Option<Option<i64>>,
}

/// Sends batches until shutdown, which also cancels a line being typed.
pub(super) async fn run<S: QuestionStore>(
    store: Arc<S>,
    typing: Arc<dyn PaneTyping>,
    chief: Arc<dyn ChiefPane>,
    wake: Arc<Notify>,
    mut shutdown: watch::Receiver<()>,
) {
    let mut sender = Sender {
        store,
        typing,
        chief,
        typed: None,
        waiting_logged: None,
    };
    loop {
        let next = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            next = sender.round() => next,
        };
        match next {
            Next::Again => {}
            Next::Idle => tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                _ = wake.notified() => {}
            },
            Next::Retry => tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                _ = wake.notified() => {}
                _ = tokio::time::sleep(RETRY) => {}
            },
        }
    }
}

impl<S: QuestionStore> Sender<S> {
    async fn round(&mut self) -> Next {
        if let Some((batch_id, how)) = self.typed {
            return self.mark_typed(batch_id, how).await;
        }
        let batch = match self.next_batch().await {
            Ok(Found::Batch(batch)) => batch,
            Ok(Found::Nothing) => {
                self.waiting_logged = None;
                return Next::Idle;
            }
            // A write landed after `ready`: build the batch again from what is there now.
            Ok(Found::Changed) => return Next::Again,
            Err(_) => {
                self.log_waiting(None, "store_failed");
                return Next::Retry;
            }
        };
        let Some(pane) = self.chief.chief_pane() else {
            self.log_waiting(Some(batch.id), "no_chief");
            return Next::Retry;
        };
        match self.typing.type_line(&pane, &batch.text).await {
            how @ (Typing::Typed | Typing::EnterFailed) => {
                self.typed = Some((batch.id, how));
                self.mark_typed(batch.id, how).await
            }
            how @ (Typing::PaneBusy | Typing::NoPane) => {
                self.log_waiting(Some(batch.id), how.reason());
                Next::Retry
            }
        }
    }

    /// The started batch from before, else a new one when answers are ready.
    async fn next_batch(&self) -> Result<Found, StoreError> {
        if let Some(batch) = self.store.started_batch().await? {
            return Ok(Found::Batch(batch));
        }
        let Some(ready) = self.store.ready().await? else {
            return Ok(Found::Nothing);
        };
        let answers: Vec<(String, u32)> = ready
            .answers
            .iter()
            .map(|(id, version, ..)| (id.clone(), *version))
            .collect();
        let started = self
            .store
            .start_batch(&answers, &batch_line(&ready), now_ms())
            .await?;
        Ok(started.map_or(Found::Changed, Found::Batch))
    }

    async fn mark_typed(&mut self, batch_id: i64, how: Typing) -> Next {
        if self.store.batch_typed(batch_id, now_ms()).await.is_err() {
            self.log_waiting(Some(batch_id), "store_failed");
            return Next::Retry;
        }
        self.typed = None;
        self.waiting_logged = None;
        match how {
            Typing::EnterFailed => {
                warn!(tool = NAME, reason = how.reason(), "answers batch typed")
            }
            _ => info!(tool = NAME, reason = how.reason(), "answers batch typed"),
        }
        Next::Again
    }

    /// Logs a batch's wait the first time only.
    fn log_waiting(&mut self, batch_id: Option<i64>, reason: &str) {
        if self.waiting_logged == Some(batch_id) {
            return;
        }
        self.waiting_logged = Some(batch_id);
        if reason == "store_failed" {
            warn!(tool = NAME, reason, "answers batch waiting");
        } else {
            info!(tool = NAME, reason, "answers batch waiting");
        }
    }
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
