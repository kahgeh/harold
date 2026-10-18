//! The question block's state machine: the keys, the picks and the notices.
//!
//! Pure and free of IO. The runtime maps Harold's question queue into [`Queue`], feeds keys
//! to [`QuestionBlock::handle_key`], does what the returned [`BlockEffect`] asks, and reports
//! the outcome of a send through [`QuestionBlock::sent`].

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
    pub highlighted: usize,
    pub mode: Mode,
    /// The labels picked so far.
    pub picks: Vec<String>,
    /// One `<ask> — <label>` line per picked decision.
    pub summary: Vec<String>,
    pub notice: Option<String>,
}

/// The block's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuestionBlock {
    queue: Queue,
    current: Option<String>,
    version: u32,
    decision: usize,
    picks: Vec<PickView>,
    highlighted: usize,
    mode: Mode,
    notice: Option<String>,
    /// Hidden after a failed read or a lost stream, with the picks, mode and note kept
    /// for the next listing.
    suspended: bool,
}

/// The notice when Harold could not save an answer or a deferral.
pub const UNAVAILABLE_NOTICE: &str = "Harold can't save answers right now";

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
    /// A suspended block shows again, and keeps its state when its question is listed at
    /// the same version.
    pub fn reconcile(&mut self, queue: Queue) {
        self.suspended = false;
        self.queue = queue;
        let Some(id) = self.current.clone() else {
            self.select_first();
            return;
        };
        match self.queue.questions.iter().find(|q| q.id == id) {
            Some(q) if q.version > self.version => {
                self.version = q.version;
                self.restart();
                self.notice = Some(changed_notice(&id));
            }
            Some(_) => {}
            None => {
                self.notice = Some(self.dismissed_notice(&id));
                self.select_first();
            }
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
    }

    /// Sets a notice, for the runtime's own messages.
    pub fn set_notice(&mut self, text: String) {
        self.notice = Some(text);
    }

    /// Applies the outcome of a send for question `id`.
    pub fn sent(&mut self, id: &str, result: SendResult) {
        if self.current.as_deref() != Some(id) {
            return;
        }
        match result {
            SendResult::Recorded => {
                self.queue.questions.retain(|q| q.id != id);
                self.queue.waiting = self.queue.waiting.saturating_sub(1);
                self.notice = None;
                self.select_first();
            }
            SendResult::VersionChanged => {
                self.restart();
                self.notice = Some(changed_notice(id));
            }
            SendResult::NotWaiting => {
                self.notice = Some(self.dismissed_notice(id));
                self.queue.questions.retain(|q| q.id != id);
                self.select_first();
            }
            SendResult::Unavailable => {
                self.notice = Some(UNAVAILABLE_NOTICE.into());
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
            highlighted: self.highlighted,
            mode: self.mode.clone(),
            picks,
            summary,
            notice: self.notice.clone(),
        })
    }

    /// Handles one key and says what the runtime should do.
    pub fn handle_key(&mut self, code: KeyCode) -> BlockEffect {
        if self.suspended {
            return BlockEffect::None;
        }
        if self.question().is_none() {
            if code == KeyCode::Esc {
                self.notice = None;
                return BlockEffect::Leave;
            }
            return BlockEffect::None;
        }
        self.notice = None;
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
        match code {
            KeyCode::Up => self.highlighted = self.highlighted.saturating_sub(1),
            KeyCode::Down => {
                self.highlighted = (self.highlighted + 1).min(options.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let Some(option) = options.get(self.highlighted) else {
                    return BlockEffect::None;
                };
                if option.free {
                    self.mode = Mode::Note {
                        kind: NoteKind::FreeText,
                        text: String::new(),
                    };
                } else {
                    self.push_pick(&q, option.key.clone(), String::new());
                }
            }
            KeyCode::Char('r') => self.open_note(NoteKind::Revise),
            KeyCode::Char('x') => self.open_note(NoteKind::Reject),
            KeyCode::Char('o') if is_web_link(&q.link) => return BlockEffect::OpenLink(q.link),
            KeyCode::Char('l') => {
                self.defer_locally(&q.id);
                return BlockEffect::Defer(q.id);
            }
            KeyCode::Esc => {
                if self.decision == 0 {
                    return BlockEffect::Leave;
                }
                self.step_back(&q);
            }
            _ => {}
        }
        BlockEffect::None
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
                });
            }
            KeyCode::Esc => {
                self.mode = Mode::Choosing;
                self.step_back(&q);
            }
            _ => {}
        }
        BlockEffect::None
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
                        let key = q.decisions[self.decision].options[self.highlighted]
                            .key
                            .clone();
                        self.push_pick(&q, key, typed);
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

    /// Records a pick, then moves to the next decision or the summary.
    fn push_pick(&mut self, q: &QuestionView, key: String, text: String) {
        self.picks.push(PickView {
            decision: self.decision as u32,
            key,
            text,
        });
        if self.decision + 1 < q.decisions.len() {
            self.decision += 1;
            self.highlighted = starting_highlight(&q.decisions[self.decision].options);
            self.mode = Mode::Choosing;
        } else {
            self.mode = Mode::Summary;
        }
    }

    /// Drops the last pick and shows its decision, highlighting what was picked.
    fn step_back(&mut self, q: &QuestionView) {
        let Some(dropped) = self.picks.pop() else {
            return;
        };
        self.decision = dropped.decision as usize;
        self.highlighted = q.decisions[self.decision]
            .options
            .iter()
            .position(|o| o.key == dropped.key)
            .unwrap_or(0);
        self.mode = Mode::Choosing;
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
        self.mode = Mode::Choosing;
        self.highlighted = self
            .question()
            .and_then(|q| q.decisions.first())
            .map_or(0, |d| starting_highlight(&d.options));
    }

    /// Makes the queue's first question current, or none.
    fn select_first(&mut self) {
        match self.queue.questions.first() {
            Some(q) => {
                self.current = Some(q.id.clone());
                self.version = q.version;
            }
            None => self.current = None,
        }
        self.restart();
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

    #[test]
    fn the_keys_walk_every_decision_to_the_summary() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        let v = b.view().unwrap();
        assert_eq!(v.highlighted, 1, "starts on the suggested option");
        assert_eq!(v.ask.as_deref(), Some("Ask 1?"));
        assert_eq!((v.position, v.waiting), (1, 1));
        press(&mut b, KeyCode::Up);
        assert_eq!(b.view().unwrap().highlighted, 0);
        press(&mut b, KeyCode::Up);
        assert_eq!(b.view().unwrap().highlighted, 0);
        assert_eq!(press(&mut b, KeyCode::Enter), BlockEffect::None);
        let v = b.view().unwrap();
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.picks, vec!["label a".to_string()]);
        assert_eq!(v.highlighted, 1);
        press(&mut b, KeyCode::Down);
        press(&mut b, KeyCode::Down);
        press(&mut b, KeyCode::Down);
        assert_eq!(b.view().unwrap().highlighted, 2);
        press(&mut b, KeyCode::Up);
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.view().unwrap().mode, Mode::Summary);
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
            })
        );
    }

    #[test]
    fn esc_steps_back_and_leaves_at_the_first_decision() {
        let mut b = block(vec![question("Q1", 1, 2)]);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Enter);
        assert_eq!(b.view().unwrap().mode, Mode::Summary);
        assert_eq!(press(&mut b, KeyCode::Esc), BlockEffect::None);
        let v = b.view().unwrap();
        assert_eq!(v.mode, Mode::Choosing);
        assert_eq!(v.ask.as_deref(), Some("Ask 2?"));
        assert_eq!(v.picks.len(), 1, "the last pick was dropped");
        assert_eq!(press(&mut b, KeyCode::Esc), BlockEffect::None);
        let v = b.view().unwrap();
        assert_eq!(v.ask.as_deref(), Some("Ask 1?"));
        assert!(v.picks.is_empty());
        assert_eq!(press(&mut b, KeyCode::Esc), BlockEffect::Leave);
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
    fn r_and_x_take_a_note() {
        let mut b = block(vec![question("Q1", 2, 2)]);
        press(&mut b, KeyCode::Enter);
        press(&mut b, KeyCode::Char('r'));
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
        type_text(&mut b, "make it bigg");
        press(&mut b, KeyCode::Char('x'));
        press(&mut b, KeyCode::Backspace);
        press(&mut b, KeyCode::Backspace);
        type_text(&mut b, "ger");
        assert_eq!(
            press(&mut b, KeyCode::Enter),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 2,
                kind: AnswerKind::Revise,
                picks: vec![pick(0, "b", "")],
                note: "make it bigger".into(),
            })
        );

        let mut b = block(vec![question("Q1", 2, 2)]);
        press(&mut b, KeyCode::Char('x'));
        type_text(&mut b, "no");
        assert_eq!(
            press(&mut b, KeyCode::Enter),
            BlockEffect::Send(Outgoing {
                id: "Q1".into(),
                version: 2,
                kind: AnswerKind::Reject,
                picks: vec![],
                note: "no".into(),
            })
        );

        let mut b = block(vec![question("Q1", 2, 2)]);
        press(&mut b, KeyCode::Char('r'));
        type_text(&mut b, "abc");
        press(&mut b, KeyCode::Esc);
        let v = b.view().unwrap();
        assert_eq!(v.mode, Mode::Choosing);
        assert_eq!(v.ask.as_deref(), Some("Ask 1?"));
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
            })
        );
    }

    #[test]
    fn a_note_stops_at_1999_characters() {
        let mut b = block(vec![question("Q1", 1, 1)]);
        press(&mut b, KeyCode::Char('r'));
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
    fn l_defers() {
        let mut b = block(vec![question("Q1", 1, 1), question("Q2", 1, 1)]);
        assert_eq!(
            press(&mut b, KeyCode::Char('l')),
            BlockEffect::Defer("Q1".into())
        );
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
}
