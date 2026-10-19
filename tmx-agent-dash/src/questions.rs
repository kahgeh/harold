//! The question block's state machine: the keys, the picks and the notices.
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
    /// The option the chief or product manager suggests; the highlight starts on it.
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
    pub deferred: bool,
    pub decisions: Vec<DecisionView>,
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
    FreeText,
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
    /// The summary's "send now" mark; a revise or reject note has none, so it is false.
    pub send_now: bool,
}

/// What a note is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Revise,
    Reject,
    FreeText,
}

/// What the block is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Choosing,
    Summary,
    Note {
        kind: NoteKind,
        text: String,
    },
}

/// What the runtime does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockEffect {
    None,
    /// Give the focus back to the agent list.
    Leave,
    OpenLink(String),
    Defer(String),
    Send(Outgoing),
}

/// The outcome of a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendResult {
    Recorded,
    VersionChanged,
    NotWaiting,
    Unavailable,
}

/// Everything the drawing needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockView {
    pub id: String,
    pub product: String,
    pub spec: String,
    pub intro: String,
    /// The current question's place in the listed order, from 1.
    pub position: usize,
    pub waiting: u32,
    pub ask: Option<String>,
    pub options: Vec<OptionView>,
    /// The highlighted row: an option below `options.len()`, else an [`ANSWER_ACTIONS`] row.
    pub highlighted: usize,
    pub mode: Mode,
    /// The labels picked so far.
    pub picks: Vec<String>,
    /// One `<ask> — <label>` line per picked decision.
    pub summary: Vec<String>,
    /// The summary's "send now" mark.
    pub send_now: bool,
    pub notice: Option<String>,
}

/// The walk through a question that is not in view, kept for its version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    version: u32,
    decision: usize,
    picks: Vec<PickView>,
    highlights: Vec<usize>,
    /// Choosing or Summary.
    mode: Mode,
    send_now: bool,
}

/// The block's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuestionBlock {
    queue: Queue,
    current: Option<String>,
    version: u32,
    decision: usize,
    /// One pick per decision, in decision order. Going back with `h` keeps them, so
    /// there can be picks for decisions after the current one.
    picks: Vec<PickView>,
    /// Each decision's highlighted row, kept while moving between decisions.
    highlights: Vec<usize>,
    mode: Mode,
    /// The "send now" mark for the answer on the summary. It is kept through going back
    /// and returning, and goes with a restart or the recorded answer.
    send_now: bool,
    /// The walks through listed questions other than the current one, by id. One goes
    /// when its question is no longer listed or is listed at a higher version.
    kept: BTreeMap<String, Kept>,
    notice: Option<String>,
    /// The notice stood on a block with no question through one listing already; the next
    /// listing that still finds no question clears it, and the block hides.
    notice_listed: bool,
    /// Hidden after a failed read or a lost stream, with the picks, mode and note kept
    /// for the next listing.
    suspended: bool,
}

/// The notice when Harold could not save an answer or a deferral.
pub const UNAVAILABLE_NOTICE: &str = "Harold can't save answers right now";

/// The rows under every decision's options. Highlighted row `options.len() + i` is action
/// `i`: `later` defers the question, and the other two open a note to send with the picks.
pub const ANSWER_ACTIONS: [&str; 3] = ["later", "revise\u{2026}", "reject\u{2026}"];

/// The longest note the block takes; Harold refuses a field of 2,000 characters.
const NOTE_LIMIT: usize = 1_999;

fn changed_notice(id: &str) -> String {
    format!("{id} changed, your picks were cleared")
}

fn starting_highlight(options: &[OptionView]) -> usize {
    options.iter().position(|o| o.suggested).unwrap_or(0)
}

fn is_web_link(link: &str) -> bool {
    link.starts_with("https://") || link.starts_with("http://")
}

impl QuestionBlock {
    /// Takes the latest listing. The current question stays while it is listed at the same
    /// version; a higher version restarts it; a missing one is replaced with a notice.
    /// Walks kept for other questions go when those are missing or at a higher version.
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
    }

    fn follow_current(&mut self) {
        let Some(id) = self.current.clone() else {
            self.select_first();
            return;
        };
        match self.queue.questions.iter().find(|q| q.id == id) {
            Some(q) if q.version > self.version => {
                self.version = q.version;
                self.restart();
                self.set_notice(changed_notice(&id));
            }
            Some(_) => {}
            None => {
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
    /// question leaves the queue before the next one shows, so its walk is not kept.
    pub fn sent(&mut self, id: &str, result: SendResult) {
        if self.current.as_deref() != Some(id) {
            return;
        }
        match result {
            SendResult::Recorded => {
                self.queue.questions.retain(|q| q.id != id);
                self.queue.waiting = self.queue.waiting.saturating_sub(1);
                self.send_now = false;
                self.clear_notice();
                self.select_first();
            }
            SendResult::VersionChanged => {
                self.restart();
                self.set_notice(changed_notice(id));
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
    pub fn view(&self) -> Option<BlockView> {
        if !self.is_visible() {
            return None;
        }
        let Some(q) = self.question() else {
            return Some(BlockView {
                id: String::new(),
                product: String::new(),
                spec: String::new(),
                intro: String::new(),
                position: 0,
                waiting: self.queue.waiting,
                ask: None,
                options: vec![],
                highlighted: 0,
                mode: Mode::Choosing,
                picks: vec![],
                summary: vec![],
                send_now: false,
                notice: self.notice.clone(),
            });
        };
        let decision = q.decisions.get(self.decision);
        let picks: Vec<String> = self.picks.iter().map(|p| self.pick_label(q, p)).collect();
        let summary = self
            .picks
            .iter()
            .zip(&picks)
            .map(|(p, label)| format!("{} \u{2014} {label}", q.decisions[p.decision as usize].ask))
            .collect();
        Some(BlockView {
            id: q.id.clone(),
            product: q.product.clone(),
            spec: q.spec.clone(),
            intro: q.intro.clone(),
            position: self
                .queue
                .questions
                .iter()
                .position(|x| x.id == q.id)
                .map_or(1, |i| i + 1),
            waiting: self.queue.waiting,
            ask: decision.map(|d| d.ask.clone()),
            options: decision.map(|d| d.options.clone()).unwrap_or_default(),
            highlighted: self.highlights.get(self.decision).copied().unwrap_or(0),
            mode: self.mode.clone(),
            picks,
            summary,
            send_now: self.send_now,
            notice: self.notice.clone(),
        })
    }

    /// Handles one key and says what the runtime should do.
    pub fn handle_key(&mut self, code: KeyCode) -> BlockEffect {
        if self.suspended {
            return BlockEffect::None;
        }
        if self.question().is_none() {
            // Any key clears a notice on a block with no question, so the block hides.
            self.clear_notice();
            if code == KeyCode::Esc {
                return BlockEffect::Leave;
            }
            return BlockEffect::None;
        }
        self.clear_notice();
        match self.mode.clone() {
            Mode::Choosing => self.choosing_key(code),
            Mode::Summary => self.summary_key(code),
            Mode::Note { kind, text } => self.note_key(code, kind, text),
        }
    }

    fn choosing_key(&mut self, code: KeyCode) -> BlockEffect {
        let Some(q) = self.question().cloned() else {
            return BlockEffect::None;
        };
        let options = &q.decisions[self.decision].options;
        let last_row = options.len() + ANSWER_ACTIONS.len() - 1;
        let highlighted = self.highlights[self.decision];
        match code {
            KeyCode::Char('k') | KeyCode::Up => {
                self.highlights[self.decision] = highlighted.saturating_sub(1);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.highlights[self.decision] = (highlighted + 1).min(last_row);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                // An action row is not an answer to move on with. A free option moves on
                // past text it already has, and otherwise opens its text box as Enter does.
                let Some(option) = options.get(highlighted) else {
                    return BlockEffect::None;
                };
                if !option.free {
                    self.set_pick(&q, option.key.clone(), String::new());
                } else if self.kept_text(option).is_some() {
                    self.advance(&q);
                } else {
                    self.open_note(NoteKind::FreeText);
                }
            }
            KeyCode::Char('h') | KeyCode::Left => self.decision = self.decision.saturating_sub(1),
            KeyCode::Enter => {
                let Some(option) = options.get(highlighted) else {
                    return self.answer_action(highlighted - options.len(), q.id);
                };
                if option.free {
                    let text = self.kept_text(option).unwrap_or_default();
                    self.mode = Mode::Note {
                        kind: NoteKind::FreeText,
                        text,
                    };
                } else {
                    self.set_pick(&q, option.key.clone(), String::new());
                }
            }
            KeyCode::Char('o') if is_web_link(&q.link) => return BlockEffect::OpenLink(q.link),
            KeyCode::Char('n') => self.step(1),
            KeyCode::Char('N') => self.step(-1),
            KeyCode::Esc => return BlockEffect::Leave,
            _ => {}
        }
        BlockEffect::None
    }

    /// Enter on [`ANSWER_ACTIONS`] row `action`.
    fn answer_action(&mut self, action: usize, id: String) -> BlockEffect {
        match ANSWER_ACTIONS.get(action).copied() {
            Some("later") => {
                self.defer_locally(&id);
                BlockEffect::Defer(id)
            }
            Some("revise\u{2026}") => {
                self.open_note(NoteKind::Revise);
                BlockEffect::None
            }
            Some("reject\u{2026}") => {
                self.open_note(NoteKind::Reject);
                BlockEffect::None
            }
            _ => BlockEffect::None,
        }
    }

    /// The text of this decision's pick, when `option` is that pick.
    fn kept_text(&self, option: &OptionView) -> Option<String> {
        self.picks
            .get(self.decision)
            .filter(|p| p.key == option.key)
            .map(|p| p.text.clone())
    }

    fn summary_key(&mut self, code: KeyCode) -> BlockEffect {
        let Some(q) = self.question().cloned() else {
            return BlockEffect::None;
        };
        match code {
            KeyCode::Enter => {
                let last_free = self
                    .picks
                    .iter()
                    .rev()
                    .find(|p| self.is_free(&q, p))
                    .map(|p| p.text.clone());
                let (kind, note) = match last_free {
                    Some(text) => (AnswerKind::FreeText, text),
                    None => (AnswerKind::Answer, String::new()),
                };
                return BlockEffect::Send(Outgoing {
                    id: q.id,
                    version: self.version,
                    kind,
                    picks: self.picks.clone(),
                    note,
                    send_now: self.send_now,
                });
            }
            // The summary stands on the last decision, so going back shows it again.
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => self.mode = Mode::Choosing,
            KeyCode::Char('o') if is_web_link(&q.link) => return BlockEffect::OpenLink(q.link),
            KeyCode::Char('s') => self.send_now = !self.send_now,
            KeyCode::Char('n') => self.step(1),
            KeyCode::Char('N') => self.step(-1),
            _ => {}
        }
        BlockEffect::None
    }

    /// Shows the listed question `by` places from the current one, wrapping over the
    /// listed order. With one question listed, nothing changes.
    fn step(&mut self, by: isize) {
        let listed = &self.queue.questions;
        let Some(at) = listed
            .iter()
            .position(|q| Some(&q.id) == self.current.as_ref())
        else {
            return;
        };
        let next = (at as isize + by).rem_euclid(listed.len() as isize) as usize;
        if next != at {
            let id = listed[next].id.clone();
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
                return BlockEffect::None;
            }
            KeyCode::Enter => {
                let typed = text.trim().to_string();
                if typed.is_empty() {
                    return BlockEffect::None;
                }
                match kind {
                    NoteKind::FreeText => {
                        let key = q.decisions[self.decision].options
                            [self.highlights[self.decision]]
                            .key
                            .clone();
                        self.set_pick(&q, key, typed);
                    }
                    NoteKind::Revise | NoteKind::Reject => {
                        let kind = if kind == NoteKind::Revise {
                            AnswerKind::Revise
                        } else {
                            AnswerKind::Reject
                        };
                        return BlockEffect::Send(Outgoing {
                            id: q.id,
                            version: self.version,
                            kind,
                            picks: self.picks.clone(),
                            note: typed,
                            send_now: false,
                        });
                    }
                }
                return BlockEffect::None;
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

    /// Records the current decision's pick, replacing an earlier one, then moves on.
    fn set_pick(&mut self, q: &QuestionView, key: String, text: String) {
        let pick = PickView {
            decision: self.decision as u32,
            key,
            text,
        };
        match self.picks.get_mut(self.decision) {
            Some(kept) => *kept = pick,
            None => self.picks.push(pick),
        }
        self.advance(q);
    }

    /// Moves to the next decision, or to the summary after the last one.
    fn advance(&mut self, q: &QuestionView) {
        if self.decision + 1 < q.decisions.len() {
            self.decision += 1;
            self.mode = Mode::Choosing;
        } else {
            self.mode = Mode::Summary;
        }
    }

    fn defer_locally(&mut self, id: &str) {
        if let Some(index) = self.queue.questions.iter().position(|q| q.id == id) {
            let mut q = self.queue.questions.remove(index);
            q.deferred = true;
            self.queue.questions.push(q);
        }
        self.select_first();
    }

    fn question(&self) -> Option<&QuestionView> {
        let id = self.current.as_deref()?;
        self.queue.questions.iter().find(|q| q.id == id)
    }

    fn is_free(&self, q: &QuestionView, pick: &PickView) -> bool {
        q.decisions[pick.decision as usize]
            .options
            .iter()
            .any(|o| o.key == pick.key && o.free)
    }

    fn pick_label(&self, q: &QuestionView, pick: &PickView) -> String {
        if self.is_free(q, pick) {
            return format!("\"{}\"", pick.text);
        }
        q.decisions[pick.decision as usize]
            .options
            .iter()
            .find(|o| o.key == pick.key)
            .map(|o| o.label.clone())
            .unwrap_or_default()
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

    /// Decision 1 of the current question, with no picks.
    fn restart(&mut self) {
        self.decision = 0;
        self.picks.clear();
        self.send_now = false;
        self.mode = Mode::Choosing;
        self.highlights = self.question().map_or(vec![], |q| {
            q.decisions
                .iter()
                .map(|d| starting_highlight(&d.options))
                .collect()
        });
    }

    /// Makes the queue's first question current, or none.
    fn select_first(&mut self) {
        match self.queue.questions.first() {
            Some(q) => {
                let id = q.id.clone();
                self.show(&id);
            }
            None => {
                self.current = None;
                self.restart();
            }
        }
    }

    /// Makes listed question `id` current. The current question's walk is kept while it
    /// is still listed, and `id`'s kept walk comes back when it was made on the listed
    /// version; otherwise `id` starts at decision 1.
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
        if let Some(current) = self.question().map(|q| q.id.clone()) {
            let mode = match self.mode {
                Mode::Summary => Mode::Summary,
                _ => Mode::Choosing,
            };
            let kept = Kept {
                version: self.version,
                decision: self.decision,
                picks: std::mem::take(&mut self.picks),
                highlights: std::mem::take(&mut self.highlights),
                mode,
                send_now: self.send_now,
            };
            self.kept.insert(current, kept);
        }
        self.current = Some(id.to_string());
        self.version = version;
        match self.kept.remove(id) {
            Some(kept) if kept.version == version => {
                self.decision = kept.decision;
                self.picks = kept.picks;
                self.highlights = kept.highlights;
                self.mode = kept.mode;
                self.send_now = kept.send_now;
            }
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
            deferred: false,
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

    fn press(block: &mut QuestionBlock, code: KeyCode) -> BlockEffect {
        block.handle_key(code)
    }

    fn type_text(block: &mut QuestionBlock, text: &str) {
        for c in text.chars() {
            press(block, KeyCode::Char(c));
        }
    }

    fn pick(decision: u32, key: &str, text: &str) -> PickView {
        PickView {
            decision,
            key: key.into(),
            text: text.into(),
        }
    }

    fn press_times(block: &mut QuestionBlock, code: KeyCode, times: usize) {
        for _ in 0..times {
            press(block, code);
        }
    }

    fn highlighted(block: &QuestionBlock) -> usize {
        block.view().unwrap().highlighted
    }

    /// Highlights action row `action` of the current decision and presses Enter on it.
    fn open_action(block: &mut QuestionBlock, action: usize) -> BlockEffect {
        let options = block.view().unwrap().options.len();
        press_times(block, KeyCode::Char('k'), options + ANSWER_ACTIONS.len());
        press_times(block, KeyCode::Char('j'), options + action);
        assert_eq!(highlighted(block), options + action);
        press(block, KeyCode::Enter)
    }

    #[test]
    fn the_answer_actions_are_later_revise_and_reject() {
        assert_eq!(
            ANSWER_ACTIONS,
            ["later", "revise\u{2026}", "reject\u{2026}"]
        );
    }

    #[test]
    fn j_and_k_move_over_options_then_the_three_actions() {
        for (down, up) in [
            (KeyCode::Char('j'), KeyCode::Char('k')),
            (KeyCode::Down, KeyCode::Up),
        ] {
            let mut b = block(vec![question("Q1", 1, 1)]);
            let v = b.view().unwrap();
            assert_eq!(v.highlighted, 1, "starts on the suggested option");
            assert_eq!(v.options.len(), 3, "the options are the real ones only");
            assert_eq!((v.position, v.waiting), (1, 1));
            press_times(&mut b, down, 2);
            assert_eq!(highlighted(&b), 3, "the later row");
            press_times(&mut b, down, 2);
            assert_eq!(highlighted(&b), 5, "the reject row");
            press(&mut b, down);
            assert_eq!(highlighted(&b), 5, "stops at the last action");
            press_times(&mut b, up, 9);
            assert_eq!(highlighted(&b), 0, "stops at the first option");
            assert_eq!(b.view().unwrap().mode, Mode::Choosing);
        }
    }

    #[test]
    fn l_picks_the_highlighted_option_and_h_goes_back_keeping_it() {
        for (next, back) in [
            (KeyCode::Char('l'), KeyCode::Char('h')),
            (KeyCode::Right, KeyCode::Left),
        ] {
            let mut b = block(vec![question("Q1", 1, 2)]);
            press(&mut b, KeyCode::Char('k'));
            assert_eq!(press(&mut b, next), BlockEffect::None);
            let v = b.view().unwrap();
            assert_eq!(v.picks, vec!["label a".to_string()]);
            assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
            assert_eq!(v.highlighted, 1, "decision 2 starts on its suggestion");
            assert_eq!(press(&mut b, back), BlockEffect::None);
            let v = b.view().unwrap();
            assert_eq!(v.ask.as_deref(), Some("Ask 1?"));
            assert_eq!(v.picks, vec!["label a".to_string()]);
            assert_eq!(v.highlighted, 0);
            assert_eq!(v.mode, Mode::Choosing);
            assert_eq!(press(&mut b, back), BlockEffect::None);
            assert_eq!(b.view().unwrap(), v, "h on decision 1 changes nothing");
        }
    }

    #[test]
    fn the_highlight_is_remembered_per_decision() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Char('l'));
        press(&mut b, KeyCode::Char('k'));
        press(&mut b, KeyCode::Char('h'));
        assert_eq!(highlighted(&b), 1, "decision 1 keeps its own highlight");
        press(&mut b, KeyCode::Char('l'));
        let v = b.view().unwrap();
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.highlighted, 0);
        press(&mut b, KeyCode::Enter);
        let v = b.view().unwrap();
        assert_eq!(v.mode, Mode::Summary);
        assert_eq!(v.picks, vec!["label b".to_string(), "label a".to_string()]);
    }

    #[test]
    fn each_decision_stops_at_its_own_last_action() {
        let mut q = question("Q1", 1, 2);
        q.decisions[1].options.pop();
        let mut b = block(vec![q]);
        press_times(&mut b, KeyCode::Char('j'), 9);
        assert_eq!(highlighted(&b), 5);
        press(&mut b, KeyCode::Char('k'));
        press(&mut b, KeyCode::Char('k'));
        press(&mut b, KeyCode::Char('k'));
        press(&mut b, KeyCode::Char('k'));
        press(&mut b, KeyCode::Char('l'));
        let v = b.view().unwrap();
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.options.len(), 2);
        press_times(&mut b, KeyCode::Char('j'), 9);
        assert_eq!(highlighted(&b), 4, "two options and three actions");
        press(&mut b, KeyCode::Char('h'));
        assert_eq!(highlighted(&b), 1, "decision 1 kept its highlight");
    }

    #[test]
    fn l_on_an_action_row_does_nothing() {
        for (action, label) in ANSWER_ACTIONS.iter().enumerate() {
            let mut b = block(vec![question("Q1", 1, 2)]);
            press_times(&mut b, KeyCode::Char('j'), 2 + action);
            let before = b.view().unwrap();
            assert_eq!(before.highlighted, 3 + action);
            assert_eq!(press(&mut b, KeyCode::Char('l')), BlockEffect::None);
            assert_eq!(b.view().unwrap(), before, "{label}");
        }
    }

    #[test]
    fn later_on_any_decision_defers() {
        let mut b = block(vec![question("Q1", 1, 2), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.view().unwrap().ask.as_deref(), Some("Ask 2?"));
        assert_eq!(open_action(&mut b, 0), BlockEffect::Defer("Q1".into()));
        assert_eq!(
            b.view().unwrap().id,
            "Q2",
            "the next waiting question shows"
        );
    }

    #[test]
    fn revise_and_reject_rows_open_a_note_that_blank_enter_cannot_send() {
        let mut b = block(vec![question("Q1", 2, 2)]);
        assert_eq!(open_action(&mut b, 1), BlockEffect::None);
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::Revise,
                text: String::new()
            }
        );
        assert_eq!(
            press(&mut b, KeyCode::Enter),
            BlockEffect::None,
            "blank sends nothing"
        );
        type_text(&mut b, "fixx");
        press(&mut b, KeyCode::Backspace);
        assert_eq!(
            press(&mut b, KeyCode::Enter),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 2,
                kind: AnswerKind::Revise,
                picks: vec![],
                note: "fix".into(),
                send_now: false,
            })
        );

        let mut b = block(vec![question("Q1", 2, 2)]);
        press(&mut b, KeyCode::Enter);
        assert_eq!(open_action(&mut b, 2), BlockEffect::None);
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::Reject,
                text: String::new()
            }
        );
        assert_eq!(press(&mut b, KeyCode::Enter), BlockEffect::None);
        type_text(&mut b, "fix");
        assert_eq!(
            press(&mut b, KeyCode::Enter),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 2,
                kind: AnswerKind::Reject,
                picks: vec![pick(0, "b", "")],
                note: "fix".into(),
                send_now: false,
            })
        );

        // Esc leaves the note for the same decision and drops its text.
        let mut b = block(vec![question("Q1", 2, 2)]);
        press(&mut b, KeyCode::Enter);
        open_action(&mut b, 1);
        type_text(&mut b, "abc");
        assert_eq!(press(&mut b, KeyCode::Esc), BlockEffect::None);
        let v = b.view().unwrap();
        assert_eq!(v.mode, Mode::Choosing);
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        open_action(&mut b, 1);
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::Revise,
                text: String::new()
            }
        );
    }

    #[test]
    fn r_and_x_and_q_do_nothing_in_choosing() {
        let mut b = block(vec![question("Q1", 1, 2), question("Q2", 1, 1)]);
        let before = b.view().unwrap();
        for c in ['r', 'x', 'q'] {
            assert_eq!(press(&mut b, KeyCode::Char(c)), BlockEffect::None, "{c}");
            assert_eq!(b.view().unwrap(), before, "{c}");
        }
    }

    #[test]
    fn a_note_types_every_letter() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        open_action(&mut b, 1);
        type_text(&mut b, "hjklnNrxoq");
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::Revise,
                text: "hjklnNrxoq".into()
            }
        );
    }

    #[test]
    fn esc_leaves_from_any_decision_and_keeps_picks() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        assert_eq!(press(&mut b, KeyCode::Esc), BlockEffect::Leave);
        press(&mut b, KeyCode::Enter);
        assert_eq!(press(&mut b, KeyCode::Esc), BlockEffect::Leave);
        let v = b.view().unwrap();
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.picks, vec!["label b".to_string()]);
    }

    #[test]
    fn summary_h_and_esc_go_back_to_the_last_decision_keeping_picks() {
        for back in [KeyCode::Char('h'), KeyCode::Left, KeyCode::Esc] {
            let mut b = block(vec![question("Q1", 1, 2)]);
            press(&mut b, KeyCode::Enter);
            press(&mut b, KeyCode::Enter);
            assert_eq!(b.view().unwrap().mode, Mode::Summary);
            assert_eq!(press(&mut b, back), BlockEffect::None, "{back:?}");
            let v = b.view().unwrap();
            assert_eq!(v.mode, Mode::Choosing, "{back:?}");
            assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
            assert_eq!(v.picks, vec!["label b".to_string(), "label b".to_string()]);
            press(&mut b, KeyCode::Enter);
            assert_eq!(b.view().unwrap().mode, Mode::Summary);
            assert_eq!(
                press(&mut b, KeyCode::Char('o')),
                BlockEffect::OpenLink("https://example.com/x".into())
            );
            let before = b.view().unwrap();
            for c in ['j', 'k', 'l', 'r', 'x'] {
                assert_eq!(press(&mut b, KeyCode::Char(c)), BlockEffect::None, "{c}");
                assert_eq!(b.view().unwrap(), before, "{c}");
            }
        }
    }

    #[test]
    fn a_free_option_revisited_keeps_its_text() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Char('j'));
        press(&mut b, KeyCode::Enter);
        type_text(&mut b, "mine");
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.view().unwrap().ask.as_deref(), Some("Ask 2?"));
        press(&mut b, KeyCode::Char('h'));
        assert_eq!(highlighted(&b), 2, "still on the free option");
        press(&mut b, KeyCode::Char('l'));
        let v = b.view().unwrap();
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.picks, vec!["\"mine\"".to_string()]);
        press(&mut b, KeyCode::Char('h'));
        press(&mut b, KeyCode::Enter);
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::FreeText,
                text: "mine".into()
            }
        );
    }

    #[test]
    fn l_on_a_free_option_with_no_pick_opens_its_text_box() {
        for next in [KeyCode::Char('l'), KeyCode::Right] {
            let mut b = block(vec![question("Q1", 1, 2)]);
            press(&mut b, KeyCode::Char('j'));
            assert_eq!(press(&mut b, next), BlockEffect::None);
            let v = b.view().unwrap();
            assert_eq!(
                v.mode,
                Mode::Note {
                    kind: NoteKind::FreeText,
                    text: String::new()
                },
                "{next:?}"
            );
            assert_eq!(v.ask.as_deref(), Some("Ask 1?"), "it does not move on");
            assert!(v.picks.is_empty());
        }
    }

    #[test]
    fn l_on_a_decision_with_a_pick_replaces_it_and_keeps_later_picks() {
        let mut b = block(vec![question("Q1", 1, 3)]);
        press(&mut b, KeyCode::Char('l'));
        press(&mut b, KeyCode::Char('l'));
        press(&mut b, KeyCode::Char('h'));
        press(&mut b, KeyCode::Char('h'));
        press(&mut b, KeyCode::Char('k'));
        press(&mut b, KeyCode::Char('l'));
        let v = b.view().unwrap();
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.picks, vec!["label a".to_string(), "label b".to_string()]);
    }

    #[test]
    fn enter_on_the_summary_sends_the_answer_with_its_version() {
        let mut b = block(vec![question("Q1", 3, 2)]);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Up);
        press(&mut b, KeyCode::Enter);
        assert_eq!(
            press(&mut b, KeyCode::Enter),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 3,
                kind: AnswerKind::Answer,
                picks: vec![pick(0, "b", ""), pick(1, "a", "")],
                note: String::new(),
                send_now: false,
            })
        );
    }

    #[test]
    fn the_summary_shows_one_line_per_decision_in_words() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Up);
        press(&mut b, KeyCode::Enter);
        assert_eq!(
            b.view().unwrap().summary,
            vec![
                "Ask 1? \u{2014} label b".to_string(),
                "Ask 2? \u{2014} label a".to_string()
            ]
        );
    }

    #[test]
    fn a_free_option_takes_text() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Down);
        press(&mut b, KeyCode::Enter);
        assert_eq!(
            b.view().unwrap().mode,
            Mode::Note {
                kind: NoteKind::FreeText,
                text: String::new()
            }
        );
        assert_eq!(press(&mut b, KeyCode::Enter), BlockEffect::None);
        type_text(&mut b, "my words");
        assert_eq!(press(&mut b, KeyCode::Enter), BlockEffect::None);
        let v = b.view().unwrap();
        assert_eq!(v.mode, Mode::Summary);
        assert_eq!(v.summary, vec!["Ask 1? \u{2014} \"my words\"".to_string()]);
        assert_eq!(
            press(&mut b, KeyCode::Enter),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 1,
                kind: AnswerKind::FreeText,
                picks: vec![pick(0, "z", "my words")],
                note: "my words".into(),
                send_now: false,
            })
        );

        // Esc leaves the note without a pick
        let mut b = block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Down);
        press(&mut b, KeyCode::Enter);
        type_text(&mut b, "abc");
        press(&mut b, KeyCode::Esc);
        let v = b.view().unwrap();
        assert_eq!(v.mode, Mode::Choosing);
        assert!(v.picks.is_empty());
    }

    #[test]
    fn a_free_option_on_an_early_decision_continues_to_the_next() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Down);
        press(&mut b, KeyCode::Enter);
        type_text(&mut b, "mine");
        press(&mut b, KeyCode::Enter);
        let v = b.view().unwrap();
        assert_eq!(v.mode, Mode::Choosing);
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.picks, vec!["\"mine\"".to_string()]);
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.view().unwrap().mode, Mode::Summary);
        assert_eq!(
            press(&mut b, KeyCode::Enter),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 1,
                kind: AnswerKind::FreeText,
                picks: vec![pick(0, "z", "mine"), pick(1, "b", "")],
                note: "mine".into(),
                send_now: false,
            })
        );
    }

    #[test]
    fn a_note_stops_at_1999_characters() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        open_action(&mut b, 1);
        type_text(&mut b, &"a".repeat(1_999));
        type_text(&mut b, "bcd");
        match b.view().unwrap().mode {
            Mode::Note { text, .. } => {
                assert_eq!(text.chars().count(), 1_999);
                assert!(text.chars().all(|c| c == 'a'));
            }
            other => panic!("{other:?}"),
        }
        press(&mut b, KeyCode::Backspace);
        press(&mut b, KeyCode::Char('z'));
        match press(&mut b, KeyCode::Enter) {
            BlockEffect::Send(out) => {
                assert_eq!(out.note.chars().count(), 1_999);
                assert!(out.note.ends_with('z'));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn o_opens_only_an_http_link() {
        let mut q = question("Q1", 1, 1);
        q.link = "https://example.com/x".into();
        let mut b = block(vec![q.clone()]);
        assert_eq!(
            press(&mut b, KeyCode::Char('o')),
            BlockEffect::OpenLink("https://example.com/x".into())
        );
        q.link = "http://example.com".into();
        let mut b = block(vec![q.clone()]);
        assert!(matches!(
            press(&mut b, KeyCode::Char('o')),
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
                press(&mut b, KeyCode::Char('o')),
                BlockEffect::None,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_higher_version_drops_the_picks() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Enter);
        b.reconcile(queue(vec![question("Q1", 2, 2)]));
        let v = b.view().unwrap();
        assert_eq!(v.ask.as_deref(), Some("Ask 1?"));
        assert!(v.picks.is_empty());
        assert_eq!(
            v.notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Enter);
        match press(&mut b, KeyCode::Enter) {
            BlockEffect::Send(out) => assert_eq!(out.version, 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_dismissal_removes_the_question_with_its_reason() {
        let mut b = block(vec![question("Q1", 1, 2), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Enter);
        let mut q = queue(vec![question("Q2", 1, 1)]);
        q.dismissed = vec![("Q1".into(), "no longer needed".into())];
        b.reconcile(q);
        let v = b.view().unwrap();
        assert_eq!(v.id, "Q2");
        assert_eq!(v.notice.as_deref(), Some("Q1 dismissed: no longer needed"));

        let mut b = block(vec![question("Q1", 1, 1)]);
        b.reconcile(queue(vec![]));
        assert!(b.is_visible());
        assert_eq!(
            b.view().unwrap().notice.as_deref(),
            Some("Q1 dismissed: withdrawn")
        );
        assert_eq!(press(&mut b, KeyCode::Esc), BlockEffect::Leave);
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
        // A question that comes later shows with no old notice.
        b.reconcile(queue(vec![question("Q2", 1, 1)]));
        assert_eq!(b.view().unwrap().notice, None);
    }

    #[test]
    fn a_not_waiting_notice_stays_through_the_read_after_the_send() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        b.sent("Q1", SendResult::NotWaiting);
        // The read the runtime makes right after the send still shows it.
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
        assert_eq!(press(&mut b, KeyCode::Char('j')), BlockEffect::None);
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
    fn a_reorder_keeps_the_current_question() {
        let mut b = block(vec![question("Q1", 1, 2), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Enter);
        b.reconcile(queue(vec![question("Q2", 1, 1), question("Q1", 1, 2)]));
        let v = b.view().unwrap();
        assert_eq!(v.id, "Q1");
        assert_eq!(v.position, 2);
        assert_eq!(v.picks.len(), 1);
        assert_eq!(v.notice, None);
    }

    #[test]
    fn unavailable_keeps_the_picks_and_enter_retries() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Enter);
        let first = press(&mut b, KeyCode::Enter);
        assert!(matches!(first, BlockEffect::Send(_)));
        b.sent("Q1", SendResult::Unavailable);
        let v = b.view().unwrap();
        assert_eq!(v.mode, Mode::Summary);
        assert_eq!(
            v.notice.as_deref(),
            Some("Harold can't save answers right now")
        );
        assert_eq!(press(&mut b, KeyCode::Enter), first);
    }

    #[test]
    fn version_changed_on_send_clears_the_picks() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Enter);
        b.sent("Q1", SendResult::VersionChanged);
        let v = b.view().unwrap();
        assert_eq!(v.mode, Mode::Choosing);
        assert_eq!(v.ask.as_deref(), Some("Ask 1?"));
        assert!(v.picks.is_empty());
        assert_eq!(
            v.notice.as_deref(),
            Some("Q1 changed, your picks were cleared")
        );
    }

    #[test]
    fn recorded_shows_the_next_and_not_waiting_shows_the_dismissal() {
        let mut b = block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        b.sent("Q1", SendResult::Recorded);
        let v = b.view().unwrap();
        assert_eq!((v.id.as_str(), v.position, v.waiting), ("Q2", 1, 1));
        b.sent("Q2", SendResult::NotWaiting);
        assert!(
            b.view()
                .unwrap()
                .notice
                .unwrap()
                .starts_with("Q2 dismissed: ")
        );
        b.sent("Q2", SendResult::Recorded);
        let mut b = block(vec![question("Q1", 1, 1)]);
        b.sent("Q1", SendResult::Recorded);
        assert!(!b.is_visible());
    }

    #[test]
    fn n_and_n_shift_move_between_questions_and_wrap() {
        let shown_id = |b: &QuestionBlock| b.view().unwrap().id;
        let mut b = block(vec![
            question("Q1", 1, 1),
            question("Q2", 1, 1),
            question("Q3", 1, 1),
        ]);
        press(&mut b, KeyCode::Char('n'));
        assert_eq!(shown_id(&b), "Q2");
        press_times(&mut b, KeyCode::Char('n'), 2);
        assert_eq!(shown_id(&b), "Q1", "n wraps past the last question");
        press(&mut b, KeyCode::Char('N'));
        assert_eq!(shown_id(&b), "Q3", "N wraps past the first question");
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.view().unwrap().mode, Mode::Summary);
        press(&mut b, KeyCode::Char('n'));
        let v = b.view().unwrap();
        assert_eq!(
            (v.id.as_str(), v.mode),
            ("Q1", Mode::Choosing),
            "n moves from the summary"
        );

        let mut b = block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Char('l'));
        let before = b.view();
        press(&mut b, KeyCode::Char('n'));
        assert_eq!(b.view(), before, "one question: n does nothing");
        press(&mut b, KeyCode::Char('N'));
        assert_eq!(b.view(), before, "one question: N does nothing");
    }

    #[test]
    fn n_away_and_back_keeps_the_half_picked_question() {
        let mut b = block(vec![question("Q25", 1, 2), question("Q26", 1, 1)]);
        press(&mut b, KeyCode::Char('l'));
        press(&mut b, KeyCode::Char('n'));
        assert_eq!(b.view().unwrap().id, "Q26");
        assert!(
            b.view().unwrap().picks.is_empty(),
            "Q26 starts with no picks"
        );
        press(&mut b, KeyCode::Char('N'));
        let v = b.view().unwrap();
        assert_eq!(v.id, "Q25");
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.picks, vec!["label b".to_string()]);
    }

    #[test]
    fn kept_picks_drop_when_the_version_rises() {
        let mut b = block(vec![question("Q25", 1, 2), question("Q26", 1, 1)]);
        press(&mut b, KeyCode::Char('l'));
        press(&mut b, KeyCode::Char('n'));
        b.reconcile(queue(vec![question("Q25", 2, 2), question("Q26", 1, 1)]));
        assert_eq!(b.view().unwrap().notice, None, "Q25 is not in view");
        press(&mut b, KeyCode::Char('N'));
        let v = b.view().unwrap();
        assert_eq!(v.id, "Q25");
        assert_eq!(v.ask.as_deref(), Some("Ask 1?"));
        assert!(v.picks.is_empty());
        assert_eq!(v.highlighted, 1, "the suggested option");
        assert_eq!(v.notice, None);
        assert_eq!(b.shown().unwrap().1, 2);
    }

    #[test]
    fn kept_picks_go_when_the_question_is_dismissed_or_answered() {
        let fresh = |b: &QuestionBlock| {
            let v = b.view().unwrap();
            assert_eq!(v.id, "Q25");
            assert_eq!(v.ask.as_deref(), Some("Ask 1?"));
            assert!(v.picks.is_empty());
        };
        let mut b = block(vec![question("Q25", 1, 2), question("Q26", 1, 1)]);
        press(&mut b, KeyCode::Char('l'));
        press(&mut b, KeyCode::Char('n'));
        b.reconcile(queue(vec![question("Q26", 1, 1)]));
        b.reconcile(queue(vec![question("Q25", 1, 2), question("Q26", 1, 1)]));
        assert_eq!(b.view().unwrap().id, "Q26");
        press(&mut b, KeyCode::Char('N'));
        fresh(&b);

        let mut b = block(vec![
            question("Q27", 1, 1),
            question("Q25", 1, 2),
            question("Q26", 1, 1),
        ]);
        press(&mut b, KeyCode::Char('n'));
        press(&mut b, KeyCode::Char('l'));
        press(&mut b, KeyCode::Char('n'));
        assert_eq!(b.view().unwrap().id, "Q26");
        b.sent("Q26", SendResult::Recorded);
        assert_eq!(b.view().unwrap().id, "Q27");
        b.reconcile(queue(vec![question("Q27", 1, 1)]));
        b.reconcile(queue(vec![question("Q27", 1, 1), question("Q25", 1, 2)]));
        press(&mut b, KeyCode::Char('n'));
        fresh(&b);
    }

    #[test]
    fn an_answered_question_keeps_no_walk() {
        let mut b = block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Char('l'));
        b.sent("Q1", SendResult::Recorded);
        // Harold lists it again at the same version, as a stale read might.
        b.reconcile(queue(vec![question("Q2", 1, 1), question("Q1", 1, 1)]));
        press(&mut b, KeyCode::Char('n'));
        let v = b.view().unwrap();
        assert_eq!((v.id.as_str(), v.mode), ("Q1", Mode::Choosing));
        assert!(v.picks.is_empty());
    }

    #[test]
    fn later_keeps_the_picks() {
        let mut b = block(vec![question("Q1", 1, 2), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Char('l'));
        assert_eq!(open_action(&mut b, 0), BlockEffect::Defer("Q1".into()));
        assert_eq!(b.view().unwrap().id, "Q2");
        press(&mut b, KeyCode::Char('n'));
        let v = b.view().unwrap();
        assert_eq!(v.id, "Q1");
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.picks, vec!["label b".to_string()]);
    }

    fn at_summary(version: u32) -> QuestionBlock {
        let mut b = block(vec![question("Q1", version, 1)]);
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.view().unwrap().mode, Mode::Summary);
        b
    }

    fn sent_send_now(effect: BlockEffect) -> bool {
        match effect {
            BlockEffect::Send(out) => out.send_now,
            other => panic!("not a send: {other:?}"),
        }
    }

    #[test]
    fn s_toggles_send_now_on_the_summary() {
        let mut b = at_summary(1);
        assert!(!b.view().unwrap().send_now);
        press(&mut b, KeyCode::Char('s'));
        assert!(b.view().unwrap().send_now);
        press(&mut b, KeyCode::Char('s'));
        assert!(!b.view().unwrap().send_now);
    }

    #[test]
    fn the_mark_is_kept_through_h_and_back() {
        let mut b = at_summary(1);
        press(&mut b, KeyCode::Char('s'));
        press(&mut b, KeyCode::Char('h'));
        assert_eq!(b.view().unwrap().mode, Mode::Choosing);
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.view().unwrap().mode, Mode::Summary);
        assert!(b.view().unwrap().send_now);
        press(&mut b, KeyCode::Esc);
        press(&mut b, KeyCode::Enter);
        assert!(b.view().unwrap().send_now);
    }

    #[test]
    fn the_mark_is_kept_while_another_question_is_shown() {
        let mut b = block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Char('s'));
        press(&mut b, KeyCode::Char('n'));
        assert!(!b.view().unwrap().send_now);
        press(&mut b, KeyCode::Char('N'));
        assert!(b.view().unwrap().send_now);
    }

    #[test]
    fn enter_sends_the_answer_with_send_now() {
        let mut b = at_summary(1);
        press(&mut b, KeyCode::Char('s'));
        assert!(sent_send_now(press(&mut b, KeyCode::Enter)));
        let mut b = at_summary(1);
        assert!(!sent_send_now(press(&mut b, KeyCode::Enter)));
    }

    #[test]
    fn the_mark_starts_at_no_on_every_summary() {
        // After the answer is recorded, the next question's summary starts unmarked.
        let mut b = block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Char('s'));
        b.sent("Q1", SendResult::Recorded);
        assert_eq!(b.view().unwrap().id, "Q2");
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.view().unwrap().mode, Mode::Summary);
        assert!(!b.view().unwrap().send_now);

        // A higher version restarts the walk, and the mark with it.
        let mut b = at_summary(1);
        press(&mut b, KeyCode::Char('s'));
        b.reconcile(Queue {
            questions: vec![question("Q1", 2, 1)],
            ..Queue::default()
        });
        assert!(!b.view().unwrap().send_now);
        press(&mut b, KeyCode::Enter);
        assert!(!b.view().unwrap().send_now);

        // So does a version the send was refused for.
        let mut b = at_summary(1);
        press(&mut b, KeyCode::Char('s'));
        b.sent("Q1", SendResult::VersionChanged);
        press(&mut b, KeyCode::Enter);
        assert!(!b.view().unwrap().send_now);
    }

    #[test]
    fn choosing_ignores_s() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Char('s'));
        assert!(!b.view().unwrap().send_now);
        press(&mut b, KeyCode::Enter);
        assert!(!b.view().unwrap().send_now);
    }

    #[test]
    fn revise_and_reject_notes_type_s_and_send_unmarked() {
        for kind in [NoteKind::Revise, NoteKind::Reject] {
            let mut b = at_summary(1);
            press(&mut b, KeyCode::Char('s'));
            press(&mut b, KeyCode::Char('h'));
            b.open_note(kind);
            press(&mut b, KeyCode::Char('s'));
            press(&mut b, KeyCode::Char('s'));
            let Mode::Note { text, .. } = b.view().unwrap().mode else {
                panic!("not in a note");
            };
            assert_eq!(text, "ss");
            match press(&mut b, KeyCode::Enter) {
                BlockEffect::Send(out) => {
                    assert_eq!(out.note, "ss");
                    assert!(!out.send_now);
                }
                other => panic!("not a send: {other:?}"),
            }
        }
    }
}
