use super::{
    Marker, NotifierRecord, Ownership, decide_ownership, known_machine_names, latest_notifier,
    parse_marker, record_marker, record_notification,
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

fn known() -> Vec<String> {
    vec!["studio".to_string(), "laptop".to_string()]
}

fn owned<'a>(pane_tag: Option<&'a str>, body: &'a str) -> Ownership<'a> {
    Ownership::Owned { pane_tag, body }
}

fn owned_by(owner: &str) -> Ownership<'static> {
    Ownership::NotOwned {
        owner: owner.to_string(),
    }
}

/// The decision on `studio` for a reply it read itself.
fn on_studio<'a>(text: &'a str, latest: Option<&str>) -> Ownership<'a> {
    decide_ownership(text, false, "studio", &known(), latest)
}

/// The decision on `laptop` for a reply the hub forwarded while `studio` notified last.
fn forwarded_to_laptop(text: &str) -> Ownership<'_> {
    decide_ownership(text, true, "laptop", &known(), Some("studio"))
}

#[test]
fn untagged_reply_belongs_to_the_latest_notifier() {
    assert_eq!(
        on_studio("yes, merge it", Some("studio")),
        owned(None, "yes, merge it")
    );
    assert_eq!(
        on_studio("yes, merge it", Some("laptop")),
        owned_by("laptop")
    );
}

#[test]
fn untagged_reply_with_no_latest_notifier_is_owned_by_every_mac() {
    assert_eq!(
        on_studio("yes, merge it", None),
        owned(None, "yes, merge it")
    );
    assert_eq!(
        decide_ownership("yes, merge it", false, "laptop", &known(), None),
        owned(None, "yes, merge it")
    );
}

#[test]
fn machine_and_pane_tag_is_owned_by_the_named_machine_only() {
    let text = "[studio/alir-app main:0.1] revert that";

    assert_eq!(
        on_studio(text, Some("laptop")),
        owned(Some("alir-app main:0.1"), "revert that")
    );
    assert_eq!(
        decide_ownership(text, false, "laptop", &known(), Some("laptop")),
        owned_by("studio")
    );
}

#[test]
fn machine_only_tag_is_owned_with_no_pane_tag() {
    assert_eq!(
        on_studio("[studio] status?", Some("laptop")),
        owned(None, "status?")
    );
    assert_eq!(
        on_studio("[studio/] status?", Some("laptop")),
        owned(None, "status?")
    );
    assert_eq!(
        on_studio("[laptop] status?", Some("studio")),
        owned_by("laptop")
    );
}

#[test]
fn machine_names_match_case_insensitively() {
    assert_eq!(
        on_studio("[Studio/alir] go", Some("laptop")),
        owned(Some("alir"), "go")
    );
    assert_eq!(on_studio("[STUDIO] go", Some("laptop")), owned(None, "go"));
    // The owner is spelled as it was recorded, not as the user typed it.
    assert_eq!(
        on_studio("[LAPTOP/alir] go", Some("studio")),
        owned_by("laptop")
    );
    assert_eq!(on_studio("go", Some("STUDIO")), owned(None, "go"));
}

#[test]
fn pane_only_tag_follows_the_latest_notifier_and_keeps_the_tag() {
    assert_eq!(
        on_studio("[alir-app] go", Some("studio")),
        owned(Some("alir-app"), "go")
    );
    assert_eq!(
        on_studio("[alir-app] go", Some("laptop")),
        owned_by("laptop")
    );
    assert_eq!(
        on_studio("[alir-app] go", None),
        owned(Some("alir-app"), "go")
    );
}

#[test]
fn tag_with_a_slash_but_no_known_machine_is_a_pane_tag() {
    // A tmux session named "feature/x": "feature" is not a machine anyone has seen.
    assert_eq!(
        on_studio("[feature/x] go", Some("studio")),
        owned(Some("feature/x"), "go")
    );
    assert_eq!(
        on_studio("[feature/x] go", Some("laptop")),
        owned_by("laptop")
    );
}

#[test]
fn this_machine_is_recognised_before_any_marker_was_seen() {
    assert_eq!(
        decide_ownership("[studio/alir] go", false, "studio", &[], None),
        owned(Some("alir"), "go")
    );
}

#[test]
fn forwarded_reply_is_owned_without_consulting_the_latest_notifier() {
    assert_eq!(
        forwarded_to_laptop("yes, merge it"),
        owned(None, "yes, merge it")
    );
    assert_eq!(
        forwarded_to_laptop("[harold] go"),
        owned(Some("harold"), "go")
    );
    assert_eq!(
        forwarded_to_laptop("[laptop/harold] go"),
        owned(Some("harold"), "go")
    );
    // The hub decided this Mac owns it, so a tag naming another Mac does not undo that.
    assert_eq!(
        forwarded_to_laptop("[studio/alir] go"),
        owned(Some("alir"), "go")
    );
}

#[test]
fn unclosed_bracket_is_an_untagged_reply() {
    assert_eq!(
        on_studio("[studio/alir go", Some("studio")),
        owned(None, "[studio/alir go")
    );
    assert_eq!(
        on_studio("[studio/alir go", Some("laptop")),
        owned_by("laptop")
    );
}

#[test]
fn reply_this_mac_does_not_own_names_the_mac_that_does() {
    // A hub called "hub" with two members. The tag decides the owner when it names a
    // machine, whoever notified last.
    let known = vec![
        "hub".to_string(),
        "studio".to_string(),
        "laptop".to_string(),
    ];
    let on_hub = |text, latest| decide_ownership(text, false, "hub", &known, latest);

    assert_eq!(
        on_hub("[studio/alir] go", Some("laptop")),
        owned_by("studio")
    );
    assert_eq!(on_hub("[studio] go", Some("laptop")), owned_by("studio"));
    // Otherwise the latest notifier owns it, with or without a pane tag.
    assert_eq!(on_hub("yes, merge it", Some("laptop")), owned_by("laptop"));
    assert_eq!(on_hub("[alir] go", Some("laptop")), owned_by("laptop"));
    assert_eq!(
        on_hub("yes, merge it", Some("hub")),
        owned(None, "yes, merge it")
    );
}
