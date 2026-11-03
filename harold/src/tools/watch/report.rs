//! Typing a watch's lines. A watch has one `ReportSlot`: the newest report waits in it
//! until the pane takes it, a newer report replaces a waiting one, and the closing line
//! waits in its place when the watch ends.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::Instant;

use super::obs::{ReportObs, Why};
use crate::outbound::left_out::same_folder;
use crate::timed_prompts::InputBox;
use crate::tools::{Agents, ChiefPane, PaneTyping, Typing};

/// How long a closing line is tried before it is given up: the watch is over, and nobody
/// waits on it.
const CLOSING_LINE_PATIENCE: Duration = Duration::from_secs(3600);

/// Who a watch types to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    Chief,
    /// The agent running in this folder.
    Folder(String),
}

/// Finds the pane and makes the timed-prompt checks before asking the host to type.
pub(super) struct Typist {
    pub typing: Arc<dyn PaneTyping>,
    pub chief: Arc<dyn ChiefPane>,
    pub agents: Arc<dyn Agents>,
}

impl Typist {
    /// Tries once. `Ok` holds the pane typed into and whether Harold pressed `i` to put its
    /// Vim box in INSERT mode. `Err` holds the reason it waits, one of the timed prompts'
    /// reasons.
    pub(super) async fn try_type(
        &self,
        target: &Target,
        line: &str,
    ) -> Result<(String, bool), &'static str> {
        let agents = self.agents.agents();
        let pane = match target {
            Target::Chief => self.chief.chief_pane().ok_or("no_agent_in_folder")?,
            Target::Folder(folder) => {
                let mut in_folder = agents.iter().filter(|a| same_folder(&a.folder, folder));
                let first = in_folder.next().ok_or("no_agent_in_folder")?;
                if in_folder.next().is_some() {
                    return Err("several_agents_in_folder");
                }
                first.pane_id.clone()
            }
        };
        let agent = agents
            .iter()
            .find(|agent| agent.pane_id == pane)
            .ok_or("no_agent_in_folder")?;
        if !agent.reads_as_claude {
            return Err("input_box_unreadable");
        }
        if agent.state.is_working() {
            return Err("agent_busy");
        }
        if !agent.state.is_at_rest() {
            return Err("agent_state_unknown");
        }
        match self.agents.input_box(&pane).await {
            // A box in Vim NORMAL mode is ready: the typing step presses `i`.
            Some(InputBox::Empty | InputBox::NotInsert) => {}
            Some(InputBox::HoldsText) => return Err("input_box_holds_text"),
            Some(InputBox::NotFound) | None => return Err("input_box_not_found"),
        }
        match self.typing.type_line(&pane, line).await {
            // Enter failing leaves the text in the box: typing again would repeat it.
            Typing::Typed { insert_pressed } => Ok((pane, insert_pressed)),
            Typing::EnterFailed => Ok((pane, false)),
            Typing::PaneBusy(reason) => Err(reason),
            Typing::NoPane => Err("no_agent_in_folder"),
        }
    }
}

struct Waiting {
    line: String,
    obs: ReportObs,
    generation: u64,
    /// A closing line gives up at this time; a report waits as long as its watch runs.
    give_up_at: Option<Instant>,
    /// Told `true` when the line is typed and `false` when it is given up. A line that
    /// is dropped drops it, with no value.
    done: Option<oneshot::Sender<bool>>,
}

#[derive(Default)]
struct Inner {
    waiting: Option<Waiting>,
    generation: u64,
    delivering: bool,
    /// No further report is taken: the watch is over.
    closed: bool,
}

pub(super) struct ReportSlot {
    inner: Mutex<Inner>,
    typist: Arc<Typist>,
    target: Target,
    retry: Duration,
}

impl ReportSlot {
    pub(super) fn new(typist: Arc<Typist>, target: Target, retry: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::default(),
            typist,
            target,
            retry,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Offers a run's report. It replaces a waiting one, which is never typed.
    pub(super) fn offer(self: &Arc<Self>, line: String, obs: ReportObs) {
        self.put(line, obs, None, false, None);
    }

    /// One last line, tried until `give_up_at`, as for a reminder. The slot takes nothing
    /// after it. The answer is `true` when the line is typed and `false` when it is
    /// given up. It has no value when the line is dropped, for example by `close(None)`.
    pub(super) fn last_line(
        self: &Arc<Self>,
        line: String,
        obs: ReportObs,
        give_up_at: Instant,
    ) -> oneshot::Receiver<bool> {
        let (done, answer) = oneshot::channel();
        self.put(line, obs, Some(give_up_at), true, Some(done));
        answer
    }

    /// The watch is over. A waiting report is dropped. With a closing line, that line
    /// waits in its place; without one, nothing more is typed.
    pub(super) fn close(self: &Arc<Self>, closing: Option<(String, ReportObs)>) {
        let give_up_at = Some(Instant::now() + CLOSING_LINE_PATIENCE);
        match closing {
            Some((line, obs)) => self.put(line, obs, give_up_at, true, None),
            None => {
                let mut inner = self.lock();
                inner.closed = true;
                if let Some(old) = inner.waiting.take() {
                    old.obs.left(Why::WatchEnded);
                }
            }
        }
    }

    fn put(
        self: &Arc<Self>,
        line: String,
        obs: ReportObs,
        give_up_at: Option<Instant>,
        closing: bool,
        done: Option<oneshot::Sender<bool>>,
    ) {
        let mut inner = self.lock();
        if inner.closed {
            return;
        }
        if let Some(old) = inner.waiting.take() {
            old.obs.left(if closing {
                Why::WatchEnded
            } else {
                Why::NewerReport
            });
        }
        inner.closed = closing;
        inner.generation += 1;
        let generation = inner.generation;
        inner.waiting = Some(Waiting {
            line,
            obs,
            generation,
            give_up_at,
            done,
        });
        if !inner.delivering {
            inner.delivering = true;
            tokio::spawn(Arc::clone(self).deliver());
        }
    }

    /// Tries the waiting line every `retry` until it is typed, replaced or given up.
    async fn deliver(self: Arc<Self>) {
        loop {
            let (line, generation) = {
                let mut inner = self.lock();
                match &inner.waiting {
                    Some(waiting) => (waiting.line.clone(), waiting.generation),
                    None => {
                        inner.delivering = false;
                        return;
                    }
                }
            };
            let tried = self.typist.try_type(&self.target, &line).await;
            {
                let mut inner = self.lock();
                let current = inner
                    .waiting
                    .as_ref()
                    .is_some_and(|waiting| waiting.generation == generation);
                match (tried, current) {
                    (Ok((pane, insert_pressed)), true) => {
                        if let Some(waiting) = inner.waiting.take() {
                            waiting.obs.typed(&pane, insert_pressed);
                            if let Some(done) = waiting.done {
                                let _ = done.send(true);
                            }
                        }
                        continue;
                    }
                    // Typed, but a newer report took the place meanwhile: that one is next.
                    (Ok(_), false) => continue,
                    (Err(reason), true) => {
                        let expired = inner.waiting.as_mut().is_some_and(|waiting| {
                            waiting.obs.waiting(reason);
                            waiting.give_up_at.is_some_and(|at| Instant::now() >= at)
                        });
                        if expired {
                            if let Some(waiting) = inner.waiting.take() {
                                waiting.obs.left(Why::WatchEnded);
                                if let Some(done) = waiting.done {
                                    let _ = done.send(false);
                                }
                            }
                            inner.delivering = false;
                            return;
                        }
                    }
                    (Err(_), false) => continue,
                }
            }
            tokio::time::sleep(self.retry).await;
        }
    }
}
