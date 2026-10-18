//! One batch back to the chief: the line, and the task that types it.
//!
//! When no question is waiting and some answers are unsent, the sender
//! records a batch as started, with its text, and makes one try at typing it
//! into the chief's pane. Typed, it is marked typed. Not typed (the pane busy
//! or gone, or no chief), it is released, so the next round, 5 s later, builds
//! it again from what is there then and a question put meanwhile holds it. A
//! started batch never marked typed, from before a restart, was being typed
//! when Harold stopped; it is typed again first, once.

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

/// How long an idle sender waits before it looks again without a wake: a write
/// whose call was cancelled after it committed wakes nobody.
pub(super) const IDLE_RECHECK: Duration = Duration::from_secs(30);

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
    /// Nothing to send: wait for a write, or `IDLE_RECHECK`.
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
    /// A batch not typed whose release failed; it is released before anything else.
    unreleased: Option<i64>,
    /// Whether this spell of waiting was logged. A spell ends when a batch is
    /// typed or nothing is left to send.
    waiting_logged: bool,
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
        unreleased: None,
        waiting_logged: false,
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
                _ = tokio::time::sleep(IDLE_RECHECK) => {}
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
        if let Some(batch_id) = self.unreleased {
            if self.store.release_batch(batch_id, now_ms()).await.is_err() {
                self.log_waiting("store_failed");
                return Next::Retry;
            }
            self.unreleased = None;
        }
        let batch = match self.next_batch().await {
            Ok(Found::Batch(batch)) => batch,
            Ok(Found::Nothing) => {
                self.waiting_logged = false;
                return Next::Idle;
            }
            // A write landed after `ready`: build the batch again from what is there now.
            Ok(Found::Changed) => return Next::Again,
            Err(_) => {
                self.log_waiting("store_failed");
                return Next::Retry;
            }
        };
        // The chief is looked up every round, and the line goes to the pane found now.
        let Some(pane) = self.chief.chief_pane() else {
            return self.release(batch.id, "no_chief").await;
        };
        // One try: a busy pane is not waited for here, but on the next round.
        match self.typing.type_line(&pane, &batch.text).await {
            how @ (Typing::Typed | Typing::EnterFailed) => {
                // The line reached the pane, so this spell of waiting is over.
                self.waiting_logged = false;
                self.typed = Some((batch.id, how));
                self.mark_typed(batch.id, how).await
            }
            how @ (Typing::PaneBusy | Typing::NoPane) => self.release(batch.id, how.reason()).await,
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

    /// Nothing was typed, so the batch is released: the next round builds it
    /// again, and a question put meanwhile holds it.
    async fn release(&mut self, batch_id: i64, reason: &'static str) -> Next {
        self.log_waiting(reason);
        if self.store.release_batch(batch_id, now_ms()).await.is_err() {
            self.unreleased = Some(batch_id);
        }
        Next::Retry
    }

    async fn mark_typed(&mut self, batch_id: i64, how: Typing) -> Next {
        if self.store.batch_typed(batch_id, now_ms()).await.is_err() {
            self.log_waiting("store_failed");
            return Next::Retry;
        }
        self.typed = None;
        self.waiting_logged = false;
        match how {
            Typing::EnterFailed => {
                warn!(tool = NAME, reason = how.reason(), "answers batch typed")
            }
            _ => info!(tool = NAME, reason = how.reason(), "answers batch typed"),
        }
        Next::Again
    }

    /// Logs the first reason of a spell of waiting only.
    fn log_waiting(&mut self, reason: &str) {
        if self.waiting_logged {
            return;
        }
        self.waiting_logged = true;
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
