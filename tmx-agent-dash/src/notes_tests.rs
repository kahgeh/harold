use super::*;

fn line(name: &str) -> NoteLine {
    NoteLine {
        name: name.into(),
        title: format!("Title {name}"),
        kind: "idea".into(),
    }
}

fn view_with(rows: &[&str]) -> NotesView {
    let (mut view, action) = NotesView::open();
    let NotesAction::Load { seq, .. } = action else {
        panic!("opening reads the recent list");
    };
    view.loaded(seq, Ok(rows.iter().map(|r| line(r)).collect()));
    view
}

fn load(key: NotesKey) -> (String, u64, bool) {
    match key {
        NotesKey::Action(NotesAction::Load {
            query,
            seq,
            debounce,
        }) => (query, seq, debounce),
        other => panic!("not a load: {other:?}"),
    }
}

#[test]
fn opening_reads_the_recent_list_at_once() {
    let (view, action) = NotesView::open();
    assert!(view.loading && !view.typing);
    assert_eq!(
        action,
        NotesAction::Load {
            query: String::new(),
            seq: 1,
            debounce: false
        }
    );
}

#[test]
fn j_and_k_move_within_the_rows_and_gg_and_g_go_to_the_ends() {
    let mut view = view_with(&["a", "b", "c"]);
    assert_eq!(view.handle_key(KeyCode::Char('j')), NotesKey::None);
    assert_eq!(view.cursor, 1);
    view.handle_key(KeyCode::Char('j'));
    view.handle_key(KeyCode::Char('j'));
    assert_eq!(view.cursor, 2, "no further than the last row");
    view.handle_key(KeyCode::Char('k'));
    assert_eq!(view.cursor, 1);
    view.handle_key(KeyCode::Char('g'));
    assert_eq!(view.cursor, 1, "a single g does nothing");
    view.handle_key(KeyCode::Char('g'));
    assert_eq!(view.cursor, 0);
    view.handle_key(KeyCode::Char('G'));
    assert_eq!(view.cursor, 2);
    view.handle_key(KeyCode::Up);
    assert_eq!(view.cursor, 1);
    view.handle_key(KeyCode::Down);
    assert_eq!(view.cursor, 2);
}

#[test]
fn g_and_capital_g_type_after_slash_and_leave_the_cursor() {
    let mut view = view_with(&["a", "b", "c"]);
    view.handle_key(KeyCode::Char('j'));
    view.handle_key(KeyCode::Char('/'));
    view.handle_key(KeyCode::Char('g'));
    view.handle_key(KeyCode::Char('g'));
    view.handle_key(KeyCode::Char('G'));
    assert_eq!(view.query, "ggG");
    assert_eq!(view.cursor, 1);
}

#[test]
fn a_key_between_two_g_clears_the_wait() {
    let mut view = view_with(&["a", "b", "c"]);
    view.handle_key(KeyCode::Char('G'));
    view.handle_key(KeyCode::Char('g'));
    view.handle_key(KeyCode::Char('k'));
    view.handle_key(KeyCode::Char('g'));
    assert_eq!(view.cursor, 1);
}

#[test]
fn typing_searches_after_a_pause_and_a_short_query_reads_the_recent_list() {
    let mut view = view_with(&["a"]);
    let (query, seq, debounce) = load(view.handle_key(KeyCode::Char('b')));
    assert_eq!((query.as_str(), seq, debounce), ("b", 2, false));
    let (query, seq, debounce) = load(view.handle_key(KeyCode::Char('u')));
    assert_eq!((query.as_str(), seq, debounce), ("bu", 3, true));
    assert!(view.typing);
}

#[test]
fn j_and_k_are_typed_once_typing_has_the_keys_and_slash_gives_them() {
    let mut view = view_with(&["a", "b"]);
    assert_eq!(view.handle_key(KeyCode::Char('/')), NotesKey::None);
    assert!(view.typing);
    let (query, ..) = load(view.handle_key(KeyCode::Char('j')));
    assert_eq!(query, "j");
    let (query, ..) = load(view.handle_key(KeyCode::Char('k')));
    assert_eq!(query, "jk");
    view.loaded(4, Ok(vec![line("a"), line("b")]));
    view.handle_key(KeyCode::Down);
    assert_eq!(view.cursor, 1, "the arrows move while typing");
}

#[test]
fn backspace_edits_the_query_and_does_nothing_on_an_empty_one() {
    let mut view = view_with(&["a"]);
    assert_eq!(view.handle_key(KeyCode::Backspace), NotesKey::None);
    view.handle_key(KeyCode::Char('x'));
    view.handle_key(KeyCode::Char('y'));
    let (query, ..) = load(view.handle_key(KeyCode::Backspace));
    assert_eq!(query, "x");
}

#[test]
fn esc_clears_a_query_then_closes() {
    let mut view = view_with(&["a"]);
    view.handle_key(KeyCode::Char('x'));
    view.handle_key(KeyCode::Char('y'));
    let (query, _, debounce) = load(view.handle_key(KeyCode::Esc));
    assert_eq!((query.as_str(), debounce), ("", false));
    assert!(!view.typing);
    assert_eq!(view.handle_key(KeyCode::Esc), NotesKey::Close);
}

#[test]
fn esc_while_typing_an_empty_query_leaves_typing_before_it_closes() {
    let mut view = view_with(&["a"]);
    view.handle_key(KeyCode::Char('/'));
    assert!(matches!(view.handle_key(KeyCode::Esc), NotesKey::Action(_)));
    assert!(!view.typing);
    assert_eq!(view.handle_key(KeyCode::Esc), NotesKey::Close);
}

#[test]
fn enter_opens_the_row_under_the_cursor_and_nothing_when_there_is_none() {
    let mut view = view_with(&["a", "b"]);
    view.handle_key(KeyCode::Char('j'));
    assert_eq!(
        view.handle_key(KeyCode::Enter),
        NotesKey::Action(NotesAction::Open { name: "b".into() })
    );
    let mut empty = view_with(&[]);
    assert_eq!(empty.handle_key(KeyCode::Enter), NotesKey::None);
}

#[test]
fn an_older_answer_is_ignored() {
    let mut view = view_with(&["a"]);
    view.handle_key(KeyCode::Char('x'));
    view.handle_key(KeyCode::Char('y'));
    view.loaded(2, Ok(vec![line("stale")]));
    assert_eq!(view.rows, vec![line("a")]);
    assert!(view.loading);
    view.loaded(3, Ok(vec![line("fresh")]));
    assert_eq!(view.rows, vec![line("fresh")]);
    assert!(view.searched && !view.loading);
}

#[test]
fn no_rows_say_why_and_a_failure_shows_its_words() {
    let mut view = view_with(&[]);
    assert_eq!(view.message.as_deref(), Some("No notes yet."));
    view.handle_key(KeyCode::Char('z'));
    view.handle_key(KeyCode::Char('z'));
    view.loaded(3, Ok(Vec::new()));
    assert_eq!(view.message.as_deref(), Some("No notes match."));
    view.handle_key(KeyCode::Char('z'));
    view.loaded(4, Err("Search is resting: the model didn't load.".into()));
    assert_eq!(
        view.message.as_deref(),
        Some("Search is resting: the model didn't load.")
    );
    assert!(view.rows.is_empty());
}

#[test]
fn the_list_holds_at_most_ten_rows() {
    let names: Vec<String> = (0..15).map(|i| format!("n{i}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(view_with(&refs).rows.len(), ROWS);
}

#[test]
fn a_row_is_drawn_without_terminal_controls_and_a_missing_title_is_the_name() {
    let row = NoteLine {
        name: "a-note".into(),
        title: String::new(),
        kind: "i\u{1b}[31mdea".into(),
    };
    assert_eq!(row.display(), ("idea".into(), "a-note".into()));
}
