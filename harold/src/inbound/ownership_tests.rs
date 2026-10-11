use super::{
    Marker, NotifierRecord, known_machine_names, latest_notifier, parse_marker, record_marker,
    record_notification,
};

fn notification(machine: &str) -> Option<Marker<'_>> {
    Some(Marker {
        machine,
        is_notification: true,
    })
}

#[test]
fn notification_marker_names_the_machine_before_the_first_slash() {
    assert_eq!(
        parse_marker("🤖 [studio/alir-app main:0.1] Tests pass (alir)"),
        notification("studio")
    );
    // tmux session names may contain '/': only the first one ends the machine name.
    assert_eq!(
        parse_marker("🤖 [laptop/feature/x:0.0] Build done (harold)"),
        notification("laptop")
    );
    assert_eq!(
        parse_marker("  🤖[studio/a:0.0] done"),
        notification("studio")
    );
}

#[test]
fn error_reply_marker_names_the_machine_but_is_not_a_notification() {
    assert_eq!(
        parse_marker("🤖 [studio] No active agent sessions found."),
        Some(Marker {
            machine: "studio",
            is_notification: false,
        })
    );
}

#[test]
fn old_format_notification_is_not_a_marker() {
    // Sent by a Harold from before machine names: the tag is a pane label.
    assert_eq!(parse_marker("🤖 [harold:0.1] Work is done. (harold)"), None);
    assert_eq!(
        parse_marker("🤖 [alir-app main:0.1] Tests pass (alir)"),
        None
    );
}

#[test]
fn other_messages_are_not_markers() {
    for text in [
        "🤖 ✓ Delivered to [studio/alir-app main:0.1]",
        "🤖 Should I deploy?",
        "[studio/alir-app main:0.1] revert that",
        "🤖 [studio/unclosed",
        "🤖 [] empty",
        "🤖 [/pane] no machine",
        "",
    ] {
        assert_eq!(parse_marker(text), None, "{text:?}");
    }
}

#[test]
fn record_starts_with_no_notifier_and_no_names() {
    let record = NotifierRecord::new();

    assert_eq!(record.latest_notifier(), None);
    assert!(record.known_machine_names().is_empty());
}

#[test]
fn latest_notifier_is_the_machine_of_the_last_notification_recorded() {
    let mut record = NotifierRecord::new();

    record.record_marker("🤖 [studio/alir-app main:0.1] Tests pass (alir)");
    record.record_marker("🤖 [laptop/harold main:0.0] Build done (harold)");
    assert_eq!(record.latest_notifier(), Some("laptop"));

    record.record_notification("studio");
    assert_eq!(record.latest_notifier(), Some("studio"));
    assert_eq!(record.known_machine_names(), ["studio", "laptop"]);
}

#[test]
fn error_reply_adds_a_known_name_without_becoming_the_latest_notifier() {
    let mut record = NotifierRecord::new();

    record.record_marker("🤖 [studio/alir-app main:0.1] Tests pass (alir)");
    record.record_marker("🤖 [laptop] No active agent sessions found.");

    assert_eq!(record.latest_notifier(), Some("studio"));
    assert_eq!(record.known_machine_names(), ["studio", "laptop"]);
}

#[test]
fn unmarked_and_invalid_input_leaves_the_record_unchanged() {
    let mut record = NotifierRecord::new();

    record.record_marker("yes, merge it");
    record.record_marker("🤖 [harold:0.1] Work is done. (harold)");
    record.record_marker("🤖 ✓ Delivered to [studio/alir-app main:0.1]");
    record.record_notification("");
    record.record_notification("my mac");

    assert_eq!(record.latest_notifier(), None);
    assert!(record.known_machine_names().is_empty());
}

#[test]
fn a_name_seen_in_another_case_is_known_once() {
    let mut record = NotifierRecord::new();

    record.record_notification("Studio");
    record.record_marker("🤖 [studio/a:0.0] done (a)");

    assert_eq!(record.known_machine_names(), ["Studio"]);
    assert_eq!(record.latest_notifier(), Some("studio"));
}

#[test]
fn process_wide_record_is_fed_and_read_through_the_four_functions() {
    // The only test that touches the process-wide record. The names are unique to it.
    record_marker("🤖 [ownership-test-a/x:0.0] done (x)");
    assert_eq!(latest_notifier().as_deref(), Some("ownership-test-a"));

    record_notification("ownership-test-b");
    assert_eq!(latest_notifier().as_deref(), Some("ownership-test-b"));

    let known = known_machine_names();
    assert!(known.contains(&"ownership-test-a".to_string()), "{known:?}");
    assert!(known.contains(&"ownership-test-b".to_string()), "{known:?}");
}
