//! The dash's notes search: `s` opens the vault notes changed most recently over the agent
//! list, typing searches them by meaning, `j` and `k` move, Enter opens the vault note's
//! file in `$EDITOR` in a new tmux window and Esc goes back. It reads the knowledge tool's
//! gRPC service; there is no note pane and no related list.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::KeyCode;

use crate::text::sanitize_display;

/// The most rows a list holds.
pub const ROWS: usize = 10;
/// How long typing pauses before a search runs.
pub const DEBOUNCE: Duration = Duration::from_millis(300);
/// The longest query typed, in characters.
const QUERY_MAX: usize = 200;
/// The shortest query that searches; a shorter one shows the recent list.
const QUERY_MIN: usize = 2;
/// How much of a title or kind is kept for display.
const DISPLAY_MAX: usize = 200;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What the dash asks of the knowledge tool. The real one speaks gRPC; tests have their own.
pub trait NotesBackend: Send + Sync {
    /// The vault notes changed most recently.
    fn recent(&self) -> BoxFuture<Result<Vec<NoteLine>, String>>;
    /// The vault notes closest in meaning to `query`.
    fn search(&self, query: String) -> BoxFuture<Result<Vec<NoteLine>, String>>;
    /// The absolute path of a vault note's file.
    fn file(&self, name: String) -> BoxFuture<Result<String, String>>;
    /// Whether Harold serves the knowledge tool at all: `s` is for nothing when it does not.
    fn available(&self) -> BoxFuture<bool> {
        Box::pin(async { true })
    }
}

pub type SharedBackend = Arc<dyn NotesBackend>;

/// One row: a vault note's name, title and kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteLine {
    pub name: String,
    pub title: String,
    pub kind: String,
}

impl NoteLine {
    /// The row as it is drawn: terminal controls removed.
    pub fn display(&self) -> (String, String) {
        let title = if self.title.is_empty() {
            &self.name
        } else {
            &self.title
        };
        (
            sanitize_display(&self.kind, DISPLAY_MAX),
            sanitize_display(title, DISPLAY_MAX),
        )
    }
}

/// What the runtime does for the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotesAction {
    /// Read a list. `query` under two characters reads the recent list. `seq` numbers the
    /// read, so an older answer is ignored; `debounce` waits for typing to pause.
    Load {
        query: String,
        seq: u64,
        debounce: bool,
    },
    /// Open the vault note's file in the editor.
    Open { name: String },
}

/// What the view asks of the app after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotesKey {
    None,
    Close,
    Action(NotesAction),
}

/// What an answer reports to the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotesEvent {
    /// Whether Harold serves the knowledge tool, as asked when the stream opened.
    Available(bool),
    Loaded {
        seq: u64,
        result: Result<Vec<NoteLine>, String>,
    },
    /// The file to open, or why it cannot be.
    File(Result<String, String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotesView {
    pub query: String,
    /// Keys type into the query, rather than move.
    pub typing: bool,
    pub rows: Vec<NoteLine>,
    pub cursor: usize,
    /// What the list says in place of rows, or after a failure.
    pub message: Option<String>,
    pub loading: bool,
    /// Whether `rows` answer a search, not the recent list.
    pub searched: bool,
    seq: u64,
}

impl NotesView {
    /// The view as `s` opens it, and the read of the recent list it starts.
    pub fn open() -> (Self, NotesAction) {
        let mut view = Self {
            query: String::new(),
            typing: false,
            rows: Vec::new(),
            cursor: 0,
            message: None,
            loading: true,
            searched: false,
            seq: 0,
        };
        let action = view.load(false);
        (view, action)
    }

    fn load(&mut self, debounce: bool) -> NotesAction {
        self.seq += 1;
        self.loading = true;
        NotesAction::Load {
            query: self.query.clone(),
            seq: self.seq,
            debounce,
        }
    }

    pub fn selected(&self) -> Option<&NoteLine> {
        self.rows.get(self.cursor)
    }

    fn changed(&mut self) -> NotesKey {
        NotesKey::Action(self.load(self.query.chars().count() >= QUERY_MIN))
    }

    fn push(&mut self, character: char) -> NotesKey {
        self.typing = true;
        if self.query.chars().count() < QUERY_MAX {
            self.query.push(character);
        }
        self.changed()
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    fn open_selected(&mut self) -> NotesKey {
        let Some(row) = self.selected() else {
            return NotesKey::None;
        };
        let name = row.name.clone();
        self.typing = false;
        self.message = None;
        NotesKey::Action(NotesAction::Open { name })
    }

    /// Handles one key. Printable keys type a search, except `j`, `k`, `g` and `G` while
    /// the cursor has the keys (`/` gives the keys to typing first).
    pub fn handle_key(&mut self, code: KeyCode) -> NotesKey {
        match (self.typing, code) {
            (_, KeyCode::Esc) => {
                if self.query.is_empty() && !self.typing {
                    return NotesKey::Close;
                }
                self.query.clear();
                self.typing = false;
                self.searched = false;
                NotesKey::Action(self.load(false))
            }
            (_, KeyCode::Enter) => self.open_selected(),
            (_, KeyCode::Down) => {
                self.move_by(1);
                NotesKey::None
            }
            (_, KeyCode::Up) => {
                self.move_by(-1);
                NotesKey::None
            }
            (_, KeyCode::Backspace) => {
                if self.query.pop().is_none() {
                    return NotesKey::None;
                }
                self.typing = true;
                self.changed()
            }
            (false, KeyCode::Char('j')) => {
                self.move_by(1);
                NotesKey::None
            }
            (false, KeyCode::Char('k')) => {
                self.move_by(-1);
                NotesKey::None
            }
            (false, KeyCode::Char('g')) => {
                self.cursor = 0;
                NotesKey::None
            }
            (false, KeyCode::Char('G')) => {
                self.cursor = self.rows.len().saturating_sub(1);
                NotesKey::None
            }
            (false, KeyCode::Char('/')) => {
                self.typing = true;
                NotesKey::None
            }
            (_, KeyCode::Char(character)) if !character.is_control() => self.push(character),
            _ => NotesKey::None,
        }
    }

    /// Takes the answer of read `seq`; an older one is ignored.
    pub fn loaded(&mut self, seq: u64, result: Result<Vec<NoteLine>, String>) {
        if seq != self.seq {
            return;
        }
        self.loading = false;
        match result {
            Ok(rows) => {
                self.searched = self.query.chars().count() >= QUERY_MIN;
                self.message = rows.is_empty().then(|| {
                    if self.searched {
                        "No notes match.".to_owned()
                    } else {
                        "No notes yet.".to_owned()
                    }
                });
                self.rows = rows.into_iter().take(ROWS).collect();
                self.cursor = 0;
            }
            Err(message) => {
                self.rows.clear();
                self.cursor = 0;
                self.message = Some(message);
            }
        }
    }

    /// Shows why a vault note's file could not be opened.
    pub fn failed(&mut self, message: String) {
        self.message = Some(message);
    }
}

#[cfg(test)]
#[path = "notes_tests.rs"]
mod tests;
