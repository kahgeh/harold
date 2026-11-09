//! The remote control: `control-tower --remote`.
//!
//! The model (the list of entries and the keys) and the drawing have no I/O, so
//! tests can check every rule. The run loop takes its Harold stream, keys, screen
//! and signals through small traits, so tests run it with fakes.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use tokio::runtime::Builder;

use crate::api::{AgentStateSource, SourceError, SourceStream};
use crate::app::{AgentRow, Snapshot};
use crate::cli::Options;
use crate::navigation::{PaneNavigator, TmuxNavigator};
use crate::runtime::{
    AppError, CrosstermInput, InputPump, RuntimeInput, ShutdownPort, ShutdownSignals, merge_exit,
    panic_detail, sanitize_error,
};
use crate::terminal::{TerminalError, TerminalGuard};
use crate::text::sanitize_display;
use crate::ui::{AMBER, BOARD, CORAL, INK, MUTED};

/// One line of the remote control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The pane id of the agent, for example `block:<id>` for a Rex pane.
    pub pane_id: String,
    /// The session of the agent. The jump needs it.
    pub session: String,
    /// The block id of a Rex pane, when `pane_id` is `block:<id>`.
    pub block_id: Option<String>,
    /// The product of the pod, empty for the chief.
    pub product: String,
    /// `Chief`, or `PnEM · <product>`.
    pub label: String,
    pub is_chief: bool,
}

/// What the caller must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Jump to the entry with this index, then close.
    Jump(usize),
    /// Save the new order of the PnEM products.
    Move {
        order: Vec<String>,
    },
    /// Close with no jump.
    Close,
    None,
}

/// The state of the remote control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub entries: Vec<Entry>,
    pub selected: usize,
    /// A message for the last line, for example a failed jump.
    pub notice: Option<String>,
}

/// Build the entries: the chief first, then the PnEMs in the given order.
///
/// Only rows of this Mac (empty `host`) are used. A PnEM whose product is not
/// in `order` comes after the others, in row order.
pub fn entries(rows: &[AgentRow], order: &[String]) -> Vec<Entry> {
    let local = || rows.iter().filter(|row| row.host.is_empty());
    let make = |row: &AgentRow, is_chief: bool| Entry {
        pane_id: row.incarnation.pane_id.clone(),
        session: row.session_name.clone(),
        block_id: row
            .incarnation
            .pane_id
            .strip_prefix("block:")
            .map(str::to_string),
        product: row.pod_product.clone(),
        label: if is_chief {
            "Chief".to_string()
        } else {
            format!("PnEM \u{b7} {}", row.pod_product)
        },
        is_chief,
    };

    let mut list: Vec<Entry> = local()
        .find(|row| row.role == "chief")
        .map(|row| make(row, true))
        .into_iter()
        .collect();

    let mut pnems: Vec<&AgentRow> = local().filter(|row| row.role == "pnem").collect();
    // The sort is stable, so equal keys keep the row order.
    pnems.sort_by_key(|row| {
        order
            .iter()
            .position(|name| *name == row.pod_product)
            .unwrap_or(usize::MAX)
    });
    list.extend(pnems.into_iter().map(|row| make(row, false)));
    list
}

impl Model {
    pub fn new(entries: Vec<Entry>) -> Model {
        Model {
            entries,
            selected: 0,
            notice: None,
        }
    }

    /// Use new entries and keep the selection on the same pane when it exists.
    pub fn replace_entries(&mut self, entries: Vec<Entry>) {
        let current = self.entries.get(self.selected).map(|e| e.pane_id.clone());
        self.entries = entries;
        let same = current.and_then(|id| self.entries.iter().position(|e| e.pane_id == id));
        self.selected = same.unwrap_or(self.selected);
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    fn order(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| !e.is_chief)
            .map(|e| e.product.clone())
            .collect()
    }

    /// Swap the selected PnEM with its neighbour. `up` moves it towards the chief.
    fn move_selected(&mut self, up: bool) -> Action {
        let from = self.selected;
        match self.entries.get(from) {
            Some(entry) if !entry.is_chief => {}
            _ => return Action::None,
        }
        let to = if up {
            match from.checked_sub(1) {
                Some(to) => to,
                None => return Action::None,
            }
        } else {
            from + 1
        };
        match self.entries.get(to) {
            Some(neighbour) if !neighbour.is_chief => {}
            _ => return Action::None,
        }
        self.entries.swap(from, to);
        self.selected = to;
        Action::Move {
            order: self.order(),
        }
    }

    /// Moves the selected row and clears the notice only when a row moved.
    fn moved(&mut self, up: bool) -> Action {
        let action = self.move_selected(up);
        if action != Action::None {
            self.notice = None;
        }
        action
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Action::None;
        }
        let last = self.entries.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => Action::Close,
            KeyCode::Enter if self.entries.is_empty() => Action::None,
            KeyCode::Enter => {
                self.notice = None;
                Action::Jump(self.selected)
            }
            KeyCode::Char(digit @ '0'..='9') => {
                let index = digit as usize - '0' as usize;
                if index < self.entries.len() {
                    self.notice = None;
                    Action::Jump(index)
                } else {
                    Action::None
                }
            }
            KeyCode::Char('j') => {
                self.notice = None;
                self.selected = (self.selected + 1).min(last);
                Action::None
            }
            KeyCode::Char('k') => {
                self.notice = None;
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Char('J') => self.moved(false),
            KeyCode::Char('K') => self.moved(true),
            _ => Action::None,
        }
    }
}

/// Draw the remote control in the whole area of the frame.
///
/// The Rex layer is already the small window, so the box fills the area. Text
/// is cut at the width and never wraps.
pub fn draw(frame: &mut Frame, model: &Model) {
    let area = frame.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(Line::styled(
            "\u{2500} Harold: remote control ",
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(BOARD));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let ink = Style::default().fg(INK);
    let marked = Style::default().fg(AMBER).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(MUTED);

    let mut lines: Vec<Line> = vec![Line::raw("")];
    for (index, entry) in model.entries.iter().enumerate() {
        let selected = index == model.selected;
        let mark = if selected { '\u{25b8}' } else { ' ' };
        let style = if selected { marked } else { ink };
        lines.push(Line::styled(
            format!("{mark} {index}   {}", entry.label),
            style,
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  0-9 jump   j/k select   J/K move row",
        muted,
    ));
    lines.push(match &model.notice {
        Some(notice) => Line::styled(
            format!("  {notice}"),
            Style::default().fg(CORAL).add_modifier(Modifier::BOLD),
        ),
        None => Line::styled("  Enter jump   Esc close", muted),
    });

    // Draw the hint lines at the bottom of the box; the rows and the blank
    // line above them are cut first when the area is short.
    let hint_start = lines.len().saturating_sub(2);
    let height = usize::from(inner.height);
    let (body, hints) = lines.split_at(hint_start);
    let hint_rows = hints.len().min(height);
    let body_rows = height - hint_rows;
    let body_area = Rect {
        height: body_rows as u16,
        ..inner
    };
    let hint_area = Rect {
        y: inner.y + body_rows as u16,
        height: hint_rows as u16,
        ..inner
    };
    frame.render_widget(Paragraph::new(body.to_vec()), body_area);
    frame.render_widget(Paragraph::new(hints.to_vec()), hint_area);
}

/// The full order to save after a move: the moved running products take the places
/// that the running products held in `full`, so a stopped product keeps its place.
/// A moved product that is not in `full` joins at the end. With an empty `full` (an
/// older Harold, or no registry), it is the moved list.
fn merge_order(full: &[String], moved: &[String]) -> Vec<String> {
    if full.is_empty() {
        return moved.to_vec();
    }
    let mut running: Vec<&String> = Vec::new();
    for product in moved {
        if !running.contains(&product) {
            running.push(product);
        }
    }
    let mut next = running.iter().filter(|product| full.contains(product));
    let mut merged: Vec<String> = full
        .iter()
        .map(|product| {
            let slot = running.contains(&product).then(|| next.next()).flatten();
            slot.map_or_else(|| product.clone(), |moved| (*moved).clone())
        })
        .collect();
    merged.extend(
        running
            .iter()
            .filter(|product| !full.contains(product))
            .map(|product| (*product).clone()),
    );
    merged
}

/// The remote order as Harold stores it, and the order on the screen. They differ
/// only after a failed save.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Orders {
    stored: Vec<String>,
    screen: Vec<String>,
}

impl Orders {
    fn new(stored: Vec<String>) -> Orders {
        Orders {
            screen: stored.clone(),
            stored,
        }
    }

    /// The full list to send after the running PnEMs moved to `moved`.
    fn to_save(&self, moved: &[String]) -> Vec<String> {
        merge_order(&self.stored, moved)
    }

    fn saved(&mut self, sent: Vec<String>) {
        self.stored = sent.clone();
        self.screen = sent;
    }

    /// The screen keeps the new order; the stored order stays as Harold has it.
    fn not_saved(&mut self, sent: Vec<String>) {
        self.screen = sent;
    }
}

/// The longest notice on the last line, in characters, before the width cuts it.
const NOTICE_LIMIT: usize = 200;

/// A boxed future that need not be `Send`: the loop runs on one thread.
pub(crate) type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Opens the Harold stream.
pub(crate) trait RemoteSource {
    type Stream: RemoteStream;
    fn open(&self) -> LocalFuture<'_, Result<Self::Stream, SourceError>>;
}

/// The Harold stream, the two calls of the remote order and the `AgentSeen` call.
pub(crate) trait RemoteStream {
    fn receive(&mut self) -> LocalFuture<'_, Option<Result<Snapshot, SourceError>>>;
    fn remote_order(&self) -> LocalFuture<'_, Result<Vec<String>, SourceError>>;
    fn save_remote_order<'a>(
        &'a self,
        products: &'a [String],
    ) -> LocalFuture<'a, Result<(), SourceError>>;
    fn agent_seen<'a>(&'a self, pane_id: &'a str) -> LocalFuture<'a, Result<(), SourceError>>;
}

/// The terminal keys. `None` means the input stopped.
pub(crate) trait RemoteKeys {
    fn next_input(&mut self) -> LocalFuture<'_, Option<Result<RuntimeInput, String>>>;
}

pub(crate) trait RemoteScreen {
    fn show(&mut self, model: &Model) -> Result<(), TerminalError>;
}

impl RemoteSource for AgentStateSource {
    type Stream = SourceStream;

    fn open(&self) -> LocalFuture<'_, Result<SourceStream, SourceError>> {
        AgentStateSource::open(self)
    }
}

impl RemoteStream for SourceStream {
    fn receive(&mut self) -> LocalFuture<'_, Option<Result<Snapshot, SourceError>>> {
        Box::pin(self.recv())
    }

    fn remote_order(&self) -> LocalFuture<'_, Result<Vec<String>, SourceError>> {
        Box::pin(self.get_remote_order())
    }

    fn save_remote_order<'a>(
        &'a self,
        products: &'a [String],
    ) -> LocalFuture<'a, Result<(), SourceError>> {
        Box::pin(self.set_remote_order(products))
    }

    fn agent_seen<'a>(&'a self, pane_id: &'a str) -> LocalFuture<'a, Result<(), SourceError>> {
        Box::pin(self.request_agent_seen(pane_id))
    }
}

impl RemoteKeys for InputPump {
    fn next_input(&mut self) -> LocalFuture<'_, Option<Result<RuntimeInput, String>>> {
        Box::pin(self.recv())
    }
}

impl RemoteScreen for TerminalGuard {
    fn show(&mut self, model: &Model) -> Result<(), TerminalError> {
        self.draw_remote(model)
    }
}

/// Run the remote control until a jump, Esc, a signal or an error.
///
/// Every exit closes the remote's own Rex layer once (the block in `REX_BLOCK`), then
/// restores the terminal. A failed close is ignored.
pub fn run(
    _options: Options,
    source: AgentStateSource,
    navigator: TmuxNavigator,
) -> Result<(), AppError> {
    // REX_BLOCK comes from the navigator's context, as REX_SESSION does.
    let block = navigator.own_rex_block();
    let block = block.as_deref();
    let started = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AppError::Runtime(sanitize_error(&error.to_string())))
        .and_then(|runtime| {
            let shutdown = runtime.block_on(async { ShutdownSignals::new() })?;
            let terminal = TerminalGuard::acquire()?;
            Ok((runtime, shutdown, terminal))
        });
    let (runtime, mut shutdown, mut terminal) = match started {
        Ok(started) => started,
        Err(error) => {
            close_layer(&navigator, block);
            return Err(error);
        }
    };

    let mut keys = InputPump::start_with(CrosstermInput);
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(run_with(
            &source,
            &mut keys,
            &mut shutdown,
            &mut terminal,
            &navigator,
            block,
        ))
    }));
    drop(keys);
    if outcome.is_err() {
        // A panic left `run_with` before its own close.
        close_layer(&navigator, block);
    }
    let restore_result = terminal.restore().map_err(AppError::from);

    match outcome {
        Ok(Ok(())) => restore_result,
        Ok(Err(error)) => Err(merge_exit(error, restore_result)),
        Err(payload) => Err(merge_exit(
            AppError::Panic(panic_detail(payload)),
            restore_result,
        )),
    }
}

/// The loop of the remote, then the close of its own layer (`block`), on every exit.
pub(crate) async fn run_with(
    source: &impl RemoteSource,
    keys: &mut impl RemoteKeys,
    shutdown: &mut impl ShutdownPort,
    screen: &mut impl RemoteScreen,
    navigator: &TmuxNavigator,
    block: Option<&str>,
) -> Result<(), AppError> {
    let result = remote_loop(source, keys, shutdown, screen, navigator).await;
    close_layer(navigator, block);
    result
}

/// Closes the remote's own layer. A missing block or a failed close does nothing more:
/// the remote exits in any case.
fn close_layer(navigator: &TmuxNavigator, block: Option<&str>) {
    if let Some(block) = block.filter(|block| !block.trim().is_empty()) {
        let _ = navigator.close_own_layer(block);
    }
}

fn notice(what: &str, detail: &str) -> String {
    sanitize_display(&format!("{what}: {detail}"), NOTICE_LIMIT)
}

/// The next snapshot, or never when there is no stream.
async fn next_snapshot<S: RemoteStream>(
    stream: &mut Option<S>,
) -> Option<Result<Snapshot, SourceError>> {
    match stream {
        Some(stream) => stream.receive().await,
        None => std::future::pending().await,
    }
}

async fn remote_loop<S: RemoteSource>(
    source: &S,
    keys: &mut impl RemoteKeys,
    shutdown: &mut impl ShutdownPort,
    screen: &mut impl RemoteScreen,
    navigator: &TmuxNavigator,
) -> Result<(), AppError> {
    let mut model = Model::new(Vec::new());
    let mut orders = Orders::default();
    let mut stream: Option<S::Stream> = None;
    let mut opening = true;
    let open = source.open();
    tokio::pin!(open);
    screen.show(&model)?;

    loop {
        tokio::select! {
            opened = &mut open, if opening => {
                opening = false;
                match opened {
                    Ok(opened) => {
                        match opened.remote_order().await {
                            Ok(products) => orders = Orders::new(products),
                            Err(error) => {
                                model.notice =
                                    Some(notice("could not read the remote order", error.detail()));
                            }
                        }
                        stream = Some(opened);
                    }
                    Err(error) => {
                        model.notice = Some(notice("Harold is not reachable", error.detail()));
                    }
                }
            }
            item = next_snapshot(&mut stream) => {
                match item {
                    Some(Ok(snapshot)) => {
                        model.replace_entries(entries(&snapshot.rows, &orders.screen));
                    }
                    Some(Err(error)) => {
                        model.notice = Some(notice("Harold stream failed", error.detail()));
                        stream = None;
                    }
                    None => {
                        model.notice = Some("Harold stream closed".to_string());
                        stream = None;
                    }
                }
            }
            input = keys.next_input() => {
                let key = match input {
                    Some(Ok(RuntimeInput::Key(key))) => key,
                    Some(Ok(RuntimeInput::Redraw | RuntimeInput::Focus(_))) => {
                        screen.show(&model)?;
                        continue;
                    }
                    Some(Err(detail)) => return Err(AppError::Input(detail)),
                    None => return Err(AppError::Input("terminal input worker stopped".into())),
                };
                match model.key(key) {
                    Action::Close => return Ok(()),
                    Action::Jump(index) => {
                        if jump(&mut model, index, navigator) {
                            // The remote closes after a jump, so the call is awaited, not
                            // spawned: a spawned task would end with the runtime. It waits
                            // at most VOICE_REQUEST_TIMEOUT (2 s). Every error is ignored.
                            if let (Some(stream), Some(entry)) = (&stream, model.entries.get(index)) {
                                let _ = stream.agent_seen(&entry.pane_id).await;
                            }
                            return Ok(());
                        }
                    }
                    Action::Move { order: moved } => {
                        // The full order goes to Harold, so a stopped PnEM keeps its place.
                        // The screen keeps the new order even when the save fails.
                        let sent = orders.to_save(&moved);
                        let saved = match &stream {
                            Some(stream) => stream.save_remote_order(&sent).await,
                            None => Err(SourceError::Transport("Harold is not connected".into())),
                        };
                        match saved {
                            Ok(()) => orders.saved(sent),
                            Err(error) => {
                                orders.not_saved(sent);
                                model.notice = Some(notice("could not save the remote order", error.detail()));
                            }
                        }
                    }
                    Action::None => {}
                }
            }
            () = shutdown.recv_shutdown() => return Ok(()),
        }
        screen.show(&model)?;
    }
}

/// Jumps to the entry at `index`. True when the jump worked; otherwise the notice tells
/// the failed step and the remote stays open.
fn jump(model: &mut Model, index: usize, navigator: &TmuxNavigator) -> bool {
    let Some(entry) = model.entries.get(index) else {
        return false;
    };
    if entry.block_id.is_none() {
        model.notice = Some("this agent is not in a Rex pane".to_string());
        return false;
    }
    match navigator.focus_rex_block(&entry.session, &entry.pane_id) {
        Ok(()) => true,
        Err(error) => {
            model.notice = Some(sanitize_display(&error.to_string(), NOTICE_LIMIT));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AgentIncarnation, AgentState};

    fn row(pane_id: &str, session: &str, role: &str, product: &str, host: &str) -> AgentRow {
        AgentRow {
            waiting: false,
            waiting_since_ms: 0,
            incarnation: AgentIncarnation {
                pane_id: pane_id.into(),
                pane_pid: 1,
                agent_pid: 2,
                agent_started_at_ms: 3,
                provider_id: "agent".into(),
            },
            provider_display_name: "Agent".into(),
            tmux_target: String::new(),
            session_name: session.into(),
            window_index: 0,
            pane_index: 0,
            working_directory: "/w".into(),
            work_summary: None,
            current_step: None,
            recap: None,
            recap_at_ms: 0,
            state: AgentState::Idle,
            last_transition_at_ms: 0,
            host: host.into(),
            host_icon: String::new(),
            window_name: String::new(),
            role: role.into(),
            pod_product: product.into(),
        }
    }

    fn chief() -> AgentRow {
        row("block:chief", "chief", "chief", "", "")
    }

    fn pnem(product: &str) -> AgentRow {
        row(&format!("block:{product}"), product, "pnem", product, "")
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn labels(list: &[Entry]) -> Vec<&str> {
        list.iter().map(|e| e.label.as_str()).collect()
    }

    fn model(products: &[&str]) -> Model {
        let rows: Vec<AgentRow> = std::iter::once(chief())
            .chain(products.iter().map(|p| pnem(p)))
            .collect();
        Model::new(entries(&rows, &names(products)))
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        let mods = if c.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        KeyEvent::new(KeyCode::Char(c), mods)
    }

    #[test]
    fn the_chief_is_first_then_pnems_in_the_given_order() {
        let rows = [pnem("a"), pnem("b"), chief(), pnem("c")];
        let list = entries(&rows, &names(&["c", "a", "b"]));
        assert_eq!(labels(&list), ["Chief", "PnEM · c", "PnEM · a", "PnEM · b"]);
        assert!(list[0].is_chief);
        assert!(!list[1].is_chief);
        assert_eq!(list[1].session, "c");
        assert_eq!(list[1].block_id.as_deref(), Some("c"));
        assert_eq!(list[1].product, "c");
    }

    #[test]
    fn a_pnem_not_in_the_order_comes_last() {
        let rows = [chief(), pnem("x"), pnem("a"), pnem("y")];
        let list = entries(&rows, &names(&["a"]));
        assert_eq!(labels(&list), ["Chief", "PnEM · a", "PnEM · x", "PnEM · y"]);
    }

    #[test]
    fn host_rows_and_other_roles_are_not_listed() {
        let rows = [
            row("block:h", "s", "chief", "", "omarchy"),
            chief(),
            row("block:p", "s", "pnem", "far", "omarchy"),
            row("block:k", "s", "coordinator", "harold", ""),
            row("block:o", "s", "", "", ""),
            pnem("a"),
        ];
        let list = entries(&rows, &names(&["a"]));
        assert_eq!(labels(&list), ["Chief", "PnEM · a"]);
    }

    #[test]
    fn only_one_chief_is_listed() {
        let rows = [chief(), row("block:c2", "s", "chief", "", "")];
        assert_eq!(entries(&rows, &[]).len(), 1);
    }

    #[test]
    fn no_pnem_running_lists_only_the_chief() {
        let list = entries(&[chief()], &names(&["a"]));
        assert_eq!(labels(&list), ["Chief"]);
    }

    #[test]
    fn no_chief_found_lists_the_pnems_numbered_from_0() {
        let mut m = Model::new(entries(&[pnem("a"), pnem("b")], &names(&["a", "b"])));
        assert_eq!(labels(&m.entries), ["PnEM · a", "PnEM · b"]);
        assert_eq!(m.key(ch('0')), Action::Jump(0));
        assert_eq!(m.key(ch('1')), Action::Jump(1));
    }

    #[test]
    fn two_pnems_with_the_same_product_keep_row_order() {
        let rows = [chief(), pnem("a"), pnem("a")];
        let mut second = rows.to_vec();
        second[2].incarnation.pane_id = "block:second".into();
        let list = entries(&second, &names(&["a"]));
        assert_eq!(list.len(), 3);
        assert_eq!(list[1].pane_id, "block:a");
        assert_eq!(list[2].pane_id, "block:second");
    }

    #[test]
    fn digit_jumps_to_that_number() {
        let mut m = model(&["a", "b"]);
        assert_eq!(m.key(ch('0')), Action::Jump(0));
        assert_eq!(m.key(ch('2')), Action::Jump(2));
    }

    #[test]
    fn a_digit_with_no_entry_does_nothing() {
        let mut m = model(&["a"]);
        assert_eq!(m.key(ch('5')), Action::None);
    }

    #[test]
    fn j_and_k_clamp_at_the_ends() {
        let mut m = model(&["a", "b"]);
        assert_eq!(m.key(ch('k')), Action::None);
        assert_eq!(m.selected, 0);
        m.key(ch('j'));
        m.key(ch('j'));
        m.key(ch('j'));
        assert_eq!(m.selected, 2);
        m.key(ch('k'));
        assert_eq!(m.selected, 1);
    }

    #[test]
    fn shift_j_moves_the_pnem_down_and_returns_the_new_order() {
        let mut m = model(&["a", "b", "c"]);
        m.key(ch('j'));
        assert_eq!(
            m.key(ch('J')),
            Action::Move {
                order: names(&["b", "a", "c"])
            }
        );
        assert_eq!(
            labels(&m.entries),
            ["Chief", "PnEM · b", "PnEM · a", "PnEM · c"]
        );
        assert_eq!(m.selected, 2);
    }

    #[test]
    fn shift_k_moves_the_pnem_up() {
        let mut m = model(&["a", "b", "c"]);
        m.selected = 3;
        assert_eq!(
            m.key(ch('K')),
            Action::Move {
                order: names(&["a", "c", "b"])
            }
        );
        assert_eq!(m.selected, 2);
    }

    #[test]
    fn shift_k_on_the_first_pnem_does_nothing() {
        let mut m = model(&["a", "b"]);
        m.selected = 1;
        assert_eq!(m.key(ch('K')), Action::None);
        assert_eq!(m.selected, 1);
        assert_eq!(labels(&m.entries), ["Chief", "PnEM · a", "PnEM · b"]);
    }

    #[test]
    fn shift_j_on_the_last_pnem_does_nothing() {
        let mut m = model(&["a", "b"]);
        m.selected = 2;
        assert_eq!(m.key(ch('J')), Action::None);
        assert_eq!(m.selected, 2);
    }

    #[test]
    fn the_chief_row_never_moves() {
        let mut m = model(&["a", "b"]);
        assert_eq!(m.key(ch('J')), Action::None);
        assert_eq!(m.key(ch('K')), Action::None);
        assert_eq!(m.selected, 0);
        assert!(m.entries[0].is_chief);
    }

    #[test]
    fn shift_k_on_the_only_chief_does_nothing() {
        let mut m = model(&[]);
        assert_eq!(m.key(ch('K')), Action::None);
        assert_eq!(m.key(ch('J')), Action::None);
    }

    #[test]
    fn enter_jumps_to_the_selected_entry() {
        let mut m = model(&["a", "b"]);
        m.key(ch('j'));
        assert_eq!(m.key(press(KeyCode::Enter)), Action::Jump(1));
    }

    #[test]
    fn enter_on_an_empty_list_does_nothing() {
        let mut m = Model::new(Vec::new());
        assert_eq!(m.key(press(KeyCode::Enter)), Action::None);
        assert_eq!(m.key(ch('0')), Action::None);
        assert_eq!(m.key(ch('j')), Action::None);
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn esc_closes() {
        let mut m = model(&["a"]);
        assert_eq!(m.key(press(KeyCode::Esc)), Action::Close);
    }

    #[test]
    fn with_more_than_ten_entries_only_digits_0_to_9_jump() {
        let products: Vec<String> = (0..11).map(|i| format!("p{i}")).collect();
        let refs: Vec<&str> = products.iter().map(String::as_str).collect();
        let mut m = model(&refs);
        assert_eq!(m.entries.len(), 12);
        assert_eq!(m.key(ch('9')), Action::Jump(9));
        for _ in 0..11 {
            m.key(ch('j'));
        }
        assert_eq!(m.selected, 11);
        assert_eq!(m.key(press(KeyCode::Enter)), Action::Jump(11));
    }

    #[test]
    fn replace_entries_keeps_the_selection_on_the_same_pane() {
        let mut m = model(&["a", "b", "c"]);
        m.selected = 2;
        let rows = [chief(), pnem("c"), pnem("a"), pnem("b")];
        m.replace_entries(entries(&rows, &names(&["c", "a", "b"])));
        assert_eq!(m.entries[m.selected].pane_id, "block:b");
    }

    #[test]
    fn replace_entries_clamps_when_the_pane_is_gone() {
        let mut m = model(&["a", "b", "c"]);
        m.selected = 3;
        m.replace_entries(entries(&[chief(), pnem("a")], &names(&["a"])));
        assert_eq!(m.selected, 1);
        m.replace_entries(Vec::new());
        assert_eq!(m.selected, 0);
    }

    /// Keys that are not a digit, `j`, `k`, `J`, `K`, Enter or Esc.
    fn other_keys() -> Vec<KeyEvent> {
        let mut keys = vec![
            ch('a'),
            ch('n'),
            ch('x'),
            press(KeyCode::Tab),
            ch(' '),
            press(KeyCode::Backspace),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            press(KeyCode::Up),
            press(KeyCode::Down),
            press(KeyCode::Left),
            press(KeyCode::Right),
        ];
        keys.extend((1..=12).map(|n| press(KeyCode::F(n))));
        keys
    }

    #[test]
    fn no_other_key_does_anything() {
        for key in other_keys() {
            let mut m = model(&["a", "b"]);
            m.selected = 1;
            m.notice = Some("jump failed".into());
            let before = m.entries.clone();
            assert_eq!(m.key(key), Action::None, "{key:?}");
            assert_eq!(m.selected, 1, "{key:?}");
            assert_eq!(m.entries, before, "{key:?}");
            assert_eq!(m.notice.as_deref(), Some("jump failed"), "{key:?}");
        }
    }

    #[test]
    fn a_digit_with_no_entry_and_a_blocked_move_stay_silent() {
        let mut m = model(&["a", "b"]);
        let before = m.entries.clone();
        assert_eq!(m.key(ch('7')), Action::None);
        // The chief never moves.
        assert_eq!(m.key(ch('J')), Action::None);
        m.selected = 1;
        // The first PnEM never moves above the chief.
        assert_eq!(m.key(ch('K')), Action::None);
        m.selected = 2;
        // The last PnEM cannot move down.
        assert_eq!(m.key(ch('J')), Action::None);
        assert_eq!(m.entries, before);
        assert_eq!(m.selected, 2);
        assert_eq!(m.notice, None);
    }

    #[test]
    fn a_notice_stays_until_a_key_acts() {
        let mut m = model(&["a", "b"]);
        m.notice = Some("jump failed".into());
        // A digit with no entry and a blocked move do nothing, so the notice stays.
        assert_eq!(m.key(ch('7')), Action::None);
        assert_eq!(m.key(ch('J')), Action::None);
        assert_eq!(m.notice.as_deref(), Some("jump failed"));
        // A key that acts clears it.
        assert_eq!(m.key(ch('j')), Action::None);
        assert_eq!(m.notice, None);
        m.notice = Some("jump failed".into());
        assert_eq!(m.key(ch('1')), Action::Jump(1));
        assert_eq!(m.notice, None);
        m.notice = Some("jump failed".into());
        assert!(matches!(m.key(ch('J')), Action::Move { .. }));
        assert_eq!(m.notice, None);
    }

    #[test]
    fn key_releases_do_nothing() {
        let mut m = model(&["a"]);
        let mut release = ch('0');
        release.kind = KeyEventKind::Release;
        assert_eq!(m.key(release), Action::None);
    }

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(model: &Model, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, model)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn sample() -> Model {
        let mut m = model(&["harold", "keymap", "buckwal"]);
        m.selected = 1;
        m
    }

    #[test]
    fn draw_matches_the_mock_at_60_columns() {
        let rows = render(&sample(), 60, 10);
        let inner = |text: &str| format!("\u{2502}{text:<58}\u{2502}");
        assert_eq!(rows[0].chars().count(), 60);
        assert!(rows[0].starts_with("\u{250c}\u{2500} Harold: remote control \u{2500}"));
        assert!(rows[0].ends_with('\u{2510}'));
        assert_eq!(rows[1], inner(""));
        assert_eq!(rows[2], inner("  0   Chief"));
        assert_eq!(rows[3], inner("\u{25b8} 1   PnEM \u{b7} harold"));
        assert_eq!(rows[4], inner("  2   PnEM \u{b7} keymap"));
        assert_eq!(rows[5], inner("  3   PnEM \u{b7} buckwal"));
        assert_eq!(rows[6], inner(""));
        assert_eq!(rows[7], inner("  0-9 jump   j/k select   J/K move row"));
        assert_eq!(rows[8], inner("  Enter jump   Esc close"));
    }

    #[test]
    fn the_box_fills_a_taller_area() {
        let rows = render(&sample(), 60, 12);
        assert!(rows[11].starts_with('\u{2514}'));
        assert!(rows[10].contains("Enter jump"));
    }

    #[test]
    fn draw_works_at_40_and_80_columns() {
        for width in [40u16, 80] {
            let rows = render(&sample(), width, 12);
            assert!(rows[0].contains("Harold: remote control"));
            assert!(rows[3].contains("\u{25b8} 1   PnEM \u{b7} harold"));
            assert!(rows.iter().all(|r| r.chars().count() == width as usize));
        }
    }

    #[test]
    fn only_the_selected_row_has_the_marker() {
        let text = render(&sample(), 60, 12).join("\n");
        assert_eq!(text.matches('\u{25b8}').count(), 1);
    }

    #[test]
    fn a_row_shows_no_state_word() {
        let text = render(&sample(), 60, 12).join("\n");
        for word in ["idle", "working", "blocked"] {
            assert!(!text.contains(word), "found {word}");
        }
    }

    #[test]
    fn the_notice_replaces_the_last_line() {
        let mut m = sample();
        m.notice = Some("jump failed: no pane".into());
        let text = render(&m, 60, 12).join("\n");
        assert!(text.contains("jump failed: no pane"));
        assert!(!text.contains("Enter jump"));
        assert!(text.contains("0-9 jump"));
    }

    #[test]
    fn long_labels_are_cut_at_40_columns() {
        let mut m = sample();
        m.entries[1].label = "PnEM \u{b7} a-product-with-a-very-long-name-indeed".into();
        let rows = render(&m, 40, 12);
        assert!(rows.iter().all(|r| r.chars().count() == 40));
        assert!(rows[3].starts_with("\u{2502}\u{25b8} 1   PnEM \u{b7} a-product"));
        assert!(rows[3].ends_with('\u{2502}'));
        assert!(!rows.join("\n").contains("indeed"));
        assert_eq!(rows[8].chars().count(), 40);
    }

    #[test]
    fn an_empty_list_draws_no_rows_and_the_hint() {
        let m = Model::new(Vec::new());
        let rows = render(&m, 60, 9);
        assert_eq!(rows[2], format!("\u{2502}{:<58}\u{2502}", ""));
        let text = rows.join("\n");
        assert!(text.contains("0-9 jump"));
        assert!(text.contains("Esc close"));
    }

    #[test]
    fn a_very_small_area_does_not_panic() {
        for (w, h) in [(20, 5), (1, 1), (3, 2), (40, 3), (0, 0)] {
            let _ = render(&sample(), w, h);
        }
    }

    // ---- The run loop, with a fake Harold, fake keys and a scripted `rex`. ----

    use crate::api::SourceError;
    use crate::app::Snapshot;
    use crate::navigation::fake_rex::{self, VIEW, steps};
    use crate::runtime::{BoxFuture, RuntimeInput, ShutdownPort};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::time::Duration;
    use tokio::sync::Notify;

    /// A run that has not ended after this time stays open: every fake answers at once.
    const STAYS_OPEN: Duration = Duration::from_millis(300);
    const OWN_BLOCK: &str = "block:01f9";
    const CLOSE: Result<&str, &str> = Ok(r#"{"revision":216}"#);

    struct FakeStream {
        snapshots: VecDeque<Snapshot>,
        /// Sent after the first save of the order, to check that the screen keeps its order.
        after_save: VecDeque<Snapshot>,
        order: Result<Vec<String>, SourceError>,
        save: Result<(), SourceError>,
        saved: Rc<RefCell<Vec<Vec<String>>>>,
        drained: Rc<Notify>,
        /// The pane id of each `AgentSeen` call.
        seen: Rc<RefCell<Vec<String>>>,
        /// The answer to each `AgentSeen` call.
        seen_answer: Result<(), SourceError>,
    }

    impl RemoteStream for FakeStream {
        fn receive(&mut self) -> LocalFuture<'_, Option<Result<Snapshot, SourceError>>> {
            Box::pin(async move {
                if let Some(snapshot) = self.snapshots.pop_front() {
                    return Some(Ok(snapshot));
                }
                if !self.saved.borrow().is_empty()
                    && let Some(snapshot) = self.after_save.pop_front()
                {
                    return Some(Ok(snapshot));
                }
                self.drained.notify_one();
                std::future::pending().await
            })
        }

        fn remote_order(&self) -> LocalFuture<'_, Result<Vec<String>, SourceError>> {
            let order = self.order.clone();
            Box::pin(async move { order })
        }

        fn save_remote_order<'a>(
            &'a self,
            products: &'a [String],
        ) -> LocalFuture<'a, Result<(), SourceError>> {
            self.saved.borrow_mut().push(products.to_vec());
            let save = self.save.clone();
            Box::pin(async move { save })
        }

        fn agent_seen<'a>(&'a self, pane_id: &'a str) -> LocalFuture<'a, Result<(), SourceError>> {
            self.seen.borrow_mut().push(pane_id.to_string());
            let answer = self.seen_answer.clone();
            Box::pin(async move { answer })
        }
    }

    struct FakeSource {
        stream: RefCell<Option<FakeStream>>,
    }

    impl RemoteSource for FakeSource {
        type Stream = FakeStream;

        fn open(&self) -> LocalFuture<'_, Result<FakeStream, SourceError>> {
            let stream = self.stream.borrow_mut().take();
            Box::pin(async move {
                stream.ok_or_else(|| SourceError::Transport("connection refused".into()))
            })
        }
    }

    struct FakeKeys {
        keys: VecDeque<KeyEvent>,
        /// When set, the keys wait until the stream has sent every snapshot.
        gate: Option<Rc<Notify>>,
        /// When true, the input ends after the last key; otherwise it waits.
        ends: bool,
    }

    impl RemoteKeys for FakeKeys {
        fn next_input(&mut self) -> LocalFuture<'_, Option<Result<RuntimeInput, String>>> {
            Box::pin(async move {
                if let Some(gate) = self.gate.clone() {
                    gate.notified().await;
                    self.gate = None;
                }
                match self.keys.pop_front() {
                    Some(key) => Some(Ok(RuntimeInput::Key(key))),
                    None if self.ends => None,
                    None => std::future::pending().await,
                }
            })
        }
    }

    struct FakeShutdown {
        fires: bool,
    }

    impl ShutdownPort for FakeShutdown {
        fn recv_shutdown(&mut self) -> BoxFuture<'_, ()> {
            let fires = self.fires;
            Box::pin(async move {
                if !fires {
                    std::future::pending::<()>().await;
                }
            })
        }
    }

    #[derive(Default)]
    struct FakeScreen {
        shown: Vec<Model>,
    }

    impl RemoteScreen for FakeScreen {
        fn show(&mut self, model: &Model) -> Result<(), crate::terminal::TerminalError> {
            self.shown.push(model.clone());
            Ok(())
        }
    }

    struct Case {
        snapshots: Vec<Snapshot>,
        after_save: Vec<Snapshot>,
        order: Result<Vec<String>, SourceError>,
        save: Result<(), SourceError>,
        seen_answer: Result<(), SourceError>,
        harold_down: bool,
        keys: Vec<KeyEvent>,
        wait_for_snapshots: bool,
        input_ends: bool,
        shutdown: bool,
        rex: Vec<Result<&'static str, &'static str>>,
        session: Option<&'static str>,
        block: Option<&'static str>,
    }

    impl Default for Case {
        fn default() -> Self {
            Case {
                snapshots: vec![snapshot(1, vec![chief(), pnem("a"), pnem("b")])],
                after_save: Vec::new(),
                order: Ok(names(&["a", "b"])),
                save: Ok(()),
                seen_answer: Ok(()),
                harold_down: false,
                keys: Vec::new(),
                wait_for_snapshots: true,
                input_ends: false,
                shutdown: false,
                rex: Vec::new(),
                session: Some("session:01a1"),
                block: Some(OWN_BLOCK),
            }
        }
    }

    struct Outcome {
        /// `None` when the remote was still open at the end of the test.
        result: Option<Result<(), AppError>>,
        rex: fake_rex::Calls,
        saved: Vec<Vec<String>>,
        seen: Vec<String>,
        last: Model,
    }

    impl Outcome {
        fn steps(&self) -> Vec<String> {
            steps(&self.rex)
        }
    }

    fn snapshot(version: u64, rows: Vec<AgentRow>) -> Snapshot {
        Snapshot {
            focus_known: None,
            jobs: Vec::new(),
            through_event_version: version,
            server_time_ms: 0,
            monitor_health: Vec::new(),
            rows,
            tts_muted: false,
            messaging_paused: false,
            watches_running: 0,
            watches_paused: 0,
            watches_capped: 0,
            harold_version: None,
        }
    }

    fn sessions_named(name: &'static str) -> &'static str {
        Box::leak(
            format!(r#"{{"sessions":[{{"session_id":"session:s-{name}","label":"{name}"}}]}}"#)
                .into_boxed_str(),
        )
    }

    async fn run_case(case: Case) -> Outcome {
        let drained = Rc::new(Notify::new());
        let saved = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::new(RefCell::new(Vec::new()));
        let stream = FakeStream {
            snapshots: case.snapshots.into(),
            after_save: case.after_save.into(),
            order: case.order,
            save: case.save,
            saved: Rc::clone(&saved),
            drained: Rc::clone(&drained),
            seen: Rc::clone(&seen),
            seen_answer: case.seen_answer,
        };
        let source = FakeSource {
            stream: RefCell::new((!case.harold_down).then_some(stream)),
        };
        let mut keys = FakeKeys {
            keys: case.keys.into(),
            gate: case.wait_for_snapshots.then_some(drained),
            ends: case.input_ends,
        };
        let mut shutdown = FakeShutdown {
            fires: case.shutdown,
        };
        let mut screen = FakeScreen::default();
        let (navigator, rex) = fake_rex::navigator(case.session, case.rex);
        let result = tokio::time::timeout(
            STAYS_OPEN,
            run_with(
                &source,
                &mut keys,
                &mut shutdown,
                &mut screen,
                &navigator,
                case.block,
            ),
        )
        .await
        .ok();
        let saved = saved.borrow().clone();
        let seen = seen.borrow().clone();
        Outcome {
            result,
            rex,
            saved,
            seen,
            last: screen.shown.last().cloned().expect("the remote draws"),
        }
    }

    #[tokio::test]
    async fn a_digit_jumps_and_closes_the_layer() {
        let outcome = run_case(Case {
            order: Ok(names(&["b", "a"])),
            keys: vec![ch('1')],
            rex: vec![Ok(""), Ok(sessions_named("b")), Ok(""), Ok(VIEW), CLOSE],
            ..Case::default()
        })
        .await;

        assert!(matches!(outcome.result, Some(Ok(()))));
        assert_eq!(
            outcome.steps(),
            ["focus", "ls", "do", "session.view", "session.close_layer"]
        );
        let calls = outcome.rex.borrow();
        assert_eq!(calls[0][3..], ["--session=b", "block:b"]);
        assert_eq!(
            calls[4][5],
            r#"{"session_id":"session:01a1","layer_id":"layer:01c3"}"#
        );
    }

    #[tokio::test]
    async fn jump_sends_agent_seen() {
        let outcome = run_case(Case {
            order: Ok(names(&["b", "a"])),
            keys: vec![ch('1')],
            rex: vec![Ok(""), Ok(sessions_named("b")), Ok(""), Ok(VIEW), CLOSE],
            ..Case::default()
        })
        .await;

        assert!(matches!(outcome.result, Some(Ok(()))));
        assert_eq!(outcome.seen, ["block:b"]);
    }

    #[tokio::test]
    async fn a_failed_agent_seen_still_jumps_and_closes_the_layer() {
        let outcome = run_case(Case {
            keys: vec![ch('1')],
            seen_answer: Err(SourceError::Transport("status: Unimplemented".into())),
            rex: vec![Ok(""), Ok(sessions_named("a")), Ok(""), Ok(VIEW), CLOSE],
            ..Case::default()
        })
        .await;

        assert!(matches!(outcome.result, Some(Ok(()))));
        assert_eq!(outcome.seen, ["block:a"]);
        assert_eq!(
            outcome.steps(),
            ["focus", "ls", "do", "session.view", "session.close_layer"]
        );
    }

    #[tokio::test]
    async fn a_failed_jump_sends_no_agent_seen() {
        let outcome = run_case(Case {
            keys: vec![ch('1'), ch('5')],
            rex: vec![Err("no block has this id")],
            ..Case::default()
        })
        .await;

        assert!(outcome.result.is_none(), "the remote stays open");
        assert!(outcome.seen.is_empty());
    }

    #[tokio::test]
    async fn esc_closes_without_a_jump() {
        let outcome = run_case(Case {
            keys: vec![press(KeyCode::Esc)],
            rex: vec![Ok(VIEW), CLOSE],
            ..Case::default()
        })
        .await;

        assert!(matches!(outcome.result, Some(Ok(()))));
        assert_eq!(outcome.steps(), ["session.view", "session.close_layer"]);
    }

    #[tokio::test]
    async fn other_keys_do_nothing_and_the_remote_stays_open() {
        let outcome = run_case(Case {
            keys: other_keys(),
            ..Case::default()
        })
        .await;

        assert!(outcome.result.is_none(), "the remote stays open");
        assert!(outcome.rex.borrow().is_empty(), "no rex call");
        assert!(outcome.saved.is_empty());
        assert_eq!(outcome.last.selected, 0);
        assert_eq!(outcome.last.notice, None);
        assert_eq!(
            labels(&outcome.last.entries),
            ["Chief", "PnEM · a", "PnEM · b"]
        );
    }

    #[tokio::test]
    async fn a_failed_jump_shows_the_step_and_stays_open() {
        let outcome = run_case(Case {
            keys: vec![ch('1')],
            rex: vec![Err("no block has this id")],
            ..Case::default()
        })
        .await;

        assert!(outcome.result.is_none(), "the remote stays open");
        assert_eq!(outcome.steps(), ["focus"]);
        assert_eq!(
            outcome.last.notice.as_deref(),
            Some("could not focus Rex block: no block has this id")
        );
    }

    #[tokio::test]
    async fn a_jump_notice_is_cleaned_for_the_screen() {
        let outcome = run_case(Case {
            keys: vec![ch('1')],
            rex: vec![Err("bad\x1b[31m red")],
            ..Case::default()
        })
        .await;

        let notice = outcome.last.notice.expect("a notice");
        assert!(!notice.contains('\x1b'), "{notice:?}");
        assert!(notice.starts_with("could not focus Rex block: bad"));
    }

    #[tokio::test]
    async fn an_agent_that_is_not_in_a_rex_pane_is_not_jumped_to() {
        let outcome = run_case(Case {
            snapshots: vec![snapshot(1, vec![chief(), row("%3", "a", "pnem", "a", "")])],
            order: Ok(names(&["a"])),
            keys: vec![ch('1')],
            ..Case::default()
        })
        .await;

        assert!(outcome.result.is_none());
        assert!(outcome.rex.borrow().is_empty());
        assert_eq!(
            outcome.last.notice.as_deref(),
            Some("this agent is not in a Rex pane")
        );
    }

    #[tokio::test]
    async fn shift_j_calls_set_remote_order_with_the_new_order() {
        let outcome = run_case(Case {
            snapshots: vec![snapshot(1, vec![chief(), pnem("a"), pnem("b"), pnem("c")])],
            order: Ok(names(&["a", "b", "c"])),
            keys: vec![ch('j'), ch('J')],
            ..Case::default()
        })
        .await;

        assert!(outcome.result.is_none());
        assert_eq!(outcome.saved, [names(&["b", "a", "c"])]);
        assert_eq!(
            labels(&outcome.last.entries),
            ["Chief", "PnEM · b", "PnEM · a", "PnEM · c"]
        );
        assert_eq!(outcome.last.notice, None);
        assert!(outcome.rex.borrow().is_empty());
    }

    #[tokio::test]
    async fn shift_k_on_c_with_b_stopped_sends_the_full_order() {
        let outcome = run_case(Case {
            snapshots: vec![snapshot(1, vec![chief(), pnem("a"), pnem("c")])],
            order: Ok(names(&["a", "b", "c"])),
            keys: vec![ch('j'), ch('j'), ch('K')],
            ..Case::default()
        })
        .await;

        assert_eq!(outcome.saved, [names(&["c", "b", "a"])]);
        assert_eq!(
            labels(&outcome.last.entries),
            ["Chief", "PnEM · c", "PnEM · a"]
        );
        assert_eq!(outcome.last.notice, None);
    }

    #[tokio::test]
    async fn shift_j_on_a_with_b_stopped_sends_the_full_order() {
        let outcome = run_case(Case {
            snapshots: vec![snapshot(1, vec![chief(), pnem("a"), pnem("c")])],
            order: Ok(names(&["a", "b", "c"])),
            keys: vec![ch('j'), ch('J')],
            ..Case::default()
        })
        .await;

        assert_eq!(outcome.saved, [names(&["c", "b", "a"])]);
        assert_eq!(
            labels(&outcome.last.entries),
            ["Chief", "PnEM · c", "PnEM · a"]
        );
    }

    #[tokio::test]
    async fn with_no_full_order_a_move_sends_the_moved_list() {
        let outcome = run_case(Case {
            order: Ok(Vec::new()),
            keys: vec![ch('j'), ch('J')],
            ..Case::default()
        })
        .await;

        assert_eq!(outcome.saved, [names(&["b", "a"])]);
        assert_eq!(
            labels(&outcome.last.entries),
            ["Chief", "PnEM · b", "PnEM · a"]
        );
    }

    #[test]
    fn the_moved_products_fill_the_slots_of_the_running_products() {
        let full = names(&["a", "b", "c"]);
        assert_eq!(
            merge_order(&full, &names(&["c", "a"])),
            names(&["c", "b", "a"])
        );
        assert_eq!(
            merge_order(&full, &names(&["b", "a", "c"])),
            names(&["b", "a", "c"])
        );
        assert_eq!(merge_order(&[], &names(&["b", "a"])), names(&["b", "a"]));
        // A running product that is not in the full order joins at the end.
        assert_eq!(
            merge_order(&full, &names(&["x", "c", "a"])),
            names(&["c", "b", "a", "x"])
        );
        // Two PnEMs of one product count once.
        assert_eq!(
            merge_order(&full, &names(&["c", "c", "a"])),
            names(&["c", "b", "a"])
        );
    }

    #[test]
    fn a_failed_save_keeps_the_screen_and_the_stored_full_order() {
        let mut orders = Orders::new(names(&["a", "b", "c"]));
        let sent = orders.to_save(&names(&["c", "a"]));
        assert_eq!(sent, names(&["c", "b", "a"]));

        orders.not_saved(sent.clone());
        assert_eq!(orders.stored, names(&["a", "b", "c"]));
        assert_eq!(orders.screen, names(&["c", "b", "a"]));

        orders.saved(sent);
        assert_eq!(orders.stored, names(&["c", "b", "a"]));
        assert_eq!(orders.screen, names(&["c", "b", "a"]));
    }

    #[tokio::test]
    async fn a_failed_set_keeps_the_screen_order_and_shows_a_notice() {
        let rows = || vec![chief(), pnem("a"), pnem("b"), pnem("c")];
        let outcome = run_case(Case {
            snapshots: vec![snapshot(1, rows())],
            // A later snapshot must not put the old order back.
            after_save: vec![snapshot(2, rows())],
            order: Ok(names(&["a", "b", "c"])),
            save: Err(SourceError::Transport("status: Unavailable".into())),
            keys: vec![ch('j'), ch('J')],
            ..Case::default()
        })
        .await;

        assert!(outcome.result.is_none());
        assert_eq!(outcome.saved, [names(&["b", "a", "c"])]);
        assert_eq!(
            labels(&outcome.last.entries),
            ["Chief", "PnEM · b", "PnEM · a", "PnEM · c"]
        );
        assert_eq!(
            outcome.last.notice.as_deref(),
            Some("could not save the remote order: status: Unavailable")
        );
    }

    #[tokio::test]
    async fn an_older_harold_without_the_order_lists_the_row_order_with_no_notice() {
        // The api turns an `Unimplemented` answer into an empty order.
        let outcome = run_case(Case {
            order: Ok(Vec::new()),
            ..Case::default()
        })
        .await;

        assert!(outcome.result.is_none());
        assert_eq!(
            labels(&outcome.last.entries),
            ["Chief", "PnEM · a", "PnEM · b"]
        );
        assert_eq!(outcome.last.notice, None);
    }

    #[tokio::test]
    async fn a_failed_order_read_shows_a_notice_and_still_lists_the_rows() {
        let outcome = run_case(Case {
            order: Err(SourceError::Transport("status: Unavailable".into())),
            ..Case::default()
        })
        .await;

        assert_eq!(
            labels(&outcome.last.entries),
            ["Chief", "PnEM · a", "PnEM · b"]
        );
        assert_eq!(
            outcome.last.notice.as_deref(),
            Some("could not read the remote order: status: Unavailable")
        );
    }

    #[tokio::test]
    async fn harold_down_shows_a_notice_and_stays_open() {
        let outcome = run_case(Case {
            harold_down: true,
            wait_for_snapshots: false,
            ..Case::default()
        })
        .await;

        assert!(outcome.result.is_none());
        assert_eq!(
            outcome.last.notice.as_deref(),
            Some("Harold is not reachable: connection refused")
        );
    }

    #[tokio::test]
    async fn a_failed_close_layer_exits_quietly() {
        let outcome = run_case(Case {
            keys: vec![press(KeyCode::Esc)],
            rex: vec![Ok(VIEW), Err("not_found")],
            ..Case::default()
        })
        .await;

        assert!(matches!(outcome.result, Some(Ok(()))));
        assert_eq!(outcome.steps(), ["session.view", "session.close_layer"]);
    }

    #[tokio::test]
    async fn a_missing_rex_block_variable_exits_quietly() {
        let outcome = run_case(Case {
            keys: vec![press(KeyCode::Esc)],
            block: None,
            ..Case::default()
        })
        .await;

        assert!(matches!(outcome.result, Some(Ok(()))));
        assert!(outcome.rex.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_missing_rex_session_variable_exits_quietly() {
        let outcome = run_case(Case {
            keys: vec![press(KeyCode::Esc)],
            session: None,
            ..Case::default()
        })
        .await;

        assert!(matches!(outcome.result, Some(Ok(()))));
        assert!(outcome.rex.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_shutdown_signal_closes_the_layer_once() {
        let outcome = run_case(Case {
            wait_for_snapshots: false,
            shutdown: true,
            rex: vec![Ok(VIEW), CLOSE],
            ..Case::default()
        })
        .await;

        assert!(matches!(outcome.result, Some(Ok(()))));
        assert_eq!(outcome.steps(), ["session.view", "session.close_layer"]);
    }

    #[tokio::test]
    async fn a_fatal_input_error_closes_the_layer_and_returns_the_error() {
        let outcome = run_case(Case {
            input_ends: true,
            rex: vec![Ok(VIEW), CLOSE],
            ..Case::default()
        })
        .await;

        assert!(matches!(outcome.result, Some(Err(AppError::Input(_)))));
        assert_eq!(outcome.steps(), ["session.view", "session.close_layer"]);
    }
}
