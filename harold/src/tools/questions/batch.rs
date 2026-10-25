//! One batch back to the chief: the line, and the task that types it.
//!
//! Questions still waiting do not hold a line back, and neither do waiting
//! instructions; answered instructions go in the line as `done` or `ignored`.
//! When some answers are unsent, the sender waits out the settle (3 s from the newest unsent answer's
//! own stamp in the store), or does not wait when an unsent answer is marked
//! "send now". Then it records a batch as started, with its text, and makes
//! one try at typing it into the chief's pane. Typed, it is marked typed. Not
//! typed (the pane busy or gone, or no chief), a batch built in this run is
//! released, so the next round, 5 s later, builds it again from what is there
//! then; the answers keep their stamps, so the retry adds no second settle. A
//! wake from another write ends the sleep early, and the next round asks the
//! store again, so it never restarts the settle. A started batch never marked
//! typed, found when the sender starts, was being typed when Harold stopped
//! and may have reached the pane. It is kept, never released: it waits for the
//! pane like any batch and is typed again once, before any new batch.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tracing::{info, warn};

use super::schema::{Answer, Kind, Question};
use super::store::{QuestionStore, Readiness, Ready, StartedBatch, StoreError};
use super::{NAME, now_ms};
use crate::telemetry;
use crate::tools::{ChiefPane, PaneTyping, Typing};

/// How long the newest unsent answer settles before its line starts.
pub(super) const SETTLE: Duration = Duration::from_secs(3);

/// The clock the sender reads: wall-clock milliseconds, as the store stamps
/// answers.
#[cfg(not(test))]
pub(super) fn clock_ms() -> i64 {
    now_ms()
}

/// Under test the wall clock is read once per test thread and tokio's clock
/// moves it, so paused time ages an answer the way real time does.
#[cfg(test)]
pub(super) fn clock_ms() -> i64 {
    thread_local! {
        static BASE: (i64, tokio::time::Instant) = (now_ms(), tokio::time::Instant::now());
    }
    BASE.with(|(wall, start)| {
        wall + tokio::time::Instant::now()
            .duration_since(*start)
            .as_millis() as i64
    })
}

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

/// `ANSWERS <n> | <answer> | ...`, every field cleaned.
pub(super) fn batch_line(ready: &Ready) -> String {
    let mut line = format!("ANSWERS {}", ready.answers.len());
    for (id, _, question, answer) in &ready.answers {
        line.push_str(" | ");
        line.push_str(&answer_part(id, question, answer));
    }
    line
}

fn answer_part(id: &str, question: &Question, answer: &Answer) -> String {
    let id = clean(id);
    let spec = clean(&question.spec);
    match answer.kind {
        Kind::Done => {
            let product = clean(&question.product);
            let steps = question.steps.len();
            if (answer.ticked as usize) < steps {
                format!("{id} {product}: done, {} of {steps} ticked", answer.ticked)
            } else {
                format!("{id} {product}: done")
            }
        }
        Kind::Ignore => format!("{id} {}: ignored", clean(&question.product)),
        Kind::Revise => format!("{id} {spec}: revise \"{}\"", clean(&answer.note)),
        Kind::Reject => format!("{id} {spec}: reject \"{}\"", clean(&answer.note)),
        Kind::Relegate => {
            let note = clean(&answer.note);
            let note = note.trim();
            if note.is_empty() {
                format!("{id} {spec}: relegate to idea")
            } else {
                format!("{id} {spec}: relegate to idea \"{note}\"")
            }
        }
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
    /// Answers are unsent and settling: ask again when the wait is over, or
    /// sooner on a write.
    Settle(Duration),
    /// A batch was just typed, or could not start because the answers changed:
    /// look again at once.
    Again,
}

/// What the sender found to type.
enum Found {
    /// A batch built in this run: released if the try types nothing.
    Batch(StartedBatch),
    /// The started batch found when the sender started, left from before a
    /// restart: it may have reached the pane, so it is kept and typed once.
    Inherited(StartedBatch),
    Nothing,
    /// Unsent answers are still settling, for this much longer.
    Settling(Duration),
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
    /// Whether the store was asked, since the sender started, for a started
    /// batch left from before.
    looked_for_leftover: bool,
    /// The started batch left from before, until it is typed.
    inherited: Option<StartedBatch>,
    /// The reason last logged in this spell of waiting. A spell ends when a
    /// batch is typed or nothing is left to send.
    waiting_logged: Option<&'static str>,
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
        looked_for_leftover: false,
        inherited: None,
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
                _ = tokio::time::sleep(IDLE_RECHECK) => {}
            },
            Next::Retry => tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                _ = wake.notified() => {}
                _ = tokio::time::sleep(RETRY) => {}
            },
            Next::Settle(wait) => tokio::select! {
                biased;
                _ = shutdown.changed() => return,
                _ = wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
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
            if self.store.release_batch(batch_id).await.is_err() {
                self.log_waiting("store_failed");
                return Next::Retry;
            }
            self.unreleased = None;
        }
        let (batch, inherited) = match self.next_batch().await {
            Ok(Found::Batch(batch)) => (batch, false),
            Ok(Found::Inherited(batch)) => (batch, true),
            Ok(Found::Nothing) => {
                self.waiting_logged = None;
                return Next::Idle;
            }
            Ok(Found::Settling(wait)) => return Next::Settle(wait),
            // A write landed after `ready`: build the batch again from what is there now.
            Ok(Found::Changed) => return Next::Again,
            Err(_) => {
                self.log_waiting("store_failed");
                return Next::Retry;
            }
        };
        // The chief is looked up every round, and the line goes to the pane found now.
        let Some(pane) = self.chief.chief_pane() else {
            return self.not_typed(batch.id, inherited, "no_chief").await;
        };
        // One try: a busy pane is not waited for here, but on the next round.
        // `Typed` and `EnterFailed` mean the line may be in the pane, so the batch
        // is never released after either. A stop between typing and the mark is
        // typed again once at the next start; a failed mark is retried without
        // typing again.
        let typing_from = time::OffsetDateTime::now_utc();
        match self.typing.type_line(&pane, &batch.text).await {
            how @ (Typing::Typed | Typing::EnterFailed) => {
                // The chief's turn for this line joins the trace of the answer that came
                // with one; a line of answers from the dash is a root.
                telemetry::answers_typed(&pane, &batch.traces, typing_from);
                // The line reached the pane, so this spell of waiting is over.
                self.waiting_logged = None;
                self.inherited = None;
                self.typed = Some((batch.id, how));
                self.mark_typed(batch.id, how).await
            }
            how @ (Typing::PaneBusy | Typing::NoPane) => {
                self.not_typed(batch.id, inherited, how.reason()).await
            }
        }
    }

    /// The started batch left from before, else a started batch, else a new
    /// one when answers are ready.
    async fn next_batch(&mut self) -> Result<Found, StoreError> {
        if let Some(batch) = &self.inherited {
            return Ok(Found::Inherited(batch.clone()));
        }
        if let Some(batch) = self.store.started_batch().await? {
            if self.looked_for_leftover {
                return Ok(Found::Batch(batch));
            }
            // Found at the first look since the sender started: from before a restart.
            self.looked_for_leftover = true;
            self.inherited = Some(batch.clone());
            return Ok(Found::Inherited(batch));
        }
        self.looked_for_leftover = true;
        let ready = match self
            .store
            .ready(clock_ms(), SETTLE.as_millis() as i64)
            .await?
        {
            Readiness::Nothing => return Ok(Found::Nothing),
            Readiness::Settling { wait_ms } => {
                return Ok(Found::Settling(
                    Duration::from_millis(wait_ms.max(0) as u64),
                ));
            }
            Readiness::Ready(ready) => ready,
        };
        let answers: Vec<(String, u32)> = ready
            .answers
            .iter()
            .map(|(id, version, ..)| (id.clone(), *version))
            .collect();
        let started = self
            .store
            .start_batch(&answers, &batch_line(&ready), clock_ms())
            .await?;
        Ok(started.map_or(Found::Changed, |started| {
            Found::Batch(StartedBatch {
                traces: ready.traces,
                ..started
            })
        }))
    }

    /// Nothing was typed. A batch built in this run is released: the next
    /// round builds it again, with any answer given meanwhile. The
    /// batch left from before a restart may have reached the pane, so it is
    /// kept and tried again.
    async fn not_typed(&mut self, batch_id: i64, inherited: bool, reason: &'static str) -> Next {
        self.log_waiting(reason);
        if inherited {
            return Next::Retry;
        }
        if self.store.release_batch(batch_id).await.is_err() {
            self.log_waiting("store_failed");
            self.unreleased = Some(batch_id);
        }
        Next::Retry
    }

    async fn mark_typed(&mut self, batch_id: i64, how: Typing) -> Next {
        if self.store.batch_typed(batch_id, clock_ms()).await.is_err() {
            self.log_waiting("store_failed");
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

    /// Logs a reason when it differs from the one last logged in this spell
    /// of waiting, so a spell gives one line per change of reason.
    fn log_waiting(&mut self, reason: &'static str) {
        if self.waiting_logged == Some(reason) {
            return;
        }
        self.waiting_logged = Some(reason);
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
