use super::pane_route::{
    PaneRefusal, PaneRoute, host_pane_id, parse_host_pane_id, route_pane, valid_host_label,
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
