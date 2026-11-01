use super::pane_route::{
    MAX_LOGGED_REFUSALS, PaneRefusal, PaneRoute, RefusalLog, host_pane_id, kept_ids, loggable_id,
    parse_host_pane_id, refuse_write, route_pane, valid_host_label,
};
use super::rex::valid_rex_pane_id;
use super::runtime::{valid_pane_id, valid_tmux_pane_id};

#[test]
fn host_pane_id_shape_accepts_only_the_valid_form() {
    assert_eq!(
        parse_host_pane_id("rex:omarchy/block:abc"),
        Some(("omarchy", "block:abc"))
    );
    assert_eq!(host_pane_id("omarchy", "abc"), "rex:omarchy/block:abc");
    let long = format!("rex:{}/block:a", "x".repeat(33));
    for bad in [
        "block:abc",
        "%4",
        "rex:/block:a",
        "rex:h/",
        "rex:h/block:",
        "rex:h/%4",
        "rex:h/rex:h/block:a",
        long.as_str(),
        "",
    ] {
        assert_eq!(parse_host_pane_id(bad), None, "{bad:?}");
    }
    let id = "rex:omarchy/block:abc";
    assert!(!valid_pane_id(id));
    assert!(!valid_rex_pane_id(id));
    assert!(!valid_tmux_pane_id(id));
}

#[test]
fn route_pane_by_shape() {
    assert_eq!(route_pane("%7"), Ok(PaneRoute::Tmux));
    assert_eq!(route_pane("block:abc"), Ok(PaneRoute::Rex));
    assert_eq!(
        route_pane("rex:omarchy/block:abc"),
        Err(PaneRefusal::OnRexHost)
    );
    assert_eq!(
        route_pane("rex:a_b/block:x"),
        Err(PaneRefusal::InvalidPaneId)
    );
    assert_eq!(route_pane("main:0.1"), Err(PaneRefusal::InvalidPaneId));
    assert_eq!(route_pane(""), Err(PaneRefusal::InvalidPaneId));
    assert_eq!(PaneRefusal::OnRexHost.reason(), "on_rex_host");
    assert_eq!(PaneRefusal::InvalidPaneId.reason(), "invalid_pane_id");
}

#[test]
fn valid_host_label_shape() {
    assert!(valid_host_label("omarchy"));
    assert!(valid_host_label("a.b-c1"));
    for bad in ["a/b", "a:b", "a b", "a_b", ""] {
        assert!(!valid_host_label(bad), "{bad:?}");
    }
    assert!(!valid_host_label(&"x".repeat(33)));
    assert!(valid_host_label(&"x".repeat(32)));
}

#[test]
fn a_full_dedupe_set_still_logs_once_a_minute_for_each_writer() {
    let mut log = RefusalLog::default();
    for n in 0..MAX_LOGGED_REFUSALS {
        assert!(log.should_log(&format!("rex:h/block:{n}"), "pane_key", 1000));
    }
    // The set is full: a new pair still logs, but only once a minute for each writer.
    assert!(log.should_log("rex:h/block:new1", "pane_key", 1000));
    assert!(!log.should_log("rex:h/block:new2", "pane_key", 1030));
    assert!(log.should_log("rex:h/block:new2", "pane_key", 1060));
    assert!(log.should_log("rex:h/block:new3", "speak_line", 1060));
    // A pair that the set holds never logs again.
    assert!(!log.should_log("rex:h/block:0", "pane_key", 5000));
}

#[test]
fn a_refused_id_is_logged_and_kept_in_a_bounded_form() {
    let hostile = format!("rex:\n{}\u{7}\u{e9}", "x".repeat(10_000));
    assert_eq!(
        loggable_id(&hostile, PaneRefusal::InvalidPaneId),
        "<invalid-pane-id>"
    );
    let long = format!("rex:h/block:{}\u{e9}", "a".repeat(200));
    let cut = loggable_id(&long, PaneRefusal::OnRexHost);
    assert_eq!(cut.len(), 128);
    assert!(long.starts_with(cut));
    // A cut inside a character moves back to the character start.
    let wide = format!("{}{}", "a".repeat(127), "\u{e9}");
    assert_eq!(loggable_id(&wide, PaneRefusal::OnRexHost).len(), 127);
    assert_eq!(
        loggable_id("rex:h/block:a", PaneRefusal::OnRexHost),
        "rex:h/block:a"
    );
}

#[test]
fn a_hostile_id_is_neither_logged_nor_kept() {
    let logs = crate::outbound::fake::CapturedLogs::start();
    let hostile = format!("rex:\n\u{1b}[31mHOSTILE{}\r\n", "x".repeat(10_000));
    // A writer name that no other test uses, so the process-wide dedupe does not hide the line.
    let text = refuse_write(&hostile, "hostile_id_test", PaneRefusal::InvalidPaneId);
    let lines = logs.text();
    assert!(lines.contains("pane write refused"), "{lines}");
    assert!(lines.contains("<invalid-pane-id>"), "{lines}");
    assert!(!lines.contains("HOSTILE"), "{lines}");
    assert!(!lines.contains("xxxx"), "{lines}");
    assert!(!text.contains("HOSTILE"), "{text}");
    assert!(text.len() < 200, "{}", text.len());
    let kept = kept_ids();
    assert!(kept.iter().any(|id| id == "<invalid-pane-id>"));
    assert!(kept.iter().all(|id| id.len() <= 128), "{kept:?}");
    assert!(kept.iter().all(|id| !id.contains("HOSTILE")), "{kept:?}");
}
