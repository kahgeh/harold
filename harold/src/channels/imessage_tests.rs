use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use crate::channels::{split_body, truncate_body};
use crate::inbound::ownership::{
    NotifierRecord, Ownership, decide_ownership, delivered_text, error_text, fed_markers,
    notification_text, parse_marker,
};
use crate::store::{HaroldStore, TurnCompleted};
use crate::util::sanitise_for_applescript;

use super::{
    ChatDb, Cursors, Direction, FetchedRow, MarkedRow, NotificationPlan, RecentRows, TWIN_WINDOW,
    is_marked_as_harold, max_rowid_of, notification_message, notification_plan, parse_max_rowid,
    poll_chat_db, poll_db, read_start, record_row, send_script, start_cursors,
    start_listener_cursors, start_listener_reading, wait_for_start_row,
};

#[test]
fn split_body_no_question() {
    let (main, q) = split_body("Work is done. All good.");
    assert_eq!(main, "Work is done. All good.");
    assert_eq!(q, None);
}

#[test]
fn split_body_trailing_question() {
    let (main, q) = split_body("Build succeeded. Should I deploy?");
    assert_eq!(main, "Build succeeded.");
    assert_eq!(q, Some("Should I deploy?"));
}

#[test]
fn split_body_only_question() {
    let (main, q) = split_body("Should I deploy?");
    assert_eq!(main, "Should I deploy?");
    assert_eq!(q, None);
}

#[test]
fn split_body_multiple_sentences_with_question() {
    let (main, q) = split_body("Done. Tests pass. Ready to merge. Shall I open a PR?");
    assert_eq!(main, "Done. Tests pass. Ready to merge.");
    assert_eq!(q, Some("Shall I open a PR?"));
}

#[test]
fn truncate_body_caps_at_280_chars_and_flattens_newlines() {
    let short = "Hello world.\nDone.";
    assert_eq!(truncate_body(short), "Hello world. Done.");

    let long: String = "x".repeat(300);
    let result = truncate_body(&long);
    assert_eq!(result.len(), 280);

    // Multi-byte: caps at 280 *characters*, not bytes.
    let emoji_long: String = "\u{1F600}".repeat(300);
    let result = truncate_body(&emoji_long);
    assert_eq!(result.chars().count(), 280);
    assert!(result.len() > 280);
}

#[test]
fn sanitise_strips_newlines_and_continuation() {
    let result = sanitise_for_applescript("line1\nline2\r¬end");
    assert!(!result.contains('\n'));
    assert!(!result.contains('\r'));
    assert!(!result.contains('¬'));
    assert!(result.contains("line1"));
    assert!(result.contains("line2"));
}

#[test]
fn retry_after_main_only_resumes_with_the_missing_question() {
    let plan = notification_plan(
        &["🤖 [studio/harold:0.1] Work is done. (harold)"],
        "[studio/harold:0.1] Work is done. (harold)",
        Some("Should I deploy?"),
    );

    assert_eq!(plan, NotificationPlan::SendQuestionOnly("Should I deploy?"));
}

#[test]
fn retry_after_both_parts_skips_the_completed_notification() {
    let plan = notification_plan(
        &[
            "🤖 Should I deploy?",
            "🤖 [studio/harold:0.1] Work is done. (harold)",
        ],
        "[studio/harold:0.1] Work is done. (harold)",
        Some("Should I deploy?"),
    );

    assert_eq!(plan, NotificationPlan::Skip);
}

#[test]
fn matching_question_from_another_turn_sends_the_full_notification() {
    let plan = notification_plan(
        &[
            "🤖 Should I deploy?",
            "🤖 [studio/other:0.1] Different work. (other)",
        ],
        "[studio/harold:0.1] Work is done. (harold)",
        Some("Should I deploy?"),
    );

    assert_eq!(plan, NotificationPlan::SendAll);
}

/// The text Messages is told to send, as the listener later reads it from chat.db.
fn sent_text(text: &str) -> String {
    let script = send_script(text, "+61400000000");
    let body = script
        .strip_prefix("tell application \"Messages\" to send \"")
        .expect("send script prefix");
    let end = body
        .rfind("\" to buddy \"")
        .expect("send script recipient clause");
    body[..end].to_string()
}

#[test]
fn texts_that_name_the_machine_are_sent_in_the_forms_the_spec_gives() {
    let notification = notification_text("studio", "alir-app main:0.1", "Tests pass.", "alir");

    assert_eq!(
        sent_text(&notification),
        "🤖 [studio/alir-app main:0.1] Tests pass. (alir)"
    );
    assert_eq!(
        parse_marker(&sent_text(&notification)).map(|marker| marker.machine),
        Some("studio")
    );
    assert_eq!(
        sent_text(&delivered_text("studio", "alir-app main:0.1")),
        "🤖 ✓ Delivered to [studio/alir-app main:0.1]"
    );
    assert_eq!(
        sent_text(&error_text("studio", "No active agent sessions found.")),
        "🤖 [studio] No active agent sessions found."
    );
}

#[test]
fn listener_skips_every_message_harold_sends() {
    // Messages to the user's own number are stored as both sent and received rows,
    // and the listener reads both, so anything unmarked is routed back in as a reply.
    for text in [
        "✓ Delivered to [harold  main:0.3]",
        "No active pane found. Available: harold  main:0.3",
        "No active agent sessions found.",
        "[studio/harold:0.1] Work is done. (harold)",
        "Should I deploy?",
    ] {
        let sent = sent_text(text);
        assert!(
            is_marked_as_harold(sent.trim()),
            "{text:?} is sent as {sent:?} and would be read back as an inbound message"
        );
        assert!(sent.ends_with(text), "{sent:?} lost its body");
    }
}

#[test]
fn send_script_escapes_quotes_and_backslashes_after_marking() {
    assert_eq!(
        send_script("say \"hi\" \\ bye", "bud\"dy"),
        "tell application \"Messages\" to send \"🤖 say \\\"hi\\\" \\\\ bye\" to buddy \"bud\\\"dy\""
    );
}

#[test]
fn listener_keeps_messages_the_user_wrote() {
    for text in ["Yes", "[harold  main:0.3] carry on", "✓ sounds good"] {
        assert!(!is_marked_as_harold(text), "{text:?}");
    }
}

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("harold-imessage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The handle id the test databases are read for.
const HANDLE: i64 = 7;

/// An empty database in `directory` with the columns of chat.db's `message` table that
/// the listener reads. `ROWID` is declared as chat.db declares it, so `sqlite3 -json`
/// names the column the same way. Marked rows are read, as when `machines` is set.
fn chat_db(directory: &TestDirectory) -> ChatDb {
    let db = chat_db_without_date(directory);
    add_date_column(&db);
    db
}

/// A database like `chat_db` whose `message` table has no `date` column, so the reply
/// queries succeed and the marked queries fail.
fn chat_db_without_date(directory: &TestDirectory) -> ChatDb {
    let db = ChatDb {
        path: directory.0.join("chat.db").to_str().unwrap().to_string(),
        handle_ids: vec![HANDLE],
        read_marked: true,
    };
    run_sql(
        &db,
        "CREATE TABLE message(ROWID INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT, \
         handle_id INTEGER, is_from_me INTEGER);",
    );
    db
}

/// `db` as a Mac whose `machines` setting is empty reads it: no marked row is read.
fn without_marked_rows(db: &ChatDb) -> ChatDb {
    ChatDb {
        read_marked: false,
        ..db.clone()
    }
}

fn add_date_column(db: &ChatDb) {
    run_sql(db, "ALTER TABLE message ADD COLUMN date INTEGER;");
}

fn run_sql(db: &ChatDb, sql: &str) {
    let status = std::process::Command::new("sqlite3")
        .arg(&db.path)
        .arg(sql)
        .status()
        .unwrap();
    assert!(status.success(), "{sql}");
}

/// Adds the date column while a listener may be reading the same file. sqlite3 refuses a
/// write at once, with "database is locked", while a reader holds the file, so this tries
/// again. Returns the last failure if it never succeeds.
fn add_date_column_while_read(db: &ChatDb) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..200 {
        let out = std::process::Command::new("sqlite3")
            .arg(&db.path)
            .arg("ALTER TABLE message ADD COLUMN date INTEGER;")
            .output()
            .map_err(|error| format!("sqlite3 did not start: {error}"))?;
        if out.status.success() {
            return Ok(());
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(last)
}

/// How long a test lets the listener's startup wait for a database before it fails.
/// Normal runs take well under a second.
const START_DEADLINE: Duration = Duration::from_secs(20);

/// Appends `rows`, each a (handle id, is_from_me, text), in order, with no date. The first
/// row of a new database gets ROWID 1.
fn insert(db: &ChatDb, rows: &[(i64, u8, &str)]) {
    for (handle_id, is_from_me, text) in rows {
        let text = text.replace('\'', "''");
        run_sql(
            db,
            &format!(
                "INSERT INTO message(text, handle_id, is_from_me) \
                 VALUES ('{text}', {handle_id}, {is_from_me});"
            ),
        );
    }
}

/// Appends `rows`, each a (handle id, is_from_me, text, date), in order.
fn insert_dated(db: &ChatDb, rows: &[(i64, u8, &str, i64)]) {
    for (handle_id, is_from_me, text, date) in rows {
        let text = text.replace('\'', "''");
        run_sql(
            db,
            &format!(
                "INSERT INTO message(text, handle_id, is_from_me, date) \
                 VALUES ('{text}', {handle_id}, {is_from_me}, {date});"
            ),
        );
    }
}

fn marked(rows: &[(i64, i64, &str)]) -> Vec<MarkedRow> {
    rows.iter()
        .map(|(rowid, date, text)| MarkedRow {
            rowid: *rowid,
            date: *date,
            text: text.to_string(),
        })
        .collect()
}

/// A notifier record for a Mac whose `machines` setting lists studio and laptop.
fn studio_and_laptop() -> Mutex<NotifierRecord> {
    Mutex::new(NotifierRecord::for_machines(&[
        "studio".to_string(),
        "laptop".to_string(),
    ]))
}

fn texts(rows: &[(i64, &str)]) -> Vec<(i64, String)> {
    rows.iter()
        .map(|(rowid, text)| (*rowid, text.to_string()))
        .collect()
}

#[test]
fn replies_are_read_per_direction_for_the_configured_handles_without_harolds_messages() {
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    insert(
        &db,
        &[
            (HANDLE, 0, "yes, merge it"),
            (HANDLE, 1, "🤖 [studio/alir-app main:0.1] Tests pass (alir)"),
            (HANDLE, 1, "typed on the Mac"),
            (99, 0, "from another conversation"),
            (HANDLE, 0, "🤖 [laptop/harold main:0.0] Build done (harold)"),
            (HANDLE, 0, "  and deploy  "),
        ],
    );

    assert_eq!(
        db.replies_after(0, 0),
        texts(&[(1, "yes, merge it"), (6, "and deploy")])
    );
    assert_eq!(db.replies_after(0, 1), texts(&[(3, "typed on the Mac")]));
    assert_eq!(db.replies_after(1, 0), texts(&[(6, "and deploy")]));

    let no_handles = ChatDb {
        handle_ids: Vec::new(),
        ..db.clone()
    };
    assert!(no_handles.replies_after(0, 0).is_empty());
}

const STUDIO_NOTIFICATION: &str = "🤖 [studio/alir-app main:0.1] Tests pass (alir)";
const LAPTOP_NOTIFICATION: &str = "🤖 [laptop/harold main:0.0] it's \"done\" (harold)";
const STUDIO_ERROR: &str = "🤖 [studio] No active agent sessions found.";

#[test]
fn marked_rows_are_read_in_both_directions_for_the_configured_handles() {
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    insert_dated(
        &db,
        &[
            (HANDLE, 1, STUDIO_NOTIFICATION, 500),
            (HANDLE, 0, "yes, merge it", 600),
        ],
    );
    insert(
        &db,
        &[
            (HANDLE, 0, LAPTOP_NOTIFICATION),
            (99, 0, "🤖 [mini/x:0.0] another conversation (x)"),
        ],
    );
    insert_dated(&db, &[(HANDLE, 1, STUDIO_ERROR, 400)]);
    run_sql(
        &db,
        "INSERT INTO message(text, handle_id, is_from_me) VALUES (NULL, 7, 0);",
    );

    // A row with no date reads as date 0.
    assert_eq!(
        db.marked_after(0),
        Ok(marked(&[
            (1, 500, STUDIO_NOTIFICATION),
            (3, 0, LAPTOP_NOTIFICATION),
            (5, 400, STUDIO_ERROR)
        ]))
    );
    assert_eq!(
        db.marked_after(1),
        Ok(marked(&[
            (3, 0, LAPTOP_NOTIFICATION),
            (5, 400, STUDIO_ERROR)
        ]))
    );
    // sqlite3 prints nothing when no row matches: that is no rows, not a failure.
    assert_eq!(db.marked_after(5), Ok(vec![]));

    // The last rows up to a row, oldest first.
    assert_eq!(
        db.latest_marked(6, 2),
        Ok(marked(&[
            (3, 0, LAPTOP_NOTIFICATION),
            (5, 400, STUDIO_ERROR)
        ]))
    );
    assert_eq!(
        db.latest_marked(4, 50),
        Ok(marked(&[
            (1, 500, STUDIO_NOTIFICATION),
            (3, 0, LAPTOP_NOTIFICATION)
        ]))
    );
    assert_eq!(db.latest_marked(0, 50), Ok(vec![]));

    let no_handles = ChatDb {
        handle_ids: Vec::new(),
        ..db.clone()
    };
    assert_eq!(no_handles.marked_after(0), Ok(vec![]));
    assert_eq!(no_handles.latest_marked(6, 50), Ok(vec![]));
}

#[test]
fn a_marked_read_that_fails_is_an_error_and_the_reply_read_is_no_rows() {
    let directory = TestDirectory::new();
    let db = chat_db_without_date(&directory);
    insert(&db, &[(HANDLE, 1, STUDIO_NOTIFICATION), (HANDLE, 0, "yes")]);
    let missing = ChatDb {
        path: directory
            .0
            .join("no-such-folder/chat.db")
            .to_str()
            .unwrap()
            .to_string(),
        ..db.clone()
    };

    let no_date = db.marked_after(0);
    assert!(
        no_date.as_ref().is_err_and(|cause| cause.contains("date")),
        "{no_date:?}"
    );
    assert!(db.latest_marked(2, 50).is_err());
    assert!(missing.marked_after(0).is_err());
    assert!(missing.latest_marked(2, 50).is_err());
    // The reply queries keep giving no rows when they fail.
    assert_eq!(db.replies_after(0, 0), texts(&[(2, "yes")]));
    assert!(missing.replies_after(0, 0).is_empty());
}

#[test]
fn startup_reads_the_marked_rows_already_stored_and_starts_every_cursor_after_them() {
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    insert_dated(
        &db,
        &[
            (HANDLE, 1, STUDIO_NOTIFICATION, 100),
            (HANDLE, 0, "yes, merge it", 150),
            (HANDLE, 0, LAPTOP_NOTIFICATION, 200),
        ],
    );
    let record = studio_and_laptop();

    let (initial, rows) = read_start(&db).unwrap();
    let cursors = start_cursors(initial, rows, |text, date| {
        record.lock().unwrap().record_marker(text, date);
    });

    assert_eq!(initial, 3);
    assert_eq!(record.lock().unwrap().latest_notifier(), Some("laptop"));
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 3);
    assert_eq!(cursors.self_sent.load(Ordering::Relaxed), 3);
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 3);
}

#[test]
fn startup_reads_only_the_last_50_marked_rows_oldest_first() {
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    let mut rows = vec![(HANDLE, 1, "🤖 [old/a:0.0] done (a)")];
    rows.extend([(HANDLE, 1, STUDIO_NOTIFICATION); 49]);
    rows.push((HANDLE, 0, LAPTOP_NOTIFICATION));
    insert(&db, &rows);
    let fed = Mutex::new(Vec::new());

    let (initial, rows) = read_start(&db).unwrap();
    start_cursors(initial, rows, |text, _| {
        fed.lock().unwrap().push(text.to_string());
    });

    let mut expected = vec![STUDIO_NOTIFICATION.to_string(); 49];
    expected.push(LAPTOP_NOTIFICATION.to_string());
    assert_eq!(*fed.lock().unwrap(), expected);
}

#[test]
fn startup_orders_the_latest_notifier_by_date_not_by_row() {
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    // The row read later was sent earlier.
    insert_dated(
        &db,
        &[
            (HANDLE, 1, STUDIO_NOTIFICATION, 200),
            (HANDLE, 0, LAPTOP_NOTIFICATION, 100),
        ],
    );
    let record = studio_and_laptop();

    let (initial, rows) = read_start(&db).unwrap();
    start_cursors(initial, rows, |text, date| {
        record.lock().unwrap().record_marker(text, date);
    });

    assert_eq!(record.lock().unwrap().latest_notifier(), Some("studio"));
}

#[tokio::test]
async fn poll_orders_the_latest_notifier_by_date_not_by_row() {
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    insert_dated(&db, &[(HANDLE, 1, STUDIO_NOTIFICATION, 200)]);
    let record = studio_and_laptop();
    let (initial, rows) = read_start(&db).unwrap();
    let feed = |text: &str, date| record.lock().unwrap().record_marker(text, date);
    let cursors = start_cursors(initial, rows, feed);
    let recent = Mutex::new(RecentRows::new());

    // Stored after the studio row but sent before it.
    insert_dated(&db, &[(HANDLE, 0, LAPTOP_NOTIFICATION, 150)]);
    poll_chat_db(&store, db.clone(), &cursors, feed, &recent, || false).await;
    assert_eq!(record.lock().unwrap().latest_notifier(), Some("studio"));
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 2);

    insert_dated(&db, &[(HANDLE, 0, LAPTOP_NOTIFICATION, 250)]);
    poll_chat_db(&store, db, &cursors, feed, &recent, || false).await;
    assert_eq!(record.lock().unwrap().latest_notifier(), Some("laptop"));
}

#[tokio::test]
async fn with_no_machines_listed_an_untagged_reply_after_notifications_is_owned() {
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    let record = Mutex::new(NotifierRecord::for_machines(&[]));
    let cursors = Cursors::starting_at(0);
    let recent = Mutex::new(RecentRows::new());
    insert_dated(
        &db,
        &[
            (
                HANDLE,
                1,
                "🤖 [studio/alir-app main:0.1] Two options. (alir)",
                100,
            ),
            (HANDLE, 1, "🤖 [1/2] Should I deploy?", 200),
            (HANDLE, 0, "yes", 300),
        ],
    );

    let feed = |text: &str, date| record.lock().unwrap().record_marker(text, date);
    poll_chat_db(&store, db, &cursors, feed, &recent, || false).await;
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);

    let record = record.lock().unwrap();
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 2);
    assert_eq!(record.latest_notifier(), None);
    assert_eq!(
        decide_ownership(
            "yes",
            false,
            "studio",
            record.known_machine_names(),
            record.latest_notifier()
        ),
        Ownership::Owned {
            pane_tag: None,
            body: "yes"
        }
    );
}

#[tokio::test]
async fn poll_that_cannot_read_the_marked_rows_records_no_reply_and_keeps_its_cursors() {
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    let db = chat_db_without_date(&directory);
    insert(&db, &[(HANDLE, 1, STUDIO_NOTIFICATION), (HANDLE, 0, "yes")]);
    let cursors = Cursors::starting_at(0);
    let recent = Mutex::new(RecentRows::new());

    // The closure is written in the call so that its return type is taken from the parameter.
    poll_chat_db(
        &store,
        db.clone(),
        &cursors,
        |text, _| panic!("{text:?} was read"),
        &recent,
        || false,
    )
    .await;

    assert_eq!(cursors.marked.load(Ordering::Relaxed), 0);
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 0);
    assert_eq!(cursors.self_sent.load(Ordering::Relaxed), 0);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);

    // Once the marked rows can be read, the next poll records the reply, once.
    add_date_column(&db);
    let record = studio_and_laptop();
    let feed = |text: &str, date| record.lock().unwrap().record_marker(text, date);
    poll_chat_db(&store, db.clone(), &cursors, feed, &recent, || false).await;
    poll_chat_db(&store, db, &cursors, feed, &recent, || false).await;

    assert_eq!(record.lock().unwrap().latest_notifier(), Some("studio"));
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 1);
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 2);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
}

#[tokio::test]
async fn with_no_machines_listed_no_marked_row_is_read_so_a_failing_marked_query_holds_nothing_back()
 {
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    // The marked queries would fail on this database: it has no `date` column.
    let db = without_marked_rows(&chat_db_without_date(&directory));
    insert(&db, &[(HANDLE, 1, STUDIO_NOTIFICATION), (HANDLE, 0, "yes")]);

    assert_eq!(read_start(&db), Ok((2, vec![])));
    assert_eq!(db.marked_after(0), Ok(vec![]));

    let cursors = Cursors::starting_at(0);
    let recent = Mutex::new(RecentRows::new());
    // The closure is written in the call so that its return type is taken from the parameter.
    poll_chat_db(
        &store,
        db,
        &cursors,
        |text, _| panic!("{text:?} was read"),
        &recent,
        || false,
    )
    .await;

    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 2);
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 0);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
}

#[tokio::test]
async fn poll_reads_marked_rows_before_it_records_a_reply_and_never_records_them() {
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    insert_dated(&db, &[(HANDLE, 1, STUDIO_NOTIFICATION, 100)]);
    let record = studio_and_laptop();
    let (initial, rows) = read_start(&db).unwrap();
    let cursors = start_cursors(initial, rows, |text, date| {
        record.lock().unwrap().record_marker(text, date);
    });
    let recent = Mutex::new(RecentRows::new());
    assert_eq!(record.lock().unwrap().latest_notifier(), Some("studio"));

    insert_dated(
        &db,
        &[
            (HANDLE, 0, LAPTOP_NOTIFICATION, 200),
            (HANDLE, 0, "yes, merge it", 300),
            (HANDLE, 1, STUDIO_ERROR, 400),
        ],
    );
    let before_any_reply = |text: &str, date| {
        assert_eq!(
            cursors.inbound.load(Ordering::Relaxed),
            1,
            "a reply was recorded before the marked rows were read"
        );
        record.lock().unwrap().record_marker(text, date);
    };
    poll_chat_db(
        &store,
        db.clone(),
        &cursors,
        before_any_reply,
        &recent,
        || false,
    )
    .await;

    assert_eq!(record.lock().unwrap().latest_notifier(), Some("laptop"));
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 4);
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 3);
    assert_eq!(cursors.self_sent.load(Ordering::Relaxed), 1);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);

    // Nothing new: no marked row is read a second time. The closure is written in the
    // call so that its return type is taken from the parameter.
    let read_again = "was read twice";
    poll_chat_db(
        &store,
        db,
        &cursors,
        |text, _| panic!("{text:?} {read_again}"),
        &recent,
        || false,
    )
    .await;
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);
}

#[tokio::test]
async fn paused_poll_still_reads_marked_rows_and_discards_the_reply() {
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    insert(
        &db,
        &[
            (HANDLE, 0, LAPTOP_NOTIFICATION),
            (HANDLE, 0, "yes, merge it"),
        ],
    );
    let record = studio_and_laptop();
    let cursors = Cursors::starting_at(0);
    let recent = Mutex::new(RecentRows::new());

    let feed = |text: &str, date| record.lock().unwrap().record_marker(text, date);
    poll_chat_db(&store, db, &cursors, feed, &recent, || true).await;

    assert_eq!(record.lock().unwrap().latest_notifier(), Some("laptop"));
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 1);
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 2);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);
}

#[tokio::test]
async fn poll_of_a_database_it_cannot_read_records_nothing_and_keeps_its_cursors() {
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    let not_a_database = directory.0.join("chat.db");
    std::fs::write(&not_a_database, "this is not a database").unwrap();
    let db = ChatDb {
        path: not_a_database.to_str().unwrap().to_string(),
        handle_ids: vec![HANDLE],
        read_marked: true,
    };
    let cursors = Cursors::starting_at(10);
    let recent = Mutex::new(RecentRows::new());

    // The closure is written in the call so that its return type is taken from the parameter.
    poll_chat_db(
        &store,
        db,
        &cursors,
        |text, _| panic!("{text:?} was read"),
        &recent,
        || false,
    )
    .await;

    assert_eq!(cursors.marked.load(Ordering::Relaxed), 10);
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 10);
    assert_eq!(cursors.self_sent.load(Ordering::Relaxed), 10);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);
}

fn row(rowid: i64, text: &str, direction: Direction) -> FetchedRow {
    FetchedRow {
        rowid,
        text: text.into(),
        direction,
    }
}

#[tokio::test]
async fn paused_listener_advances_the_cursor_and_appends_nothing() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let now = Instant::now();

    let confirmation = row(11, "✓ Delivered to [harold:0.1]", Direction::Inbound);
    record_row(&store, confirmation, &cursor, &recent, true, now).await;
    let reply = row(12, "carry on", Direction::Inbound);
    record_row(&store, reply, &cursor, &recent, true, now).await;

    assert_eq!(cursor.load(Ordering::Relaxed), 12);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);
    assert!(store.next_pending_delivery().await.unwrap().is_none());
}

#[tokio::test]
async fn running_listener_appends_the_row_and_advances_the_cursor() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());

    let reply = row(11, "carry on", Direction::Inbound);
    record_row(&store, reply, &cursor, &recent, false, Instant::now()).await;

    assert_eq!(cursor.load(Ordering::Relaxed), 11);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
    let delivery = store.next_pending_delivery().await.unwrap().unwrap();
    assert_eq!(delivery.event_type, "InboundMessageReceived");
}

#[test]
fn same_text_from_the_other_direction_within_the_window_is_a_duplicate() {
    let start = Instant::now();
    let soon = start + Duration::from_millis(5);

    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);
    assert!(recent.take_twin(Direction::SelfSent, "Yes", soon));

    let mut recent = RecentRows::new();
    recent.remember(Direction::SelfSent, "Yes".into(), start);
    assert!(recent.take_twin(Direction::Inbound, "Yes", start + TWIN_WINDOW));
}

#[test]
fn recorded_row_absorbs_only_one_twin() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    recent.remember(Direction::SelfSent, "Yes".into(), start);

    // The wrong direction neither matches nor uses the entry up.
    assert!(!recent.take_twin(Direction::SelfSent, "Yes", start));
    assert!(recent.take_twin(Direction::Inbound, "Yes", start));
    assert!(recent.0.is_empty());
    assert!(!recent.take_twin(Direction::Inbound, "Yes", start));
}

#[test]
fn twin_takes_the_oldest_matching_entry() {
    let start = Instant::now();
    let later = start + Duration::from_secs(6);
    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);
    recent.remember(Direction::SelfSent, "Yes".into(), start);
    recent.remember(Direction::Inbound, "Yes".into(), later);

    assert!(recent.take_twin(Direction::SelfSent, "Yes", later));
    assert_eq!(
        recent.0,
        [
            (Direction::SelfSent, "Yes".to_string(), start),
            (Direction::Inbound, "Yes".to_string(), later),
        ]
    );
}

#[test]
fn same_text_from_the_same_direction_is_kept() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);

    assert!(!recent.take_twin(Direction::Inbound, "Yes", start + Duration::from_millis(5)));
}

#[test]
fn different_text_from_the_other_direction_is_kept() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);

    assert!(!recent.take_twin(Direction::SelfSent, "Yes please", start));
    assert!(!recent.take_twin(Direction::SelfSent, "yes", start));
}

#[test]
fn same_text_from_the_other_direction_outside_the_window_is_kept() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    recent.remember(Direction::Inbound, "Yes".into(), start);

    let late = start + TWIN_WINDOW + Duration::from_millis(1);
    assert!(!recent.take_twin(Direction::SelfSent, "Yes", late));
}

#[test]
fn recent_rows_forget_entries_older_than_the_window() {
    let start = Instant::now();
    let mut recent = RecentRows::new();
    for second in 0..5 {
        recent.remember(
            Direction::Inbound,
            "Yes".into(),
            start + Duration::from_secs(second),
        );
    }
    assert_eq!(recent.0.len(), 5);

    // Both a lookup and a new entry drop what has aged out.
    recent.take_twin(
        Direction::SelfSent,
        "No",
        start + TWIN_WINDOW + Duration::from_millis(2_500),
    );
    assert_eq!(recent.0.len(), 2);
    recent.remember(Direction::SelfSent, "No".into(), start + TWIN_WINDOW * 2);
    assert_eq!(recent.0.len(), 1);
}

#[tokio::test]
async fn reply_stored_as_an_inbound_and_a_self_row_is_recorded_once() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let inbound_cursor = AtomicI64::new(10);
    let self_cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let now = Instant::now();

    // One poll: inbound rows first, then self rows.
    let inbound = row(12, "carry on", Direction::Inbound);
    record_row(&store, inbound, &inbound_cursor, &recent, false, now).await;
    let twin = row(11, "carry on", Direction::SelfSent);
    record_row(&store, twin, &self_cursor, &recent, false, now).await;

    assert_eq!(inbound_cursor.load(Ordering::Relaxed), 12);
    assert_eq!(self_cursor.load(Ordering::Relaxed), 11);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);

    // The copies can also land in different polls, in either order.
    let later = now + Duration::from_secs(5);
    let own = row(13, "and then stop", Direction::SelfSent);
    record_row(&store, own, &self_cursor, &recent, false, now).await;
    let twin = row(14, "and then stop", Direction::Inbound);
    record_row(&store, twin, &inbound_cursor, &recent, false, later).await;

    assert_eq!(self_cursor.load(Ordering::Relaxed), 13);
    assert_eq!(inbound_cursor.load(Ordering::Relaxed), 14);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
}

#[tokio::test]
async fn same_reply_sent_twice_from_the_phone_is_recorded_twice() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let now = Instant::now();

    record_row(
        &store,
        row(11, "Yes", Direction::Inbound),
        &cursor,
        &recent,
        false,
        now,
    )
    .await;
    record_row(
        &store,
        row(12, "Yes", Direction::Inbound),
        &cursor,
        &recent,
        false,
        now,
    )
    .await;

    assert_eq!(cursor.load(Ordering::Relaxed), 12);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 2);
}

#[tokio::test]
async fn row_discarded_by_the_pause_is_not_remembered() {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let inbound_cursor = AtomicI64::new(10);
    let self_cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let now = Instant::now();

    let discarded = row(11, "carry on", Direction::Inbound);
    record_row(&store, discarded, &inbound_cursor, &recent, true, now).await;
    assert!(recent.lock().unwrap().0.is_empty());

    // Messaging resumes before the other copy is read: it is the only one recorded.
    let twin = row(12, "carry on", Direction::SelfSent);
    record_row(&store, twin, &self_cursor, &recent, false, now).await;

    assert_eq!(self_cursor.load(Ordering::Relaxed), 12);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
}

/// Feeds rows through `record_row` in the given order, a second apart, and returns how
/// many were recorded.
async fn recorded_count(directions: &[Direction]) -> usize {
    let directory = TestDirectory::new();
    let store = HaroldStore::open(&directory.0).await.unwrap();
    let inbound_cursor = AtomicI64::new(10);
    let self_cursor = AtomicI64::new(10);
    let recent = Mutex::new(RecentRows::new());
    let start = Instant::now();

    let mut rowid = 10;
    for &direction in directions {
        rowid += 1;
        let cursor = match direction {
            Direction::Inbound => &inbound_cursor,
            Direction::SelfSent => &self_cursor,
        };
        let now = start + Duration::from_secs((rowid - 10) as u64);
        record_row(
            &store,
            row(rowid, "Yes", direction),
            cursor,
            &recent,
            false,
            now,
        )
        .await;
        assert_eq!(cursor.load(Ordering::Relaxed), rowid);
    }
    store.project_unhandled_events(10).await.unwrap().applied
}

#[tokio::test]
async fn phone_reply_after_the_same_text_from_the_mac_is_recorded() {
    use Direction::{Inbound, SelfSent};

    // The Mac pair in either arrival order, then a single row from the phone.
    assert_eq!(recorded_count(&[SelfSent, Inbound, Inbound]).await, 2);
    assert_eq!(recorded_count(&[Inbound, SelfSent, Inbound]).await, 2);
}

#[tokio::test]
async fn same_reply_typed_twice_on_the_mac_is_recorded_twice() {
    use Direction::{Inbound, SelfSent};

    for order in [
        [Inbound, SelfSent, Inbound, SelfSent],
        [Inbound, SelfSent, SelfSent, Inbound],
        [SelfSent, Inbound, Inbound, SelfSent],
        [SelfSent, Inbound, SelfSent, Inbound],
        // Both pairs read in one poll: inbound rows first, then self rows.
        [Inbound, Inbound, SelfSent, SelfSent],
    ] {
        assert_eq!(recorded_count(&order).await, 2, "{order:?}");
    }
}

#[test]
fn a_failed_read_of_the_newest_row_is_not_row_zero() {
    // sqlite3 exits non-zero when it may not open chat.db; that is not an empty table.
    assert_eq!(parse_max_rowid(false, b""), None);
    assert_eq!(parse_max_rowid(false, b"0\n"), None);
    assert_eq!(parse_max_rowid(true, b""), None);
    assert_eq!(
        parse_max_rowid(true, b"Error: unable to open database file\n"),
        None
    );
}

#[test]
fn a_successful_read_gives_the_newest_row_and_zero_only_for_an_empty_table() {
    assert_eq!(parse_max_rowid(true, b"2869\n"), Some(2869));
    assert_eq!(parse_max_rowid(true, b"0\n"), Some(0));
}

#[test]
fn the_real_query_tells_an_empty_table_from_a_database_it_cannot_read() {
    let dir = std::env::temp_dir().join(format!("harold-chatdb-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let sqlite = |db: &std::path::Path, sql: &str| {
        let status = std::process::Command::new("sqlite3")
            .arg(db)
            .arg(sql)
            .status()
            .unwrap();
        assert!(status.success());
    };
    let empty = dir.join("empty.db");
    sqlite(&empty, "CREATE TABLE message(text);");
    let two = dir.join("two.db");
    sqlite(
        &two,
        "CREATE TABLE message(text); INSERT INTO message VALUES ('a'), ('b');",
    );
    let not_a_database = dir.join("garbage.db");
    std::fs::write(&not_a_database, "this is not a database").unwrap();

    assert_eq!(max_rowid_of(empty.to_str().unwrap()), Ok(0));
    assert_eq!(max_rowid_of(two.to_str().unwrap()), Ok(2));
    let unreadable = max_rowid_of(not_a_database.to_str().unwrap());
    let missing = max_rowid_of(dir.join("no-such-folder/chat.db").to_str().unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(unreadable.is_err(), "{unreadable:?}");
    assert!(missing.is_err(), "{missing:?}");
    assert!(!missing.unwrap_err().is_empty());
}

#[tokio::test]
async fn the_listener_waits_for_a_readable_database_before_it_picks_its_start_row() {
    let (_keep, mut shutdown) = tokio::sync::watch::channel(());
    let mut answers = vec![
        Err("denied".to_string()),
        Err("denied".to_string()),
        Ok(2869),
    ]
    .into_iter();
    let mut reads = 0;
    let wait = wait_for_start_row(
        || {
            reads += 1;
            answers.next().unwrap()
        },
        Duration::from_millis(1),
        &mut shutdown,
    );
    let start = tokio::time::timeout(START_DEADLINE, wait)
        .await
        .expect("the wait never ended");
    assert_eq!(start, Some(2869));
    assert_eq!(reads, 3);
}

#[tokio::test]
async fn startup_read_that_fails_is_retried_and_its_rows_recorded_once() {
    let (_keep, mut shutdown) = tokio::sync::watch::channel(());
    let directory = TestDirectory::new();
    let db = chat_db_without_date(&directory);
    insert(
        &db,
        &[
            (HANDLE, 1, STUDIO_NOTIFICATION),
            (HANDLE, 0, LAPTOP_NOTIFICATION),
        ],
    );
    // The newest row can be read, the marked rows cannot until the third read.
    let mut reads = 0;
    let wait = wait_for_start_row(
        || {
            reads += 1;
            if reads == 3 {
                add_date_column(&db);
            }
            read_start(&db)
        },
        Duration::from_millis(1),
        &mut shutdown,
    );
    let start = tokio::time::timeout(START_DEADLINE, wait)
        .await
        .expect("the database never became readable");
    let (initial, rows) = start.expect("a start row");
    let fed = Mutex::new(Vec::new());
    let cursors = start_cursors(initial, rows, |text, date| {
        fed.lock().unwrap().push((text.to_string(), date));
    });

    assert_eq!(reads, 3);
    assert_eq!(
        *fed.lock().unwrap(),
        [
            (STUDIO_NOTIFICATION.to_string(), 0),
            (LAPTOP_NOTIFICATION.to_string(), 0)
        ]
    );
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn a_shutdown_while_the_database_is_unreadable_ends_the_wait_without_a_start_row() {
    // Harold signals shutdown by dropping the sender.
    let (stop, mut shutdown) = tokio::sync::watch::channel(());
    drop(stop);
    let wait = wait_for_start_row(
        || Err("denied".into()),
        Duration::from_secs(60),
        &mut shutdown,
    );
    let start = tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("the wait did not end on shutdown");
    assert_eq!(start, None::<i64>);
}

#[test]
fn notification_is_sent_naming_the_machine_and_the_pane() {
    let turn = TurnCompleted {
        pane_id: "%1".into(),
        pane_label: "alir-app main:0.1".into(),
        last_user_prompt: "prompt".into(),
        assistant_message: "message".into(),
        main_context: "alir".into(),
        agent_incarnation: None,
        work_summary: crate::agent::domain::CompletionSummaryUpdate::Unchanged,
    };

    let message = notification_message("studio", &turn, " Tests pass. ");

    assert_eq!(message, "[studio/alir-app main:0.1] Tests pass. (alir)");
    assert_eq!(
        sent_text(&message),
        "🤖 [studio/alir-app main:0.1] Tests pass. (alir)"
    );
}

// The test settings list no machines, so the process-wide record keeps nothing it is fed.
// These tests see what it was fed instead. The texts and dates are unique to each test.

#[tokio::test(flavor = "multi_thread")]
async fn listener_startup_seeds_the_process_wide_notifier_record() {
    crate::settings::init_settings_for_test();
    let text = "🤖 [seed-test-a/x main:0.0] done (x)";
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    insert_dated(&db, &[(HANDLE, 1, text, 9_001)]);
    let (_keep, mut shutdown) = tokio::sync::watch::channel(());

    tokio::time::timeout(START_DEADLINE, start_listener_cursors(&db, &mut shutdown))
        .await
        .expect("the database never became readable")
        .expect("cursors");

    assert!(fed_markers().contains(&(text.to_string(), 9_001)));
}

#[tokio::test]
async fn listener_poll_feeds_marked_rows_to_the_process_wide_notifier_record() {
    crate::settings::init_settings_for_test();
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    let cursors = Cursors::starting_at(0);
    let text = "🤖 [poll-test-b/x main:0.0] done (x)";
    insert_dated(&db, &[(HANDLE, 0, text, 9_002)]);

    poll_db(&store, db, &cursors, || false).await;

    assert!(fed_markers().contains(&(text.to_string(), 9_002)));
}

#[tokio::test]
async fn paused_listener_poll_discards_the_reply_and_still_reads_marked_rows() {
    crate::settings::init_settings_for_test();
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    let cursors = Cursors::starting_at(0);
    let text = "🤖 [pause-test-c/x main:0.0] done (x)";
    insert_dated(
        &db,
        &[
            (HANDLE, 0, text, 9_003),
            (HANDLE, 0, "carry on", 9_004),
            (HANDLE, 1, "and stop", 9_005),
        ],
    );

    poll_db(&store, db, &cursors, || true).await;

    assert!(fed_markers().contains(&(text.to_string(), 9_003)));
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 1);
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 2);
    assert_eq!(cursors.self_sent.load(Ordering::Relaxed), 3);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 0);
}

#[tokio::test]
async fn pause_is_read_after_the_rows_are_fetched() {
    let store_directory = TestDirectory::new();
    let store = HaroldStore::open(&store_directory.0).await.unwrap();
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    let cursors = Cursors::starting_at(0);
    let recent = Mutex::new(RecentRows::new());
    insert(&db, &[(HANDLE, 0, "first")]);

    // A reply stored when the pause is read was not fetched, so it waits for the next
    // poll. Were the pause read before the fetch, it would be recorded in this one.
    let store_a_reply = || {
        insert(&db, &[(HANDLE, 0, "stored while the pause is read")]);
        false
    };
    poll_chat_db(
        &store,
        db.clone(),
        &cursors,
        |_, _| {},
        &recent,
        store_a_reply,
    )
    .await;
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 1);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);

    poll_chat_db(&store, db, &cursors, |_, _| {}, &recent, || false).await;
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 2);
    assert_eq!(store.project_unhandled_events(10).await.unwrap().applied, 1);
}

#[test]
fn chat_db_from_settings_passes_the_path_and_handles_through_and_reads_marked_rows_only_with_machines()
 {
    let directory = TestDirectory::new();
    let seeded = chat_db(&directory);
    insert_dated(
        &seeded,
        &[
            (HANDLE, 1, STUDIO_NOTIFICATION, 5),
            (HANDLE, 0, LAPTOP_NOTIFICATION, 6),
        ],
    );
    let machines = vec!["studio".to_string()];

    let without = ChatDb::from_settings(seeded.path.clone(), vec![HANDLE, 9], &[]);
    let with = ChatDb::from_settings(seeded.path.clone(), vec![HANDLE, 9], &machines);

    assert_eq!(without.path, seeded.path);
    assert_eq!(without.handle_ids, [HANDLE, 9]);
    assert_eq!(with.path, seeded.path);
    assert_eq!(with.handle_ids, [HANDLE, 9]);
    assert_eq!(without.marked_after(0), Ok(vec![]));
    assert_eq!(without.latest_marked(2, 10), Ok(vec![]));
    assert_eq!(
        with.marked_after(0).unwrap().len(),
        2,
        "a set machines list reads the marked rows"
    );
    assert_eq!(with.latest_marked(2, 10).unwrap().len(), 2);
}

/// 60 marked rows (dates 1000 + ROWID) on `HANDLE`, each followed by a row that is not
/// marked and one marked row on another handle. Returns the ROWIDs of the marked rows.
fn insert_many_marked(db: &ChatDb, count: i64) -> Vec<i64> {
    let mut rowids = vec![];
    let mut rowid = 0;
    for n in 0..count {
        rowid += 1;
        insert_dated(
            db,
            &[(
                HANDLE,
                (n % 2) as u8,
                &format!("🤖 [studio/x main:0.0] n{n} (x)"),
                1000 + rowid,
            )],
        );
        rowids.push(rowid);
        insert_dated(
            db,
            &[
                (HANDLE, 0, "plain reply", 1),
                (HANDLE + 1, 1, "🤖 [other/x main:0.0] elsewhere (x)", 2),
            ],
        );
        rowid += 2;
    }
    rowids
}

async fn start_with(db: &ChatDb) -> (Vec<(String, i64)>, Cursors) {
    let (_keep, mut shutdown) = tokio::sync::watch::channel(());
    let fed = Mutex::new(Vec::new());
    let start = start_listener_reading(db, Duration::from_millis(1), &mut shutdown, |t, d| {
        fed.lock().unwrap().push((t.to_string(), d));
    });
    let cursors = tokio::time::timeout(START_DEADLINE, start)
        .await
        .expect("the database never became readable")
        .expect("cursors");
    (fed.into_inner().unwrap(), cursors)
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_feeds_the_last_fifty_marked_rows_oldest_first() {
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    let rowids = insert_many_marked(&db, 60);

    let (fed, cursors) = start_with(&db).await;

    let expected: Vec<(String, i64)> = (10..60)
        .map(|n| {
            (
                format!("🤖 [studio/x main:0.0] n{n} (x)"),
                1000 + rowids[n as usize],
            )
        })
        .collect();
    assert_eq!(fed, expected);
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 180);
    assert_eq!(cursors.self_sent.load(Ordering::Relaxed), 180);
    assert_eq!(cursors.marked.load(Ordering::Relaxed), 180);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_feeds_fewer_than_fifty_marked_rows_all_oldest_first() {
    let directory = TestDirectory::new();
    let db = chat_db(&directory);
    let rowids = insert_many_marked(&db, 7);

    let (fed, cursors) = start_with(&db).await;

    let dates: Vec<i64> = fed.iter().map(|(_, date)| *date).collect();
    let expected: Vec<i64> = rowids.iter().map(|rowid| 1000 + rowid).collect();
    assert_eq!(dates, expected);
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 21);
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_waits_for_an_unreadable_database_then_feeds_its_rows() {
    let directory = TestDirectory::new();
    let db = chat_db_without_date(&directory);
    insert(&db, &[(HANDLE, 1, STUDIO_NOTIFICATION)]);
    let (_keep, mut shutdown) = tokio::sync::watch::channel(());
    let fed = Mutex::new(Vec::new());
    let fixer = db.clone();
    // A thread with its result returned, so a failed repair is reported with its cause.
    let repair = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        add_date_column_while_read(&fixer)
    });

    let start = start_listener_reading(&db, Duration::from_millis(5), &mut shutdown, |t, d| {
        fed.lock().unwrap().push((t.to_string(), d));
    });
    let waited = tokio::time::timeout(START_DEADLINE, start).await;
    let repaired = repair.join().expect("the repair thread panicked");
    assert_eq!(repaired, Ok(()), "the repair step failed");
    let cursors = waited
        .expect("the database never became readable after the repair")
        .expect("cursors");

    assert_eq!(*fed.lock().unwrap(), [(STUDIO_NOTIFICATION.to_string(), 0)]);
    assert_eq!(cursors.inbound.load(Ordering::Relaxed), 1);
}

#[test]
fn agent_message_goes_out_with_one_marker() {
    let text = crate::inbound::ownership::agent_message_text(
        "PasM4MBP",
        "pods:0.1",
        "Pod 3 is blocked on a merge conflict in store.rs",
    );

    let script = send_script(&text, "+61400000000");

    assert_eq!(
        script,
        "tell application \"Messages\" to send \"🤖 [PasM4MBP/pods:0.1] Pod 3 is blocked on a merge conflict in store.rs\" to buddy \"+61400000000\""
    );
    assert_eq!(script.matches('🤖').count(), 1);
}
