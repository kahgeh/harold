//! The question rows' state machine: the keys, the picks and the notices.
//!
//! Pure and free of IO. The runtime maps Harold's question queue into [`Queue`], feeds keys
//! to [`QuestionBlock::handle_key`], does what the returned [`BlockEffect`] asks, and reports
//! the outcome of a send through [`QuestionBlock::sent`].

use std::collections::BTreeMap;

use crossterm::event::KeyCode;

/// One option of a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionView {
    pub key: String,
    pub label: String,
    pub impact: String,
    /// The option the chief or product manager suggests; its pick starts on it.
    pub suggested: bool,
    /// A free option asks for typed text instead of a fixed answer.
    pub free: bool,
}

/// One decision of a question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionView {
    pub ask: String,
    pub options: Vec<OptionView>,
}

/// A question as listed by Harold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionView {
    pub id: String,
    pub product: String,
    pub spec: String,
    pub sha: String,
    pub link: String,
    pub intro: String,
    pub version: u32,
    pub decisions: Vec<DecisionView>,
    /// Instructions only: the title shown in their rows.
    pub title: String,
    /// Instructions only: the numbered steps.
    pub steps: Vec<StepView>,
    /// Instructions only: the ticked step numbers, from 1.
    pub ticked: Vec<u32>,
    /// A question only: the picks Harold has saved.
    pub picks: Vec<PickView>,
}

/// One step of instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepView {
    pub text: String,
    pub detail: String,
}

impl QuestionView {
    /// Instructions hold steps; a question holds decisions.
    pub fn is_instructions(&self) -> bool {
        !self.steps.is_empty()
    }

    fn kind(&self) -> TodoKind {
        if self.is_instructions() {
            TodoKind::Instructions
        } else {
            TodoKind::Question
        }
    }

    /// How many steps are ticked, counting each listed step once.
    fn ticked_count(&self) -> usize {
        (1..=self.steps.len() as u32)
            .filter(|n| self.ticked.contains(n))
            .count()
    }
}

/// The two kinds of todo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoKind {
    Question,
    Instructions,
}

/// What `ListQuestions` returned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Queue {
    pub questions: Vec<QuestionView>,
    pub waiting: u32,
    /// Recently dismissed questions as (id, reason).
    pub dismissed: Vec<(String, String)>,
}

/// How an answer is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerKind {
    Answer,
    Revise,
    Reject,
    /// The question leaves the todos and the chief keeps it as an idea.
    Relegate,
    FreeText,
    /// Instructions are done.
    Done,
    /// Instructions are not wanted.
    Ignore,
}

/// One pick. `decision` counts from 0; `text` is only set for a free option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickView {
    pub decision: u32,
    pub key: String,
    pub text: String,
}

/// An answer for the runtime to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub id: String,
    pub version: u32,
    pub kind: AnswerKind,
    pub picks: Vec<PickView>,
    pub note: String,
    /// Harold's send-now mark. Every answer from the dash sets it, since `send` means now.
    pub send_now: bool,
}

/// What a note is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Revise,
    Reject,
    Relegate,
    FreeText,
}

/// What the block is doing: `Choosing` means no note is open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Choosing,
    Note {
        kind: NoteKind,
        text: String,
    },
}

/// A row the cursor can stop on within the question's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QStop {
    Header,
    Option {
        decision: usize,
        option: usize,
    },
    /// An [`ANSWER_ACTIONS`] row, or an [`INSTRUCTION_ACTIONS`] row on instructions.
    Action(usize),
    /// Step `n` (from 0) of the open instructions.
    Step(usize),
    /// A todo that is not the open one, as an index into the listed order
    /// (`Queue.questions`), so the open todo's own index is never used.
    Other(usize),
}

/// What the runtime does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockEffect {
    None,
    /// Move the cursor to this stop.
    Cursor(QStop),
    OpenLink(String),
    Send(Outgoing),
    /// Save the question's current picks, in decision order.
    SavePicks {
        id: String,
        version: u32,
        picks: Vec<PickView>,
    },
    /// Tick (`ticked` true) or untick step `step` (from 1) of the instructions.
    Tick {
        id: String,
        version: u32,
        step: u32,
        ticked: bool,
    },
}

/// The outcome of a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendResult {
    Recorded,
    VersionChanged,
    NotWaiting,
    Unavailable,
}

/// One option as the rows draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRows {
    pub key: String,
    pub label: String,
    pub impact: String,
    pub suggested: bool,
    pub free: bool,
    pub picked: bool,
    /// The text of a picked free option.
    pub free_text: Option<String>,
}

/// One decision as the rows draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRows {
    pub ask: String,
    pub options: Vec<OptionRows>,
}

/// One step as the rows draw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRows {
    pub text: String,
    pub detail: String,
    pub ticked: bool,
}

/// A todo other than the open one, drawn as one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherRow {
    /// Its place in the listed order: the `n` of its [`QStop::Other`].
    pub index: usize,
    pub kind: TodoKind,
    pub id: String,
    pub ticked_count: usize,
    pub steps_total: usize,
    pub product: String,
    /// The spec of a question, the title of instructions.
    pub label: String,
}

/// Everything the drawing needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowsView {
    pub kind: TodoKind,
    pub id: String,
    pub product: String,
    pub spec: String,
    pub title: String,
    pub intro: String,
    /// The current todo's place in the listed order, from 1.
    pub position: usize,
    /// The number of listed todos.
    pub waiting: u32,
    pub open: bool,
    pub decisions: Vec<DecisionRows>,
    pub steps: Vec<StepRows>,
    pub ticked_count: usize,
    /// Instructions with every step ticked.
    pub all_ticked: bool,
    /// The other todos in listed order.
    pub others: Vec<OtherRow>,
    pub mode: Mode,
    /// The row an open note belongs to, wherever the cursor is.
    pub note_stop: Option<QStop>,
    pub notice: Option<String>,
}

/// The picks of a question that is not in view, kept for its version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    version: u32,
    picks: Vec<Option<PickView>>,
}

/// The block's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuestionBlock {
    queue: Queue,
    current: Option<String>,
    version: u32,
    /// One optional pick per decision of the current question, in decision order.
    picks: Vec<Option<PickView>>,
    mode: Mode,
    /// The free option whose text field is open: (decision, option).
    free_target: Option<(usize, usize)>,
    /// Whether the question shows its rows. One flag for the block, not per question.
    open: bool,
    /// The picks of listed questions other than the current one, by id. One goes when
    /// its question is no longer listed or is listed at a higher version.
    kept: BTreeMap<String, Kept>,
    notice: Option<String>,
    /// Ids of instructions answered `done` or `ignore` here, so that the same id listed
    /// again says its ticks were kept. Kept small.
    answered_instructions: Vec<String>,
    /// Ids that went missing while shown here, so that a question listed again says its
    /// picks were kept or cleared. Kept small.
    gone: Vec<String>,
    /// The notice stood on a block with no question through one listing already; the next
    /// listing that still finds no question clears it, and the block hides.
    notice_listed: bool,
    /// Hidden after a failed read or a lost stream, with the picks, mode and note kept
    /// for the next listing.
    suspended: bool,
}

/// The notice when Harold could not save an answer.
pub const UNAVAILABLE_NOTICE: &str = "Harold can't save answers right now";

/// The rows under every decision's options. `send` sends the picks, and the other three
/// open a note to send with the picks. The note of `relegate to idea…` may be empty.
pub const ANSWER_ACTIONS: [&str; 4] = [
    "send",
    "relegate to idea\u{2026}",
    "revise\u{2026}",
    "reject\u{2026}",
];

/// The rows under instructions' steps.
pub const INSTRUCTION_ACTIONS: [&str; 2] = ["done", "ignore"];

/// The longest note the block takes; Harold refuses a field of 2,000 characters.
const NOTE_LIMIT: usize = 1_999;

/// The notice when Harold could not save a pick.
pub const PICKS_UNAVAILABLE_NOTICE: &str = "Harold can't save your picks right now";

fn picks_kept_notice(id: &str) -> String {
    format!("{id} changed, picks kept for unchanged decisions")
}

fn changed_notice(id: &str) -> String {
    format!("{id} changed, your picks were cleared")
}

fn instructions_changed_notice(id: &str) -> String {
    format!("{id} changed, ticks kept for unchanged steps")
}

fn is_web_link(link: &str) -> bool {
    link.starts_with("https://") || link.starts_with("http://")
}

/// Each decision's starting pick: the pick Harold has saved for it, when its key is still
/// an option (a free option needs its text); else its suggested option, unless that one is
/// free (it has no text yet); else none.
fn starting_picks(q: &QuestionView) -> Vec<Option<PickView>> {
    q.decisions
        .iter()
        .enumerate()
        .map(|(decision, d)| {
            let saved = q
                .picks
                .iter()
                .find(|p| p.decision as usize == decision)
                .and_then(|p| {
                    let option = d.options.iter().find(|o| o.key == p.key)?;
                    let text = if option.free {
                        p.text.clone()
                    } else {
                        String::new()
                    };
                    (!option.free || !text.is_empty()).then(|| PickView {
                        decision: decision as u32,
                        key: p.key.clone(),
                        text,
                    })
                });
            saved.or_else(|| {
                d.options
                    .iter()
                    .find(|o| o.suggested && !o.free)
                    .map(|o| PickView {
                        decision: decision as u32,
                        key: o.key.clone(),
                        text: String::new(),
                    })
            })
        })
        .collect()
}

impl QuestionBlock {
    /// Takes the latest listing. The current question stays while it is listed at the same
    /// version; a higher version restarts it; a missing one is replaced with a notice.
    /// Picks kept for other questions go when those are missing or at a higher version.
    /// A suspended block shows again, and keeps its state when its question is listed at
    /// the same version. A notice on a block with no question stays for one more listing;
    /// the one after that, still finding no question, clears it, so the block hides.
    pub fn reconcile(&mut self, queue: Queue) {
        self.suspended = false;
        self.queue = queue;
        let listed = &self.queue.questions;
        self.kept.retain(|id, kept| {
            listed
                .iter()
                .any(|q| &q.id == id && q.version <= kept.version)
        });
        self.follow_current();
        self.age_notice();
        self.note_put_again();
    }

    /// Instructions answered here and listed again are the chief putting the same id
    /// again: say that ticks were kept for unchanged steps, and forget the id.
    fn note_put_again(&mut self) {
        let again = self
            .answered_instructions
            .iter()
            .find(|id| {
                self.queue
                    .questions
                    .iter()
                    .any(|q| &q.id == *id && q.is_instructions())
            })
            .cloned();
        if let Some(id) = again {
            self.answered_instructions.retain(|x| x != &id);
            self.set_notice(instructions_changed_notice(&id));
        }
        // A question withdrawn while shown and put again: its picks come from the listing.
        let back = self.gone.iter().find_map(|id| {
            self.queue
                .questions
                .iter()
                .find(|q| &q.id == id && !q.is_instructions())
        });
        if let Some(q) = back {
            let id = q.id.clone();
            let notice = if q.picks.is_empty() {
                changed_notice(&id)
            } else {
                picks_kept_notice(&id)
            };
            self.gone.retain(|x| x != &id);
            self.set_notice(notice);
        }
    }

    fn follow_current(&mut self) {
        let Some(id) = self.current.clone() else {
            self.select_first();
            return;
        };
        match self.queue.questions.iter().find(|q| q.id == id) {
            Some(q) if q.version > self.version => {
                self.version = q.version;
                let notice = if q.is_instructions() {
                    // The ticks are the server's: nothing local to clear.
                    instructions_changed_notice(&id)
                } else if q.picks.is_empty() {
                    changed_notice(&id)
                } else {
                    picks_kept_notice(&id)
                };
                self.restart();
                self.set_notice(notice);
            }
            Some(_) => {}
            None => {
                self.gone.retain(|x| x != &id);
                self.gone.push(id.clone());
                if self.gone.len() > 16 {
                    self.gone.remove(0);
                }
                self.set_notice(self.dismissed_notice(&id));
                self.select_first();
            }
        }
    }

    /// Counts this listing for a notice on a block with no question, and clears the
    /// notice on its second.
    fn age_notice(&mut self) {
        if self.current.is_some() || self.notice.is_none() {
            self.notice_listed = false;
        } else if self.notice_listed {
            self.clear_notice();
        } else {
            self.notice_listed = true;
        }
    }

    /// Whether the block has anything to draw: a question or a notice, while not
    /// suspended.
    pub fn is_visible(&self) -> bool {
        !self.suspended && (!self.queue.questions.is_empty() || self.notice.is_some())
    }

    /// The number of listed todos, both kinds.
    pub fn todos_waiting(&self) -> usize {
        self.queue.questions.len()
    }

    /// Whether the question shows its rows.
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Whether a note or a free option's text field is open and in view; while it is,
    /// every key goes to it.
    pub fn note_open(&self) -> bool {
        !self.suspended && matches!(self.mode, Mode::Note { .. })
    }

    /// The row the open note belongs to: the free option's, or `relegate to idea…`'s,
    /// `revise…`'s or `reject…`'s. `None` with no note, or while the rows are not shown.
    pub fn note_stop(&self) -> Option<QStop> {
        if !self.note_open() || !self.open || self.question().is_none() {
            return None;
        }
        let Mode::Note { kind, .. } = &self.mode else {
            return None;
        };
        match kind {
            NoteKind::Relegate => Some(QStop::Action(1)),
            NoteKind::Revise => Some(QStop::Action(2)),
            NoteKind::Reject => Some(QStop::Action(3)),
            NoteKind::FreeText => self
                .free_target
                .map(|(decision, option)| QStop::Option { decision, option }),
        }
    }

    /// The rows the cursor can stop on, in order: empty with no question in view, the
    /// header alone while closed, and when open the header, every option in decision
    /// order, then the answer actions.
    pub fn stops(&self) -> Vec<QStop> {
        if !self.is_visible() {
            return vec![];
        }
        let Some(q) = self.question() else {
            return vec![];
        };
        let mut stops = vec![QStop::Header];
        if self.open {
            for (decision, d) in q.decisions.iter().enumerate() {
                stops.extend((0..d.options.len()).map(|option| QStop::Option { decision, option }));
            }
            if q.is_instructions() {
                stops.extend((0..q.steps.len()).map(QStop::Step));
                stops.extend((0..INSTRUCTION_ACTIONS.len()).map(QStop::Action));
            } else {
                stops.extend((0..ANSWER_ACTIONS.len()).map(QStop::Action));
            }
            stops.extend(
                self.queue
                    .questions
                    .iter()
                    .enumerate()
                    .filter(|(_, other)| other.id != q.id)
                    .map(|(i, _)| QStop::Other(i)),
            );
        }
        stops
    }

    /// Hides the block and keeps everything, until the next [`QuestionBlock::reconcile`].
    /// It adds no notice.
    pub fn suspend(&mut self) {
        // An empty block has nothing to keep, and stays equal to a fresh one.
        if self.is_visible() {
            self.suspended = true;
        }
    }

    /// The question in view, as (id, version, mode); `None` while hidden or showing
    /// only a notice. A change here means keys typed before it were meant for something
    /// else.
    pub fn shown(&self) -> Option<(String, u32, Mode)> {
        if !self.is_visible() {
            return None;
        }
        self.kept()
    }

    /// Like [`QuestionBlock::shown`], but also while suspended: what the block will show
    /// again if the next listing keeps it.
    pub fn kept(&self) -> Option<(String, u32, Mode)> {
        let q = self.question()?;
        Some((q.id.clone(), self.version, self.mode.clone()))
    }

    /// Clears the notice, as any key in the block does.
    pub fn clear_notice(&mut self) {
        self.notice = None;
        self.notice_listed = false;
    }

    /// Sets a notice, also for the runtime's own messages.
    pub fn set_notice(&mut self, text: String) {
        self.notice = Some(text);
        self.notice_listed = false;
    }

    /// Whether the block shows only a notice, with no question; any key clears it.
    pub fn shows_notice_only(&self) -> bool {
        self.is_visible() && self.question().is_none()
    }

    /// Applies the outcome of a send for question `id`. A recorded or no-longer-waiting
    /// question leaves the queue before the next one shows, so its picks are not kept.
    /// The next question shows open when the block was open; with none left it closes.
    pub fn sent(&mut self, id: &str, result: SendResult) {
        if self.current.as_deref() != Some(id) {
            return;
        }
        self.apply_result(id, result);
    }

    /// Applies the outcome of a tick of `step` (from 1) of instructions `id`. A recorded
    /// tick changes the local ticks; the other outcomes act as for a send.
    pub fn ticked(&mut self, id: &str, step: u32, ticked: bool, result: SendResult) {
        if self.current.as_deref() != Some(id) {
            return;
        }
        if result == SendResult::Recorded {
            if let Some(q) = self.queue.questions.iter_mut().find(|q| q.id == id) {
                q.ticked.retain(|n| *n != step);
                if ticked {
                    q.ticked.push(step);
                    q.ticked.sort_unstable();
                }
            }
            self.clear_notice();
            return;
        }
        self.apply_result(id, result);
    }

    /// Applies the outcome of saving the picks of question `id` at `version`. A save for a
    /// question or version no longer shown changes nothing. A failed save keeps the pick;
    /// the next pick or send carries it. The other refusals act as for a send.
    pub fn picks_saved(&mut self, id: &str, version: u32, result: SendResult) {
        if self.current.as_deref() != Some(id) || self.version != version {
            return;
        }
        match result {
            SendResult::Recorded => {}
            SendResult::Unavailable => self.set_notice(PICKS_UNAVAILABLE_NOTICE.into()),
            SendResult::VersionChanged | SendResult::NotWaiting => self.apply_result(id, result),
        }
    }

    fn apply_result(&mut self, id: &str, result: SendResult) {
        match result {
            SendResult::Recorded => {
                if self
                    .queue
                    .questions
                    .iter()
                    .any(|q| q.id == id && q.is_instructions())
                {
                    self.answered_instructions.retain(|x| x != id);
                    self.answered_instructions.push(id.to_string());
                    if self.answered_instructions.len() > 16 {
                        self.answered_instructions.remove(0);
                    }
                }
                self.queue.questions.retain(|q| q.id != id);
                self.queue.waiting = self.queue.waiting.saturating_sub(1);
                self.clear_notice();
                self.select_first();
            }
            SendResult::VersionChanged => {
                let instructions = self.question().is_some_and(QuestionView::is_instructions);
                self.restart();
                self.set_notice(if instructions {
                    instructions_changed_notice(id)
                } else {
                    changed_notice(id)
                });
            }
            SendResult::NotWaiting => {
                self.set_notice(self.dismissed_notice(id));
                self.queue.questions.retain(|q| q.id != id);
                self.select_first();
            }
            SendResult::Unavailable => {
                self.set_notice(UNAVAILABLE_NOTICE.into());
            }
        }
    }

    /// What to draw, or `None` when the block is hidden.
    pub fn view(&self) -> Option<RowsView> {
        if !self.is_visible() {
            return None;
        }
        let Some(q) = self.question() else {
            return Some(RowsView {
                kind: TodoKind::Question,
                id: String::new(),
                product: String::new(),
                spec: String::new(),
                title: String::new(),
                intro: String::new(),
                position: 0,
                waiting: self.todos_waiting() as u32,
                open: false,
                decisions: vec![],
                steps: vec![],
                ticked_count: 0,
                all_ticked: false,
                others: vec![],
                mode: Mode::Choosing,
                note_stop: None,
                notice: self.notice.clone(),
            });
        };
        let decisions = q
            .decisions
            .iter()
            .enumerate()
            .map(|(d, decision)| DecisionRows {
                ask: decision.ask.clone(),
                options: decision
                    .options
                    .iter()
                    .map(|o| {
                        let pick = self
                            .picks
                            .get(d)
                            .and_then(Option::as_ref)
                            .filter(|p| p.key == o.key);
                        OptionRows {
                            key: o.key.clone(),
                            label: o.label.clone(),
                            impact: o.impact.clone(),
                            suggested: o.suggested,
                            free: o.free,
                            picked: pick.is_some(),
                            free_text: pick.filter(|_| o.free).map(|p| p.text.clone()),
                        }
                    })
                    .collect(),
            })
            .collect();
        let steps: Vec<StepRows> = q
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| StepRows {
                text: step.text.clone(),
                detail: step.detail.clone(),
                ticked: q.ticked.contains(&(i as u32 + 1)),
            })
            .collect();
        let ticked_count = q.ticked_count();
        let others = self
            .queue
            .questions
            .iter()
            .enumerate()
            .filter(|(_, other)| other.id != q.id)
            .map(|(index, other)| OtherRow {
                index,
                kind: other.kind(),
                id: other.id.clone(),
                ticked_count: other.ticked_count(),
                steps_total: other.steps.len(),
                product: other.product.clone(),
                label: if other.is_instructions() {
                    other.title.clone()
                } else {
                    other.spec.clone()
                },
            })
            .collect();
        Some(RowsView {
            kind: q.kind(),
            id: q.id.clone(),
            product: q.product.clone(),
            spec: q.spec.clone(),
            title: q.title.clone(),
            intro: q.intro.clone(),
            position: self
                .queue
                .questions
                .iter()
                .position(|x| x.id == q.id)
                .map_or(1, |i| i + 1),
            waiting: self.todos_waiting() as u32,
            open: self.open,
            decisions,
            all_ticked: !steps.is_empty() && ticked_count == steps.len(),
            steps,
            ticked_count,
            others,
            mode: self.mode.clone(),
            note_stop: self.note_stop(),
            notice: self.notice.clone(),
        })
    }

    /// Whether the current todo has a link `o` can open.
    pub fn has_web_link(&self) -> bool {
        self.question().is_some_and(|q| is_web_link(&q.link))
    }

    /// Whether the current todo is instructions.
    pub fn on_instructions(&self) -> bool {
        self.question().is_some_and(QuestionView::is_instructions)
    }

    /// Handles one key on `stop` and says what the runtime should do. While a note is
    /// open every key goes to the note, whatever the stop.
    pub fn handle_key(&mut self, code: KeyCode, stop: QStop) -> BlockEffect {
        if self.suspended {
            return BlockEffect::None;
        }
        if self.question().is_none() {
            // Any key clears a notice on a block with no question, so the block hides.
            self.clear_notice();
            return BlockEffect::None;
        }
        self.clear_notice();
        match self.mode.clone() {
            Mode::Choosing => self.choosing_key(code, stop),
            Mode::Note { kind, text } => self.note_key(code, kind, text),
        }
    }

    fn choosing_key(&mut self, code: KeyCode, stop: QStop) -> BlockEffect {
        let Some(q) = self.question().cloned() else {
            return BlockEffect::None;
        };
        // A closed question has only its header.
        let stop = if self.open { stop } else { QStop::Header };
        match code {
            KeyCode::Char('o') if is_web_link(&q.link) && !matches!(stop, QStop::Other(_)) => {
                return BlockEffect::OpenLink(q.link);
            }
            KeyCode::Char('n') if self.step(1) => return BlockEffect::Cursor(QStop::Header),
            KeyCode::Char('N') if self.step(-1) => return BlockEffect::Cursor(QStop::Header),
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc
                if self.open && !matches!(stop, QStop::Other(_)) =>
            {
                self.open = false;
                return BlockEffect::Cursor(QStop::Header);
            }
            KeyCode::Char('l') | KeyCode::Right if stop == QStop::Header => self.open = true,
            KeyCode::Char('l') | KeyCode::Right if matches!(stop, QStop::Other(_)) => {
                return self.open_other(stop);
            }
            KeyCode::Enter => match stop {
                QStop::Header => {
                    self.open = !self.open;
                    return BlockEffect::Cursor(QStop::Header);
                }
                QStop::Option { .. } | QStop::Step(_) => {}
                QStop::Other(_) => return self.open_other(stop),
                QStop::Action(action) if q.is_instructions() => {
                    return self.instruction_action(action, &q);
                }
                QStop::Action(action) => return self.answer_action(action, &q),
            },
            KeyCode::Char(' ') => match stop {
                QStop::Option { decision, option } if !q.is_instructions() => {
                    if self.pick(&q, decision, option) {
                        return self.save_picks(&q);
                    }
                }
                QStop::Step(i) if q.is_instructions() && i < q.steps.len() => {
                    let step = i as u32 + 1;
                    return BlockEffect::Tick {
                        id: q.id.clone(),
                        version: self.version,
                        step,
                        ticked: !q.ticked.contains(&step),
                    };
                }
                _ => {}
            },
            _ => {}
        }
        BlockEffect::None
    }

    /// Makes the todo at `Other` stop current, keeping the block open.
    fn open_other(&mut self, stop: QStop) -> BlockEffect {
        let QStop::Other(i) = stop else {
            return BlockEffect::None;
        };
        let Some(id) = self.queue.questions.get(i).map(|q| q.id.clone()) else {
            return BlockEffect::None;
        };
        if self.current.as_deref() == Some(id.as_str()) {
            return BlockEffect::None;
        }
        self.show(&id);
        BlockEffect::Cursor(QStop::Header)
    }

    /// Enter on [`INSTRUCTION_ACTIONS`] row `action`. `done` needs no count: Harold counts.
    fn instruction_action(&mut self, action: usize, q: &QuestionView) -> BlockEffect {
        let kind = match action {
            0 => AnswerKind::Done,
            1 => AnswerKind::Ignore,
            _ => return BlockEffect::None,
        };
        BlockEffect::Send(Outgoing {
            id: q.id.clone(),
            version: self.version,
            kind,
            picks: vec![],
            note: String::new(),
            send_now: true,
        })
    }

    /// The effect that saves the question's current picks.
    fn save_picks(&self, q: &QuestionView) -> BlockEffect {
        BlockEffect::SavePicks {
            id: q.id.clone(),
            version: self.version,
            picks: self.picks.iter().flatten().cloned().collect(),
        }
    }

    /// Picks option `option` of `decision` and says whether a pick was made; a free option
    /// opens its field instead, holding the text it was picked with before, and picks
    /// nothing until `Enter`.
    fn pick(&mut self, q: &QuestionView, decision: usize, option: usize) -> bool {
        let Some(chosen) = q
            .decisions
            .get(decision)
            .and_then(|d| d.options.get(option))
        else {
            return false;
        };
        if chosen.free {
            let text = self
                .picks
                .get(decision)
                .and_then(Option::as_ref)
                .filter(|p| p.key == chosen.key)
                .map(|p| p.text.clone())
                .unwrap_or_default();
            self.free_target = Some((decision, option));
            self.mode = Mode::Note {
                kind: NoteKind::FreeText,
                text,
            };
            false
        } else {
            self.set_pick(decision, chosen.key.clone(), String::new());
            true
        }
    }

    /// Enter on [`ANSWER_ACTIONS`] row `action`.
    fn answer_action(&mut self, action: usize, q: &QuestionView) -> BlockEffect {
        match action {
            0 => {
                // The first decision without a pick stops the send.
                if let Some(decision) =
                    (0..q.decisions.len()).find(|d| self.picks.get(*d).is_none_or(Option::is_none))
                {
                    return BlockEffect::Cursor(if q.decisions[decision].options.is_empty() {
                        QStop::Header
                    } else {
                        QStop::Option {
                            decision,
                            option: 0,
                        }
                    });
                }
                let last_free = self
                    .picks
                    .iter()
                    .flatten()
                    .rev()
                    .find(|p| self.is_free(q, p))
                    .map(|p| p.text.clone());
                let (kind, note) = match last_free {
                    Some(text) => (AnswerKind::FreeText, text),
                    None => (AnswerKind::Answer, String::new()),
                };
                BlockEffect::Send(self.outgoing(q, kind, note))
            }
            1 => {
                self.open_note(NoteKind::Relegate);
                BlockEffect::None
            }
            2 => {
                self.open_note(NoteKind::Revise);
                BlockEffect::None
            }
            3 => {
                self.open_note(NoteKind::Reject);
                BlockEffect::None
            }
            _ => BlockEffect::None,
        }
    }

    fn outgoing(&self, q: &QuestionView, kind: AnswerKind, note: String) -> Outgoing {
        Outgoing {
            id: q.id.clone(),
            version: self.version,
            kind,
            picks: self.picks.iter().flatten().cloned().collect(),
            note,
            send_now: true,
        }
    }

    /// Shows the listed question `by` places from the current one, wrapping over the
    /// listed order, and reports whether another question came into view.
    fn step(&mut self, by: isize) -> bool {
        let listed = &self.queue.questions;
        let Some(at) = listed
            .iter()
            .position(|q| Some(&q.id) == self.current.as_ref())
        else {
            return false;
        };
        let next = (at as isize + by).rem_euclid(listed.len() as isize) as usize;
        if next == at {
            return false;
        }
        let id = listed[next].id.clone();
        self.show(&id);
        true
    }

    /// Shows the first or the last listed todo, leaving the block open or closed as it is.
    pub fn show_boundary(&mut self, last: bool) {
        let listed = &self.queue.questions;
        let boundary = if last { listed.last() } else { listed.first() };
        if let Some(id) = boundary.map(|q| q.id.clone()) {
            self.show(&id);
        }
    }

    fn note_key(&mut self, code: KeyCode, kind: NoteKind, mut text: String) -> BlockEffect {
        let Some(q) = self.question().cloned() else {
            return BlockEffect::None;
        };
        match code {
            KeyCode::Char(c) if !c.is_control() => {
                if text.chars().count() < NOTE_LIMIT {
                    text.push(c);
                }
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Esc => {
                self.mode = Mode::Choosing;
                self.free_target = None;
                return BlockEffect::None;
            }
            KeyCode::Enter => {
                let typed = text.trim().to_string();
                // A relegate sends with no note; every other note needs words.
                if typed.is_empty() && kind != NoteKind::Relegate {
                    return BlockEffect::None;
                }
                match kind {
                    NoteKind::FreeText => {
                        if let Some((decision, option)) = self.free_target.take()
                            && let Some(chosen) = q
                                .decisions
                                .get(decision)
                                .and_then(|d| d.options.get(option))
                        {
                            self.set_pick(decision, chosen.key.clone(), typed);
                        }
                        self.mode = Mode::Choosing;
                        return self.save_picks(&q);
                    }
                    NoteKind::Revise | NoteKind::Reject | NoteKind::Relegate => {
                        let kind = match kind {
                            NoteKind::Revise => AnswerKind::Revise,
                            NoteKind::Reject => AnswerKind::Reject,
                            _ => AnswerKind::Relegate,
                        };
                        return BlockEffect::Send(self.outgoing(&q, kind, typed));
                    }
                }
            }
            _ => {}
        }
        self.mode = Mode::Note { kind, text };
        BlockEffect::None
    }

    fn open_note(&mut self, kind: NoteKind) {
        self.mode = Mode::Note {
            kind,
            text: String::new(),
        };
    }

    /// Records `decision`'s pick, replacing an earlier one.
    fn set_pick(&mut self, decision: usize, key: String, text: String) {
        if let Some(slot) = self.picks.get_mut(decision) {
            *slot = Some(PickView {
                decision: decision as u32,
                key,
                text,
            });
        }
    }

    fn question(&self) -> Option<&QuestionView> {
        let id = self.current.as_deref()?;
        self.queue.questions.iter().find(|q| q.id == id)
    }

    fn is_free(&self, q: &QuestionView, pick: &PickView) -> bool {
        q.decisions
            .get(pick.decision as usize)
            .is_some_and(|d| d.options.iter().any(|o| o.key == pick.key && o.free))
    }

    fn dismissed_notice(&self, id: &str) -> String {
        let reason = self
            .queue
            .dismissed
            .iter()
            .find(|(dismissed, _)| dismissed == id)
            .map_or("withdrawn", |(_, reason)| reason.as_str());
        format!("{id} dismissed: {reason}")
    }

    /// The current question with its starting picks and no note.
    fn restart(&mut self) {
        self.mode = Mode::Choosing;
        self.free_target = None;
        let Some(q) = self.question() else {
            self.picks = vec![];
            return;
        };
        // Instructions have no picks, and a changed version keeps their ticks.
        if !q.is_instructions() {
            self.picks = starting_picks(q);
        }
    }

    /// Makes the queue's first question current, or none. With none the block closes, so
    /// a queue that fills again starts closed.
    fn select_first(&mut self) {
        match self.queue.questions.first() {
            Some(q) => {
                let id = q.id.clone();
                self.show(&id);
            }
            None => {
                self.current = None;
                self.open = false;
                self.restart();
            }
        }
    }

    /// Makes listed question `id` current. The current question's picks are kept while
    /// it is still listed, and `id`'s kept picks come back when they were made on the
    /// listed version; otherwise `id` starts from its saved picks and suggestions.
    fn show(&mut self, id: &str) {
        let Some(version) = self
            .queue
            .questions
            .iter()
            .find(|q| q.id == id)
            .map(|q| q.version)
        else {
            return;
        };
        // Only a question has picks to keep; instructions' ticks are the server's.
        if let Some(current) = self
            .question()
            .filter(|q| !q.is_instructions())
            .map(|q| q.id.clone())
        {
            let kept = Kept {
                version: self.version,
                picks: std::mem::take(&mut self.picks),
            };
            self.kept.insert(current, kept);
        }
        self.current = Some(id.to_string());
        self.version = version;
        self.mode = Mode::Choosing;
        self.free_target = None;
        match self.kept.remove(id) {
            Some(kept) if kept.version == version => self.picks = kept.picks,
            _ => self.restart(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(key: &str, suggested: bool, free: bool) -> OptionView {
        OptionView {
            key: key.into(),
            label: format!("label {key}"),
            impact: format!("impact {key}"),
            suggested,
            free,
        }
    }

    fn question(id: &str, version: u32, decisions: usize) -> QuestionView {
        QuestionView {
            id: id.into(),
            product: "prod".into(),
            spec: "spec".into(),
            sha: "abc".into(),
            link: "https://example.com/x".into(),
            intro: "intro".into(),
            version,
            title: String::new(),
            steps: vec![],
            ticked: vec![],
            picks: vec![],
            decisions: (1..=decisions)
                .map(|n| DecisionView {
                    ask: format!("Ask {n}?"),
                    options: vec![
                        opt("a", false, false),
                        opt("b", true, false),
                        opt("z", false, true),
                    ],
                })
                .collect(),
        }
    }

    fn queue(questions: Vec<QuestionView>) -> Queue {
        let waiting = questions.len() as u32;
        Queue {
            questions,
            waiting,
            dismissed: vec![],
        }
    }

    fn block(questions: Vec<QuestionView>) -> QuestionBlock {
        let mut block = QuestionBlock::default();
        block.reconcile(queue(questions));
        block
    }

    /// A block that is open, as after `l` on the header.
    fn open_block(questions: Vec<QuestionView>) -> QuestionBlock {
        let mut b = block(questions);
        assert_eq!(
            press(&mut b, KeyCode::Char('l'), QStop::Header),
            BlockEffect::None
        );
        assert!(b.is_open());
        b
    }

    fn press(block: &mut QuestionBlock, code: KeyCode, stop: QStop) -> BlockEffect {
        block.handle_key(code, stop)
    }

    fn opt_stop(decision: usize, option: usize) -> QStop {
        QStop::Option { decision, option }
    }

    fn type_text(block: &mut QuestionBlock, text: &str) {
        for c in text.chars() {
            press(block, KeyCode::Char(c), QStop::Header);
        }
    }

    fn pick(decision: u32, key: &str, text: &str) -> PickView {
        PickView {
            decision,
            key: key.into(),
            text: text.into(),
        }
    }

    /// The picked option key of every decision.
    fn picked(block: &QuestionBlock) -> Vec<Option<String>> {
        block
            .view()
            .unwrap()
            .decisions
            .iter()
            .map(|d| d.options.iter().find(|o| o.picked).map(|o| o.key.clone()))
            .collect()
    }

    fn keys(keys: &[&str]) -> Vec<Option<String>> {
        keys.iter()
            .map(|k| (!k.is_empty()).then(|| (*k).to_string()))
            .collect()
    }

    #[test]
    fn the_answer_actions_are_send_relegate_to_idea_revise_and_reject() {
        assert_eq!(
            ANSWER_ACTIONS,
            [
                "send",
                "relegate to idea\u{2026}",
                "revise\u{2026}",
                "reject\u{2026}"
            ]
        );
    }

    #[test]
    fn starting_picks_follow_suggested() {
        let b = block(vec![question("Q1", 1, 1)]);
        assert_eq!(picked(&b), keys(&["b"]));

        let mut q = question("Q1", 1, 2);
        q.decisions[1].options[1].suggested = false;
        let b = block(vec![q]);
        assert_eq!(picked(&b), keys(&["b", ""]), "none suggested, none picked");

        // A suggested free option has no text yet, so it starts with no pick.
        let mut q = question("Q1", 1, 1);
        q.decisions[0].options[1].suggested = false;
        q.decisions[0].options[2].suggested = true;
        assert_eq!(picked(&block(vec![q])), keys(&[""]));
    }

    #[test]
    fn stops_closed_then_open() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        assert_eq!(b.stops(), vec![QStop::Header]);
        assert!(!b.view().unwrap().open);
        press(&mut b, KeyCode::Char('l'), QStop::Header);
        let mut want = vec![QStop::Header];
        for decision in 0..2 {
            want.extend((0..3).map(|option| opt_stop(decision, option)));
        }
        want.extend((0..4).map(QStop::Action));
        assert_eq!(b.stops(), want);
        assert!(b.view().unwrap().open);
    }

    #[test]
    fn no_stops_without_a_question_or_while_hidden() {
        let mut b = QuestionBlock::default();
        assert!(b.stops().is_empty());
        let mut b2 = block(vec![question("Q1", 1, 1)]);
        b2.suspend();
        assert!(b2.stops().is_empty());
        b.reconcile(queue(vec![]));
        assert!(b.stops().is_empty());
    }

    #[test]
    fn header_opens_and_closes_with_l_right_enter_h_left_esc() {
        for open in [KeyCode::Char('l'), KeyCode::Right, KeyCode::Enter] {
            let mut b = block(vec![question("Q1", 1, 1)]);
            press(&mut b, open, QStop::Header);
            assert!(b.is_open(), "{open:?}");
        }
        for close in [
            KeyCode::Char('h'),
            KeyCode::Left,
            KeyCode::Esc,
            KeyCode::Enter,
        ] {
            let mut b = open_block(vec![question("Q1", 1, 1)]);
            assert_eq!(
                press(&mut b, close, QStop::Header),
                BlockEffect::Cursor(QStop::Header),
                "{close:?}"
            );
            assert!(!b.is_open(), "{close:?}");
        }
        // On a closed header, h and Esc do nothing, and l on an open header does nothing.
        let mut b = block(vec![question("Q1", 1, 1)]);
        for code in [KeyCode::Char('h'), KeyCode::Esc] {
            assert_eq!(press(&mut b, code, QStop::Header), BlockEffect::None);
            assert!(!b.is_open());
        }
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        assert_eq!(
            press(&mut b, KeyCode::Char('l'), QStop::Header),
            BlockEffect::None
        );
        assert!(b.is_open());
    }

    #[test]
    fn h_left_and_esc_on_an_option_or_action_close_with_the_cursor_on_the_header() {
        for stop in [opt_stop(0, 1), QStop::Action(0), QStop::Action(3)] {
            for code in [KeyCode::Char('h'), KeyCode::Left, KeyCode::Esc] {
                let mut b = open_block(vec![question("Q1", 1, 1)]);
                assert_eq!(
                    press(&mut b, code, stop),
                    BlockEffect::Cursor(QStop::Header),
                    "{stop:?} {code:?}"
                );
                assert!(!b.is_open());
            }
        }
    }

    #[test]
    fn l_does_nothing_on_options_and_actions() {
        for stop in [opt_stop(0, 0), QStop::Action(0), QStop::Action(2)] {
            for code in [KeyCode::Char('l'), KeyCode::Right] {
                let mut b = open_block(vec![question("Q1", 1, 1)]);
                let before = b.view();
                assert_eq!(press(&mut b, code, stop), BlockEffect::None);
                assert_eq!(b.view(), before);
            }
        }
    }

    fn with_picks(mut q: QuestionView, picks: Vec<PickView>) -> QuestionView {
        q.picks = picks;
        q
    }

    fn save_of(effect: BlockEffect) -> (String, u32, Vec<PickView>) {
        match effect {
            BlockEffect::SavePicks { id, version, picks } => (id, version, picks),
            other => panic!("expected SavePicks, got {other:?}"),
        }
    }

    #[test]
    fn a_pick_sends_one_save_with_all_current_picks() {
        let mut b = open_block(vec![question("Q1", 3, 2)]);
        let (id, version, picks) = save_of(press(&mut b, KeyCode::Char(' '), opt_stop(0, 0)));
        assert_eq!((id.as_str(), version), ("Q1", 3));
        // Decision 0 now holds a; decision 1 still holds its suggestion b.
        assert_eq!(picks, vec![pick(0, "a", ""), pick(1, "b", "")]);
    }

    #[test]
    fn a_kept_free_text_sends_one_save_and_half_typed_text_sends_none() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        assert_eq!(
            press(&mut b, KeyCode::Char(' '), opt_stop(0, 2)),
            BlockEffect::None,
            "a free option only opens its field"
        );
        type_text(&mut b, "later");
        let (_, _, picks) = save_of(press(&mut b, KeyCode::Enter, opt_stop(0, 2)));
        assert_eq!(picks, vec![pick(0, "z", "later")]);
        // Esc on a half-typed field saves nothing and keeps the earlier pick.
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 2));
        type_text(&mut b, "x");
        assert_eq!(
            press(&mut b, KeyCode::Esc, opt_stop(0, 2)),
            BlockEffect::None
        );
        assert_eq!(picked(&b), keys(&["z"]));
    }

    #[test]
    fn a_fresh_block_starts_from_listed_picks_and_suggestions_for_the_rest() {
        let q = with_picks(
            question("Q1", 1, 3),
            vec![pick(0, "a", ""), pick(2, "z", "typed")],
        );
        let b = open_block(vec![q]);
        // Decision 1 has no saved pick: its suggestion b.
        assert_eq!(picked(&b), keys(&["a", "b", "z"]));
        let view = b.view().unwrap();
        assert_eq!(
            view.decisions[2].options[2].free_text.as_deref(),
            Some("typed")
        );
    }

    #[test]
    fn a_listed_pick_with_an_unknown_key_or_empty_free_text_is_ignored() {
        let q = with_picks(
            question("Q1", 1, 3),
            vec![pick(0, "gone", ""), pick(1, "z", ""), pick(7, "a", "")],
        );
        let b = open_block(vec![q]);
        assert_eq!(picked(&b), keys(&["b", "b", "b"]));
    }

    #[test]
    fn a_higher_version_with_picks_says_picks_kept_and_without_says_cleared() {
        let mut b = open_block(vec![question("Q1", 1, 2)]);
        b.reconcile(queue(vec![with_picks(
            question("Q1", 2, 2),
            vec![pick(1, "a", "")],
        )]));
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 changed, picks kept for unchanged decisions")
        );
        assert_eq!(picked(&b), keys(&["b", "a"]));
        b.reconcile(queue(vec![question("Q1", 3, 2)]));
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
    }

    #[test]
    fn a_question_dismissed_then_put_again_says_picks_kept() {
        let mut b = open_block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        // The chief dismisses Q1; the dash reads the empty listing first.
        let mut gone = queue(vec![]);
        gone.dismissed = vec![("Q1".into(), "stale".into())];
        b.reconcile(gone);
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 dismissed: stale")
        );
        b.reconcile(queue(vec![with_picks(
            question("Q1", 2, 2),
            vec![pick(0, "a", "")],
        )]));
        let view = b.view().unwrap();
        assert_eq!(
            view.notice.as_deref(),
            Some("Q1 changed, picks kept for unchanged decisions")
        );
        assert_eq!(view.id, "Q1");
        assert_eq!(picked(&b), keys(&["a", "b"]));
        // Put again once more with no picks: the cleared form, once only.
        b.reconcile(queue(vec![]));
        b.reconcile(queue(vec![question("Q1", 3, 2)]));
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
    }

    #[test]
    fn an_older_listing_at_the_same_version_does_not_undo_a_pick() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        // A listing read before the save landed carries no picks.
        b.reconcile(queue(vec![question("Q1", 1, 1)]));
        assert_eq!(picked(&b), keys(&["a"]));
    }

    #[test]
    fn a_failed_save_says_so_keeps_the_pick_and_the_next_pick_carries_it() {
        let mut b = open_block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        b.picks_saved("Q1", 1, SendResult::Unavailable);
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Harold can't save your picks right now")
        );
        assert_eq!(picked(&b), keys(&["a", "b"]));
        let (_, _, picks) = save_of(press(&mut b, KeyCode::Char(' '), opt_stop(1, 0)));
        assert_eq!(picks, vec![pick(0, "a", ""), pick(1, "a", "")]);
    }

    #[test]
    fn a_recorded_save_changes_nothing_and_a_stale_result_is_ignored() {
        let mut b = open_block(vec![question("Q1", 2, 1)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        let before = b.view();
        b.picks_saved("Q1", 2, SendResult::Recorded);
        assert_eq!(b.view(), before);
        b.picks_saved("Q1", 1, SendResult::Unavailable);
        b.picks_saved("Q9", 2, SendResult::Unavailable);
        assert_eq!(b.view(), before);
    }

    #[test]
    fn a_save_refused_for_version_or_waiting_acts_as_for_a_send() {
        let mut b = open_block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        b.picks_saved("Q1", 1, SendResult::VersionChanged);
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
        b.picks_saved("Q1", 1, SendResult::NotWaiting);
        assert_eq!(b.view().unwrap().id, "Q2");
    }

    #[test]
    fn space_picks_and_other_keys_do_not() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        assert_eq!(picked(&b), keys(&["b"]));
        assert_eq!(
            press(&mut b, KeyCode::Char(' '), opt_stop(0, 0)),
            BlockEffect::SavePicks {
                id: "Q1".into(),
                version: 1,
                picks: vec![PickView {
                    decision: 0,
                    key: "a".into(),
                    text: String::new()
                }]
            }
        );
        assert_eq!(picked(&b), keys(&["a"]), "a picked, b unpicked");
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('l'),
            KeyCode::Enter,
            KeyCode::Down,
            KeyCode::Right,
        ] {
            press(&mut b, code, opt_stop(0, 1));
            assert_eq!(picked(&b), keys(&["a"]), "{code:?}");
        }
        // Space elsewhere picks nothing.
        press(&mut b, KeyCode::Char(' '), QStop::Header);
        press(&mut b, KeyCode::Char(' '), QStop::Action(1));
        assert_eq!(picked(&b), keys(&["a"]));
    }

    #[test]
    fn close_keeps_picks() {
        let mut b = open_block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(1, 0));
        let before = picked(&b);
        assert_eq!(before, keys(&["b", "a"]));
        assert_eq!(
            press(&mut b, KeyCode::Char('h'), opt_stop(1, 0)),
            BlockEffect::Cursor(QStop::Header)
        );
        press(&mut b, KeyCode::Char('l'), QStop::Header);
        assert_eq!(picked(&b), before);
    }

    #[test]
    fn send_with_missing_pick_moves_cursor() {
        let mut q = question("Q1", 1, 3);
        q.decisions[1].options[1].suggested = false;
        q.decisions[2].options[1].suggested = false;
        let mut b = open_block(vec![q]);
        assert_eq!(
            press(&mut b, KeyCode::Enter, QStop::Action(0)),
            BlockEffect::Cursor(opt_stop(1, 0))
        );
        press(&mut b, KeyCode::Char(' '), opt_stop(1, 2));
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::FreeText,
                text: String::new()
            }
        );
        type_text(&mut b, "x");
        press(&mut b, KeyCode::Enter, opt_stop(1, 2));
        assert_eq!(
            press(&mut b, KeyCode::Enter, QStop::Action(0)),
            BlockEffect::Cursor(opt_stop(2, 0)),
            "the next missing decision"
        );
        press(&mut b, KeyCode::Char(' '), opt_stop(2, 0));
        assert!(matches!(
            press(&mut b, KeyCode::Enter, QStop::Action(0)),
            BlockEffect::Send(_)
        ));
    }

    #[test]
    fn send_returns_every_pick_with_version_and_send_now() {
        let mut b = open_block(vec![question("Q1", 3, 2)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(1, 0));
        assert_eq!(
            press(&mut b, KeyCode::Enter, QStop::Action(0)),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 3,
                kind: AnswerKind::Answer,
                picks: vec![pick(0, "b", ""), pick(1, "a", "")],
                note: String::new(),
                send_now: true,
            })
        );
    }

    #[test]
    fn s_does_nothing() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        let before = b.view();
        for stop in [
            QStop::Header,
            opt_stop(0, 0),
            QStop::Action(0),
            QStop::Action(1),
        ] {
            assert_eq!(press(&mut b, KeyCode::Char('s'), stop), BlockEffect::None);
            assert_eq!(b.view(), before);
        }
    }

    #[test]
    fn other_letters_and_tab_do_nothing_on_question_stops() {
        let mut b = open_block(vec![question("Q1", 1, 2), question("Q2", 1, 1)]);
        let before = b.view();
        for code in [
            KeyCode::Char('c'),
            KeyCode::Char('t'),
            KeyCode::Char('f'),
            KeyCode::Char('/'),
            KeyCode::Char('q'),
            KeyCode::Char('x'),
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
        ] {
            for stop in [QStop::Header, opt_stop(0, 0), QStop::Action(2)] {
                assert_eq!(press(&mut b, code, stop), BlockEffect::None, "{code:?}");
                assert_eq!(b.view(), before, "{code:?}");
            }
        }
    }

    #[test]
    fn free_option_space_opens_field_enter_picks_blank_rejected() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        assert_eq!(
            press(&mut b, KeyCode::Char(' '), opt_stop(0, 2)),
            BlockEffect::None
        );
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::FreeText,
                text: String::new()
            }
        );
        assert_eq!(picked(&b), keys(&["b"]), "no change before Enter");
        assert_eq!(
            press(&mut b, KeyCode::Enter, opt_stop(0, 2)),
            BlockEffect::None
        );
        assert!(b.note_open(), "blank is not accepted");
        type_text(&mut b, "   ");
        assert_eq!(
            press(&mut b, KeyCode::Enter, opt_stop(0, 2)),
            BlockEffect::None
        );
        assert!(b.note_open(), "whitespace is not accepted");
        type_text(&mut b, "my words");
        press(&mut b, KeyCode::Enter, opt_stop(0, 2));
        assert!(!b.note_open());
        assert_eq!(picked(&b), keys(&["z"]));
        let v = b.view().unwrap();
        assert_eq!(
            v.decisions[0].options[2].free_text.as_deref(),
            Some("my words")
        );
        assert_eq!(
            press(&mut b, KeyCode::Enter, QStop::Action(0)),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 1,
                kind: AnswerKind::FreeText,
                picks: vec![pick(0, "z", "my words")],
                note: "my words".into(),
                send_now: true,
            })
        );

        // The field holds the earlier text; Esc discards what was typed and keeps the pick.
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 2));
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::FreeText,
                text: "my words".into()
            }
        );
        type_text(&mut b, "!!");
        press(&mut b, KeyCode::Esc, opt_stop(0, 2));
        assert!(!b.note_open());
        assert_eq!(
            b.view().unwrap().decisions[0].options[2]
                .free_text
                .as_deref(),
            Some("my words")
        );
        // Picking another option replaces it; the free text is gone from the view.
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        assert_eq!(picked(&b), keys(&["a"]));
        assert_eq!(b.view().unwrap().decisions[0].options[2].free_text, None);
    }

    #[test]
    fn a_note_types_every_letter() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Enter, QStop::Action(2));
        type_text(&mut b, "hjklnNrxoqs ");
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::Revise,
                text: "hjklnNrxoqs ".into()
            }
        );
        assert!(b.is_open());
    }

    #[test]
    fn revise_note_sends_with_picks_and_send_now_true() {
        for (action, kind) in [(2, AnswerKind::Revise), (3, AnswerKind::Reject)] {
            let mut b = open_block(vec![question("Q1", 2, 2)]);
            assert_eq!(
                press(&mut b, KeyCode::Enter, QStop::Action(action)),
                BlockEffect::None
            );
            assert_eq!(
                press(&mut b, KeyCode::Enter, QStop::Action(action)),
                BlockEffect::None,
                "blank sends nothing"
            );
            type_text(&mut b, "fixx");
            press(&mut b, KeyCode::Backspace, QStop::Action(action));
            assert_eq!(
                press(&mut b, KeyCode::Enter, QStop::Action(action)),
                BlockEffect::Send(Outgoing {
                    id: "Q1".into(),
                    version: 2,
                    kind,
                    picks: vec![pick(0, "b", ""), pick(1, "b", "")],
                    note: "fix".into(),
                    send_now: true,
                })
            );
        }
        // A decision with no pick does not stop a note: the picks so far go.
        let mut q = question("Q1", 2, 2);
        q.decisions[1].options[1].suggested = false;
        let mut b = open_block(vec![q]);
        press(&mut b, KeyCode::Enter, QStop::Action(3));
        type_text(&mut b, "no");
        match press(&mut b, KeyCode::Enter, QStop::Action(3)) {
            BlockEffect::Send(out) => assert_eq!(out.picks, vec![pick(0, "b", "")]),
            other => panic!("{other:?}"),
        }
        // Esc leaves the note, dropping its text.
        let mut b = open_block(vec![question("Q1", 2, 2)]);
        press(&mut b, KeyCode::Enter, QStop::Action(2));
        type_text(&mut b, "abc");
        assert_eq!(
            press(&mut b, KeyCode::Esc, QStop::Action(2)),
            BlockEffect::None
        );
        assert_eq!(b.view().unwrap().mode, Mode::Choosing);
        assert!(b.is_open());
    }

    #[test]
    fn a_note_stops_at_1999_characters() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Enter, QStop::Action(2));
        type_text(&mut b, &"a".repeat(1_999));
        type_text(&mut b, "bcd");
        match b.view().unwrap().mode {
            Mode::Note { text, .. } => {
                assert_eq!(text.chars().count(), 1_999);
                assert!(text.chars().all(|c| c == 'a'));
            }
            other => panic!("{other:?}"),
        }
        press(&mut b, KeyCode::Backspace, QStop::Action(2));
        press(&mut b, KeyCode::Char('z'), QStop::Action(2));
        match press(&mut b, KeyCode::Enter, QStop::Action(2)) {
            BlockEffect::Send(out) => {
                assert_eq!(out.note.chars().count(), 1_999);
                assert!(out.note.ends_with('z'));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn relegate_to_idea_opens_a_note_that_sends_with_the_picks_even_when_empty() {
        let mut b = open_block(vec![question("Q1", 2, 2), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(1, 0));
        assert_eq!(
            press(&mut b, KeyCode::Enter, QStop::Action(1)),
            BlockEffect::None
        );
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::Relegate,
                text: String::new()
            }
        );
        assert_eq!(b.note_stop(), Some(QStop::Action(1)));
        // Enter on the empty note sends the picks so far.
        assert_eq!(
            press(&mut b, KeyCode::Enter, QStop::Action(1)),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 2,
                kind: AnswerKind::Relegate,
                picks: vec![pick(0, "b", ""), pick(1, "a", "")],
                note: String::new(),
                send_now: true,
            })
        );

        // With words typed, the note goes with it, and a blank one counts as empty.
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Enter, QStop::Action(1));
        type_text(&mut b, "  park it ");
        assert_eq!(
            send_of(press(&mut b, KeyCode::Enter, QStop::Action(1))).note,
            "park it"
        );
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Enter, QStop::Action(1));
        type_text(&mut b, "   ");
        let out = send_of(press(&mut b, KeyCode::Enter, QStop::Action(1)));
        assert_eq!((out.kind, out.note.as_str()), (AnswerKind::Relegate, ""));

        // Esc leaves the note without sending.
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Enter, QStop::Action(1));
        assert_eq!(
            press(&mut b, KeyCode::Esc, QStop::Action(1)),
            BlockEffect::None
        );
        assert_eq!(b.view().unwrap().mode, Mode::Choosing);
    }

    #[test]
    fn n_and_n_shift_wrap_and_keep_open() {
        let shown_id = |b: &QuestionBlock| b.view().unwrap().id;
        let mut b = open_block(vec![
            question("Q1", 1, 1),
            question("Q2", 1, 1),
            question("Q3", 1, 1),
        ]);
        assert_eq!(
            press(&mut b, KeyCode::Char('n'), opt_stop(0, 1)),
            BlockEffect::Cursor(QStop::Header)
        );
        assert_eq!(shown_id(&b), "Q2");
        press(&mut b, KeyCode::Char('n'), QStop::Header);
        press(&mut b, KeyCode::Char('n'), QStop::Action(0));
        assert_eq!(shown_id(&b), "Q1", "n wraps past the last question");
        assert_eq!(
            press(&mut b, KeyCode::Char('N'), QStop::Header),
            BlockEffect::Cursor(QStop::Header)
        );
        assert_eq!(shown_id(&b), "Q3", "N wraps past the first question");
        assert!(b.is_open());

        // Closed, n steps and stays closed.
        let mut b = block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Char('n'), QStop::Header);
        assert_eq!(shown_id(&b), "Q2");
        assert!(!b.is_open());

        // With one question, n does nothing.
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        let before = b.view();
        assert_eq!(
            press(&mut b, KeyCode::Char('n'), opt_stop(0, 1)),
            BlockEffect::None
        );
        assert_eq!(
            press(&mut b, KeyCode::Char('N'), opt_stop(0, 1)),
            BlockEffect::None
        );
        assert_eq!(b.view(), before);
    }

    #[test]
    fn n_away_and_back_keeps_the_picks() {
        let mut b = open_block(vec![question("Q25", 1, 2), question("Q26", 1, 1)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        press(&mut b, KeyCode::Char('n'), QStop::Header);
        assert_eq!(b.view().unwrap().id, "Q26");
        assert_eq!(picked(&b), keys(&["b"]), "Q26 starts with its suggestion");
        press(&mut b, KeyCode::Char('N'), QStop::Header);
        assert_eq!(b.view().unwrap().id, "Q25");
        assert_eq!(picked(&b), keys(&["a", "b"]));
    }

    #[test]
    fn higher_version_re_presets_picks_and_sets_notice() {
        let mut b = open_block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        b.reconcile(queue(vec![question("Q1", 2, 2)]));
        let v = b.view().unwrap();
        assert_eq!(picked(&b), keys(&["b", "b"]));
        assert_eq!(
            v.notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
        assert!(v.open, "the question stays open");
        match press(&mut b, KeyCode::Enter, QStop::Action(0)) {
            BlockEffect::Send(out) => assert_eq!(out.version, 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn kept_picks_drop_when_the_version_rises() {
        let mut b = open_block(vec![question("Q25", 1, 2), question("Q26", 1, 1)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        press(&mut b, KeyCode::Char('n'), QStop::Header);
        b.reconcile(queue(vec![question("Q25", 2, 2), question("Q26", 1, 1)]));
        assert_eq!(b.view().unwrap().notice, None, "Q25 is not in view");
        press(&mut b, KeyCode::Char('N'), QStop::Header);
        assert_eq!(b.view().unwrap().id, "Q25");
        assert_eq!(picked(&b), keys(&["b", "b"]), "back to the suggestions");
        assert_eq!(b.view().unwrap().notice, None);
        assert_eq!(b.shown().unwrap().1, 2);
    }

    #[test]
    fn kept_picks_go_when_the_question_is_dismissed_or_answered() {
        let mut b = open_block(vec![question("Q25", 1, 2), question("Q26", 1, 1)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        press(&mut b, KeyCode::Char('n'), QStop::Header);
        b.reconcile(queue(vec![question("Q26", 1, 1)]));
        b.reconcile(queue(vec![question("Q25", 1, 2), question("Q26", 1, 1)]));
        assert_eq!(b.view().unwrap().id, "Q26");
        press(&mut b, KeyCode::Char('N'), QStop::Header);
        assert_eq!(picked(&b), keys(&["b", "b"]));

        let mut b = open_block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        b.sent("Q1", SendResult::Recorded);
        // Harold lists it again at the same version, as a stale read might.
        b.reconcile(queue(vec![question("Q2", 1, 1), question("Q1", 1, 1)]));
        press(&mut b, KeyCode::Char('n'), QStop::Header);
        assert_eq!(b.view().unwrap().id, "Q1");
        assert_eq!(picked(&b), keys(&["b"]), "the answered walk was not kept");
    }

    #[test]
    fn recorded_send_shows_next_question_open() {
        let mut b = open_block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        b.sent("Q1", SendResult::Recorded);
        let v = b.view().unwrap();
        assert_eq!((v.id.as_str(), v.position, v.waiting), ("Q2", 1, 1));
        assert!(v.open);
        assert_eq!(b.stops().len(), 1 + 3 + 4);
        b.sent("Q2", SendResult::Recorded);
        assert!(!b.is_visible());
        assert!(!b.is_open());
        assert!(b.stops().is_empty());
        // A question that comes later starts closed.
        b.reconcile(queue(vec![question("Q3", 1, 1)]));
        assert!(!b.view().unwrap().open);
    }

    #[test]
    fn not_waiting_shows_the_dismissal() {
        let mut b = block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        b.sent("Q1", SendResult::Recorded);
        b.sent("Q2", SendResult::NotWaiting);
        assert!(
            b.view()
                .unwrap()
                .notice
                .unwrap()
                .starts_with("Q2 dismissed: ")
        );
    }

    #[test]
    fn zero_decision_question_stops_and_sends() {
        let mut b = block(vec![question("Q1", 4, 0)]);
        assert_eq!(b.stops(), vec![QStop::Header]);
        press(&mut b, KeyCode::Char('l'), QStop::Header);
        assert_eq!(
            b.stops(),
            vec![
                QStop::Header,
                QStop::Action(0),
                QStop::Action(1),
                QStop::Action(2),
                QStop::Action(3)
            ]
        );
        assert_eq!(
            press(&mut b, KeyCode::Enter, QStop::Action(0)),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 4,
                kind: AnswerKind::Answer,
                picks: vec![],
                note: String::new(),
                send_now: true,
            })
        );
    }

    #[test]
    fn a_decision_with_no_options_sends_the_cursor_to_the_header() {
        let mut q = question("Q1", 1, 2);
        q.decisions[1].options.clear();
        let mut b = open_block(vec![q]);
        assert_eq!(
            press(&mut b, KeyCode::Enter, QStop::Action(0)),
            BlockEffect::Cursor(QStop::Header)
        );
        assert_eq!(b.stops().len(), 1 + 3 + 4);
    }

    #[test]
    fn o_opens_only_an_http_link_on_any_stop() {
        let mut q = question("Q1", 1, 1);
        let mut b = open_block(vec![q.clone()]);
        for stop in [QStop::Header, opt_stop(0, 0), QStop::Action(2)] {
            assert_eq!(
                press(&mut b, KeyCode::Char('o'), stop),
                BlockEffect::OpenLink("https://example.com/x".into())
            );
        }
        q.link = "http://example.com".into();
        let mut b = block(vec![q.clone()]);
        assert!(matches!(
            press(&mut b, KeyCode::Char('o'), QStop::Header),
            BlockEffect::OpenLink(_)
        ));
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "-a Calculator",
            "",
        ] {
            q.link = bad.into();
            let mut b = block(vec![q.clone()]);
            assert_eq!(
                press(&mut b, KeyCode::Char('o'), QStop::Header),
                BlockEffect::None,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_dismissal_removes_the_question_with_its_reason() {
        let mut b = block(vec![question("Q1", 1, 2), question("Q2", 1, 1)]);
        let mut q = queue(vec![question("Q2", 1, 1)]);
        q.dismissed = vec![("Q1".into(), "no longer needed".into())];
        b.reconcile(q);
        let v = b.view().unwrap();
        assert_eq!(v.id, "Q2");
        assert_eq!(v.notice.as_deref(), Some("Q1 dismissed: no longer needed"));

        let mut b = block(vec![question("Q1", 1, 1)]);
        b.reconcile(queue(vec![]));
        assert!(b.is_visible());
        assert!(b.stops().is_empty(), "a notice has no stops");
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 dismissed: withdrawn")
        );
        assert_eq!(
            press(&mut b, KeyCode::Esc, QStop::Header),
            BlockEffect::None
        );
        assert!(!b.is_visible());
        assert!(b.view().is_none());
    }

    #[test]
    fn a_dismissal_notice_with_an_empty_queue_goes_after_the_next_empty_list() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        let mut gone = queue(vec![]);
        gone.dismissed = vec![("Q1".into(), "no longer needed".into())];
        b.reconcile(gone.clone());
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 dismissed: no longer needed")
        );
        b.reconcile(gone);
        assert!(!b.is_visible(), "the block hides when nothing waits");
        assert!(b.view().is_none());
        b.reconcile(queue(vec![question("Q2", 1, 1)]));
        assert_eq!(b.view().unwrap().notice, None);
    }

    #[test]
    fn a_not_waiting_notice_stays_through_the_read_after_the_send() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        b.sent("Q1", SendResult::NotWaiting);
        b.reconcile(queue(vec![]));
        assert!(
            b.view()
                .unwrap()
                .notice
                .unwrap()
                .starts_with("Q1 dismissed: ")
        );
        b.reconcile(queue(vec![]));
        assert!(!b.is_visible());
    }

    #[test]
    fn any_key_clears_a_notice_on_a_block_with_no_question() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        b.reconcile(queue(vec![]));
        assert!(b.shows_notice_only());
        assert_eq!(
            press(&mut b, KeyCode::Char('j'), QStop::Header),
            BlockEffect::None
        );
        assert!(!b.is_visible());
        assert!(!b.shows_notice_only());
    }

    #[test]
    fn a_notice_beside_a_question_is_not_aged_by_listings() {
        let mut b = block(vec![question("Q1", 1, 2), question("Q2", 1, 1)]);
        let mut q = queue(vec![question("Q2", 1, 1)]);
        q.dismissed = vec![("Q1".into(), "no longer needed".into())];
        b.reconcile(q.clone());
        b.reconcile(q.clone());
        b.reconcile(q);
        let v = b.view().unwrap();
        assert_eq!(v.id, "Q2");
        assert_eq!(v.notice.as_deref(), Some("Q1 dismissed: no longer needed"));
    }

    #[test]
    fn a_reorder_keeps_the_current_question_and_its_picks() {
        let mut b = open_block(vec![question("Q1", 1, 2), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        b.reconcile(queue(vec![question("Q2", 1, 1), question("Q1", 1, 2)]));
        let v = b.view().unwrap();
        assert_eq!(v.id, "Q1");
        assert_eq!(v.position, 2);
        assert_eq!(picked(&b), keys(&["a", "b"]));
        assert_eq!(v.notice, None);
    }

    #[test]
    fn unavailable_keeps_the_picks_and_enter_retries() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        let first = press(&mut b, KeyCode::Enter, QStop::Action(0));
        assert!(matches!(first, BlockEffect::Send(_)));
        b.sent("Q1", SendResult::Unavailable);
        let v = b.view().unwrap();
        assert_eq!(
            v.notice.as_deref(),
            Some("Harold can't save answers right now")
        );
        assert_eq!(press(&mut b, KeyCode::Enter, QStop::Action(0)), first);
    }

    #[test]
    fn version_changed_on_send_re_presets_the_picks() {
        let mut b = open_block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        b.sent("Q1", SendResult::VersionChanged);
        assert_eq!(picked(&b), keys(&["b", "b"]));
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
    }

    #[test]
    fn a_suspended_block_keeps_its_note_and_open_state() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Enter, QStop::Action(2));
        type_text(&mut b, "abc");
        b.suspend();
        assert!(b.view().is_none());
        assert_eq!(
            press(&mut b, KeyCode::Char('x'), QStop::Header),
            BlockEffect::None
        );
        b.reconcile(queue(vec![question("Q1", 1, 1)]));
        let v = b.view().unwrap();
        assert!(v.open);
        assert_eq!(
            v.mode,
            Mode::Note {
                kind: NoteKind::Revise,
                text: "abc".into()
            }
        );
    }
    #[test]
    fn note_stop_names_the_row_the_open_note_belongs_to() {
        let mut b = open_block(vec![question("Q1", 1, 2)]);
        assert_eq!(b.note_stop(), None);
        press(&mut b, KeyCode::Char(' '), opt_stop(1, 2));
        assert_eq!(b.note_stop(), Some(opt_stop(1, 2)));
        assert_eq!(b.view().unwrap().note_stop, Some(opt_stop(1, 2)));
        press(&mut b, KeyCode::Esc, opt_stop(1, 2));
        assert_eq!(b.note_stop(), None);
        press(&mut b, KeyCode::Enter, QStop::Action(2));
        assert_eq!(b.note_stop(), Some(QStop::Action(2)));
        b.suspend();
        assert_eq!(b.note_stop(), None, "not while the rows are away");
        assert!(!b.note_open());
        b.reconcile(queue(vec![question("Q1", 1, 2)]));
        assert_eq!(b.note_stop(), Some(QStop::Action(2)));
        press(&mut b, KeyCode::Esc, QStop::Action(2));
        press(&mut b, KeyCode::Enter, QStop::Action(3));
        assert_eq!(b.note_stop(), Some(QStop::Action(3)));
    }

    fn instructions(id: &str, version: u32, steps: usize) -> QuestionView {
        QuestionView {
            id: id.into(),
            product: "prod".into(),
            spec: String::new(),
            sha: String::new(),
            link: "https://example.com/i".into(),
            intro: "do these".into(),
            version,
            decisions: vec![],
            title: format!("Title {id}"),
            steps: (1..=steps)
                .map(|n| StepView {
                    text: format!("step {n}"),
                    detail: format!("detail {n}"),
                })
                .collect(),
            ticked: vec![],
            picks: vec![],
        }
    }

    fn with_ticks(mut q: QuestionView, ticks: &[u32]) -> QuestionView {
        q.ticked = ticks.to_vec();
        q
    }

    fn send_of(effect: BlockEffect) -> Outgoing {
        match effect {
            BlockEffect::Send(out) => out,
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn the_instruction_actions_are_done_and_ignore() {
        assert_eq!(INSTRUCTION_ACTIONS, ["done", "ignore"]);
    }

    #[test]
    fn closed_block_counts_both_kinds() {
        let b = block(vec![
            instructions("I1", 1, 2),
            question("Q1", 1, 1),
            question("Q2", 1, 1),
        ]);
        assert_eq!(b.todos_waiting(), 3);
        let view = b.view().unwrap();
        assert_eq!((view.waiting, view.open), (3, false));
        assert_eq!(b.stops(), vec![QStop::Header]);
    }

    #[test]
    fn open_stops_for_each_kind_in_order_with_the_other_todos() {
        let mut b = open_block(vec![
            instructions("I1", 1, 3),
            question("Q1", 1, 1),
            instructions("I2", 1, 1),
        ]);
        let mut want = vec![QStop::Header];
        want.extend((0..3).map(QStop::Step));
        want.extend((0..2).map(QStop::Action));
        want.extend([QStop::Other(1), QStop::Other(2)]);
        assert_eq!(b.stops(), want);

        press(&mut b, KeyCode::Char('l'), QStop::Other(1));
        let mut want = vec![QStop::Header];
        want.extend((0..3).map(|option| opt_stop(0, option)));
        want.extend((0..4).map(QStop::Action));
        want.extend([QStop::Other(0), QStop::Other(2)]);
        assert_eq!(b.stops(), want);
    }

    #[test]
    fn space_on_a_step_asks_to_tick_then_untick() {
        let mut b = open_block(vec![with_ticks(instructions("I1", 4, 3), &[2])]);
        assert_eq!(
            press(&mut b, KeyCode::Char(' '), QStop::Step(0)),
            BlockEffect::Tick {
                id: "I1".into(),
                version: 4,
                step: 1,
                ticked: true
            }
        );
        assert_eq!(
            press(&mut b, KeyCode::Char(' '), QStop::Step(1)),
            BlockEffect::Tick {
                id: "I1".into(),
                version: 4,
                step: 2,
                ticked: false
            }
        );
        // Space changes nothing locally.
        assert_eq!(b.view().unwrap().ticked_count, 1);
        assert_eq!(
            press(&mut b, KeyCode::Char(' '), QStop::Action(0)),
            BlockEffect::None
        );
    }

    #[test]
    fn ticked_applies_locally_on_recorded() {
        let mut b = open_block(vec![instructions("I1", 1, 3)]);
        b.ticked("I1", 2, true, SendResult::Recorded);
        b.ticked("I1", 3, true, SendResult::Recorded);
        let ticks = |b: &QuestionBlock| -> Vec<bool> {
            b.view().unwrap().steps.iter().map(|s| s.ticked).collect()
        };
        assert_eq!(ticks(&b), vec![false, true, true]);
        b.ticked("I1", 2, false, SendResult::Recorded);
        assert_eq!(ticks(&b), vec![false, false, true]);
        assert_eq!(b.view().unwrap().ticked_count, 1);
    }

    #[test]
    fn a_recorded_tick_clears_a_stale_notice() {
        let mut b = open_block(vec![instructions("I1", 1, 2)]);
        b.ticked("I1", 1, true, SendResult::Unavailable);
        assert!(b.view().unwrap().notice.is_some());
        b.ticked("I1", 1, true, SendResult::Recorded);
        assert_eq!(b.view().unwrap().notice, None);
    }

    #[test]
    fn ticked_unavailable_changes_nothing_and_says_so() {
        let mut b = open_block(vec![instructions("I1", 1, 2)]);
        b.ticked("I1", 1, true, SendResult::Unavailable);
        assert_eq!(b.view().unwrap().ticked_count, 0);
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some(UNAVAILABLE_NOTICE)
        );
    }

    #[test]
    fn ticked_version_changed_sets_the_notice() {
        let mut b = open_block(vec![instructions("I1", 1, 2)]);
        b.ticked("I1", 1, true, SendResult::VersionChanged);
        let view = b.view().unwrap();
        assert_eq!(view.ticked_count, 0);
        assert_eq!(
            view.notice.as_deref(),
            Some("I1 changed, ticks kept for unchanged steps")
        );
    }

    #[test]
    fn ticked_not_waiting_drops_the_todo_and_opens_the_next() {
        let mut b = open_block(vec![instructions("I1", 1, 2), question("Q1", 1, 1)]);
        b.ticked("I1", 1, true, SendResult::NotWaiting);
        let view = b.view().unwrap();
        assert_eq!(view.id, "Q1");
        assert!(view.open);
        assert_eq!(view.notice.as_deref(), Some("I1 dismissed: withdrawn"));
    }

    #[test]
    fn done_sends_a_done_answer_with_send_now() {
        let mut b = open_block(vec![instructions("I1", 3, 2)]);
        let out = send_of(press(&mut b, KeyCode::Enter, QStop::Action(0)));
        assert_eq!(
            out,
            Outgoing {
                id: "I1".into(),
                version: 3,
                kind: AnswerKind::Done,
                picks: vec![],
                note: String::new(),
                send_now: true,
            }
        );
    }

    #[test]
    fn ignore_sends_an_ignore_answer() {
        let mut b = open_block(vec![instructions("I1", 3, 2)]);
        let out = send_of(press(&mut b, KeyCode::Enter, QStop::Action(1)));
        assert_eq!((out.kind, out.send_now), (AnswerKind::Ignore, true));
        assert!(out.picks.is_empty() && out.note.is_empty());
    }

    #[test]
    fn done_and_ignore_rows_have_no_other_keys() {
        let mut b = open_block(vec![instructions("I1", 1, 2)]);
        for stop in [QStop::Action(0), QStop::Action(1)] {
            for c in ['r', 'x', 'q', ' '] {
                assert_eq!(press(&mut b, KeyCode::Char(c), stop), BlockEffect::None);
            }
            assert!(!b.note_open());
        }
        // No row past ignore.
        assert_eq!(
            press(&mut b, KeyCode::Enter, QStop::Action(2)),
            BlockEffect::None
        );
        assert_eq!(
            b.stops()
                .iter()
                .filter(|s| matches!(s, QStop::Action(_)))
                .count(),
            2
        );
    }

    #[test]
    fn l_on_another_todo_opens_it_and_closes_the_open_one_keeping_question_picks() {
        let mut b = open_block(vec![question("Q1", 1, 1), instructions("I1", 1, 2)]);
        press(&mut b, KeyCode::Char(' '), opt_stop(0, 0));
        assert_eq!(picked(&b), keys(&["a"]));
        for code in [KeyCode::Char('l'), KeyCode::Right, KeyCode::Enter] {
            let mut b = b.clone();
            assert_eq!(
                press(&mut b, code, QStop::Other(1)),
                BlockEffect::Cursor(QStop::Header),
                "{code:?}"
            );
            let view = b.view().unwrap();
            assert_eq!(
                (view.id.as_str(), view.kind, view.open),
                ("I1", TodoKind::Instructions, true)
            );
            assert_eq!(view.others.len(), 1);
            assert_eq!(view.others[0].id, "Q1");
            // Back: the question's pick is still there.
            press(&mut b, code, QStop::Other(0));
            assert_eq!(b.view().unwrap().id, "Q1");
            assert_eq!(picked(&b), keys(&["a"]), "{code:?}");
        }
    }

    #[test]
    fn n_wraps_across_kinds() {
        let mut b = block(vec![
            instructions("I1", 1, 2),
            question("Q1", 1, 1),
            instructions("I2", 1, 1),
        ]);
        let id = |b: &QuestionBlock| b.view().unwrap().id;
        for want in ["Q1", "I2", "I1"] {
            assert_eq!(
                press(&mut b, KeyCode::Char('n'), QStop::Header),
                BlockEffect::Cursor(QStop::Header)
            );
            assert_eq!(id(&b), want);
        }
        press(&mut b, KeyCode::Char('N'), QStop::Header);
        assert_eq!(id(&b), "I2");
    }

    #[test]
    fn a_higher_instructions_version_keeps_ticks_and_says_so() {
        let mut b = open_block(vec![with_ticks(instructions("I1", 1, 3), &[1])]);
        b.reconcile(queue(vec![with_ticks(instructions("I1", 2, 3), &[1, 3])]));
        let view = b.view().unwrap();
        assert!(view.open);
        assert_eq!(
            view.notice.as_deref(),
            Some("I1 changed, ticks kept for unchanged steps")
        );
        assert_eq!(view.ticked_count, 2);
        // A question still clears its picks.
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        b.reconcile(queue(vec![question("Q1", 2, 1)]));
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
    }

    #[test]
    fn instructions_put_again_after_done_or_ignore_say_ticks_were_kept() {
        {
            // `sent` is the same for `done` and `ignore`: both are recorded answers.
            let mut b = open_block(vec![instructions("I1", 1, 2), question("Q1", 1, 1)]);
            b.sent("I1", SendResult::Recorded);
            b.reconcile(queue(vec![question("Q1", 1, 1)]));
            assert_eq!(b.view().unwrap().notice, None);
            b.reconcile(queue(vec![
                with_ticks(instructions("I1", 2, 2), &[1]),
                question("Q1", 1, 1),
            ]));
            assert_eq!(
                b.view().unwrap().notice.as_deref(),
                Some("I1 changed, ticks kept for unchanged steps")
            );
            // The id is forgotten: a later new version of it follows the usual rules.
            assert!(b.answered_instructions.is_empty());
        }
    }

    #[test]
    fn an_unrelated_new_instruction_shows_no_put_again_notice() {
        let mut b = open_block(vec![instructions("I1", 1, 2), question("Q1", 1, 1)]);
        b.sent("I1", SendResult::Recorded);
        b.reconcile(queue(vec![question("Q1", 1, 1), instructions("I2", 1, 2)]));
        assert_eq!(b.view().unwrap().notice, None);
    }

    #[test]
    fn a_question_answered_and_put_again_gets_no_ticks_notice() {
        let mut b = open_block(vec![question("Q1", 1, 1)]);
        b.sent("Q1", SendResult::Recorded);
        b.reconcile(queue(vec![question("Q1", 2, 1)]));
        assert_eq!(b.view().and_then(|v| v.notice), None);
    }

    #[test]
    fn o_h_left_and_esc_do_nothing_on_another_todos_row() {
        let mut b = open_block(vec![instructions("I1", 1, 2), question("Q1", 1, 1)]);
        for code in [
            KeyCode::Char('o'),
            KeyCode::Char('h'),
            KeyCode::Left,
            KeyCode::Esc,
        ] {
            assert_eq!(press(&mut b, code, QStop::Other(1)), BlockEffect::None);
            assert!(b.is_open(), "{code:?} closed the block");
            assert_eq!(b.view().unwrap().id, "I1");
        }
        // `l` still opens that todo.
        assert_eq!(
            press(&mut b, KeyCode::Char('l'), QStop::Other(1)),
            BlockEffect::Cursor(QStop::Header)
        );
        assert_eq!(b.view().unwrap().id, "Q1");
    }

    #[test]
    fn a_suspended_instructions_block_takes_the_listed_ticks_back() {
        let mut b = open_block(vec![instructions("I1", 1, 4)]);
        assert!(b.stops().contains(&QStop::Step(2)));
        b.suspend();
        assert!(b.view().is_none());
        b.reconcile(queue(vec![with_ticks(instructions("I1", 1, 4), &[1, 3])]));
        let v = b.view().unwrap();
        assert!(v.open);
        assert_eq!(v.ticked_count, 2);
        assert_eq!(v.notice, None);
        assert!(v.steps[0].ticked && !v.steps[1].ticked && v.steps[2].ticked);
        assert!(
            b.stops().contains(&QStop::Step(2)),
            "the step is still there"
        );
        // A key on that step acts on it at once.
        assert!(matches!(
            press(&mut b, KeyCode::Char(' '), QStop::Step(2)),
            BlockEffect::Tick {
                step: 3,
                ticked: false,
                ..
            }
        ));
    }

    #[test]
    fn a_dismissed_instruction_reads_dismissed_reason() {
        let mut b = open_block(vec![instructions("I1", 1, 2), question("Q1", 1, 1)]);
        let mut q = queue(vec![question("Q1", 1, 1)]);
        q.dismissed = vec![("I1".into(), "superseded".into())];
        b.reconcile(q);
        let view = b.view().unwrap();
        assert_eq!(view.notice.as_deref(), Some("I1 dismissed: superseded"));
        assert_eq!(view.id, "Q1");
    }

    #[test]
    fn sent_recorded_opens_the_next_todo_open() {
        let mut b = open_block(vec![instructions("I1", 1, 2), question("Q1", 1, 1)]);
        send_of(press(&mut b, KeyCode::Enter, QStop::Action(0)));
        b.sent("I1", SendResult::Recorded);
        let view = b.view().unwrap();
        assert_eq!(
            (view.id.as_str(), view.kind, view.open),
            ("Q1", TodoKind::Question, true)
        );
        assert_eq!(view.waiting, 1);
        assert_eq!(b.stops()[0], QStop::Header);
    }

    #[test]
    fn all_ticked_flag() {
        let mut b = open_block(vec![instructions("I1", 1, 2)]);
        assert!(!b.view().unwrap().all_ticked);
        b.ticked("I1", 1, true, SendResult::Recorded);
        assert!(!b.view().unwrap().all_ticked);
        b.ticked("I1", 2, true, SendResult::Recorded);
        assert!(b.view().unwrap().all_ticked);
        assert!(
            !open_block(vec![question("Q1", 1, 1)])
                .view()
                .unwrap()
                .all_ticked
        );
    }

    #[test]
    fn view_names_kind_title_and_ticked_count() {
        let b = open_block(vec![
            with_ticks(instructions("I1", 1, 4), &[1, 3, 9]),
            question("Q1", 1, 1),
            with_ticks(instructions("I2", 1, 3), &[2]),
        ]);
        let view = b.view().unwrap();
        assert_eq!(view.kind, TodoKind::Instructions);
        assert_eq!(view.title, "Title I1");
        assert_eq!((view.ticked_count, view.steps.len()), (2, 4));
        assert_eq!((view.position, view.waiting), (1, 3));
        assert_eq!(view.steps[0].detail, "detail 1");
        assert_eq!(
            view.others,
            vec![
                OtherRow {
                    index: 1,
                    kind: TodoKind::Question,
                    id: "Q1".into(),
                    ticked_count: 0,
                    steps_total: 0,
                    product: "prod".into(),
                    label: "spec".into(),
                },
                OtherRow {
                    index: 2,
                    kind: TodoKind::Instructions,
                    id: "I2".into(),
                    ticked_count: 1,
                    steps_total: 3,
                    product: "prod".into(),
                    label: "Title I2".into(),
                },
            ]
        );
    }

    #[test]
    fn o_opens_the_instructions_link() {
        let mut b = open_block(vec![instructions("I1", 1, 1)]);
        assert_eq!(
            press(&mut b, KeyCode::Char('o'), QStop::Step(0)),
            BlockEffect::OpenLink("https://example.com/i".into())
        );
    }
}
