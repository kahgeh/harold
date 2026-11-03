use super::focus::{FocusPort, UnknownReason, read_focus};
use std::collections::HashMap;
use std::sync::Mutex;

/// Maps one argv to a canned reply. A missing argv is a failed call.
struct FakeFocusPort {
    replies: HashMap<Vec<String>, Result<String, String>>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl FakeFocusPort {
    fn new(replies: Vec<(&[&str], Result<&str, &str>)>) -> Self {
        Self {
            replies: replies
                .into_iter()
                .map(|(argv, reply)| {
                    (
                        argv.iter().map(|part| (*part).to_owned()).collect(),
                        reply.map(str::to_owned).map_err(str::to_owned),
                    )
                })
                .collect(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl FocusPort for FakeFocusPort {
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let mut argv = vec![program.to_owned()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        self.calls.lock().unwrap().push(argv.clone());
        self.replies
            .get(&argv)
            .cloned()
            .unwrap_or_else(|| Err("no such call".to_owned()))
    }
}

const CLIENTS: &str = r#"{
  "clients": [
    {
      "client_id": "client:cli",
      "info": { "kind": "cli", "name": "rex" },
      "connection_state": "connected"
    },
    {
      "client_id": "client:app",
      "info": {
        "kind": "app",
        "name": "Rex Beta"
      },
      "connection_state": "connected",
      "session_ids": [
        "session:aaa",
        "session:bbb"
      ]
    },
    {
      "client_id": "client:other",
      "info": { "kind": "cli", "name": "rex" },
      "session_ids": ["session:zzz"]
    }
  ]
}"#;

const CLIENTS_ONE_SESSION: &str = r#"{"clients":[{"client_id":"client:app","info":{"kind":"app"},"session_ids":["session:aaa"]}]}"#;

const VIEW_OF_AAA: &str = r#"{"revision":1,"session_id":"session:aaa","label":"arid-moor","active_window_id":"window:2","windows":[{"window_id":"window:1","label":"One","focused_block_id":"block:one","layers":[]},{"window_id":"window:2","label":"Two","focused_block_id":"block:two","layers":[{"layer_id":"layer:x","blocks":[{"block_id":"block:two"}]}]}]}"#;

const REX_CLIENTS: &[&str] = &["rex", "client", "ls", "--json"];
const REX_VIEW: &[&str] = &[
    "rex",
    "api",
    "call",
    "session.view",
    r#"{"session_id":"session:aaa"}"#,
];
const TMUX_CLIENTS: &[&str] = &[
    "tmux",
    "list-clients",
    "-F",
    "#{client_activity}\t#{client_flags}\t#{pane_id}",
];

#[test]
fn one_rex_session_gives_the_focused_block() {
    let port = FakeFocusPort::new(vec![
        (REX_CLIENTS, Ok(CLIENTS_ONE_SESSION)),
        (REX_VIEW, Ok(VIEW_OF_AAA)),
    ]);
    assert_eq!(read_focus(&port, true), Ok(Some("block:two".to_owned())));
    assert_eq!(port.calls().len(), 2);
}

#[test]
fn many_rex_sessions_is_unknown_many_sessions() {
    let port = FakeFocusPort::new(vec![(REX_CLIENTS, Ok(CLIENTS))]);
    assert_eq!(read_focus(&port, true), Err(UnknownReason::ManySessions));
    assert_eq!(port.calls().len(), 1);
}

#[test]
fn rex_missing_is_unknown_rex_unreachable() {
    let port = FakeFocusPort::new(vec![]);
    assert_eq!(read_focus(&port, true), Err(UnknownReason::RexUnreachable));
    let port = FakeFocusPort::new(vec![(REX_CLIENTS, Ok(r#"{"clients":[]}"#))]);
    assert_eq!(read_focus(&port, true), Err(UnknownReason::RexUnreachable));
    let port = FakeFocusPort::new(vec![
        (REX_CLIENTS, Ok(CLIENTS_ONE_SESSION)),
        (REX_VIEW, Err("down")),
    ]);
    assert_eq!(read_focus(&port, true), Err(UnknownReason::RexUnreachable));
}

#[test]
fn tmux_picks_the_focused_client_and_skips_suspended() {
    let port = FakeFocusPort::new(vec![(
        TMUX_CLIENTS,
        Ok("1790945360\tattached,focused,suspended,UTF-8\t%0\n\
            1791018376\tattached,focused,suspended,UTF-8\t%36\n\
            1791453304\tattached,focused,UTF-8\t%53\n"),
    )]);
    assert_eq!(read_focus(&port, false), Ok(Some("%53".to_owned())));
    assert_eq!(port.calls().len(), 1);

    let port = FakeFocusPort::new(vec![(
        TMUX_CLIENTS,
        Ok("900\tattached,UTF-8\t%1\n100\tattached,focused\t%2\n"),
    )]);
    assert_eq!(read_focus(&port, false), Ok(Some("%2".to_owned())));
}

#[test]
fn tmux_with_no_client_is_unknown_tmux_no_client() {
    for stdout in [
        "",
        "\n",
        "garbage\n",
        "x\tattached\t%1\n",
        "5\tattached,suspended\t%1\n",
        "5\tattached\t\n",
    ] {
        let port = FakeFocusPort::new(vec![(TMUX_CLIENTS, Ok(stdout))]);
        assert_eq!(
            read_focus(&port, false),
            Err(UnknownReason::TmuxNoClient),
            "{stdout:?}"
        );
    }
    let port = FakeFocusPort::new(vec![(TMUX_CLIENTS, Err("no server running"))]);
    assert_eq!(read_focus(&port, false), Err(UnknownReason::TmuxNoClient));
}

#[test]
fn tmux_activity_breaks_ties() {
    let port = FakeFocusPort::new(vec![(
        TMUX_CLIENTS,
        Ok("100\tattached\t%1\n900\tattached\t%2\n500\tattached\t%3\n"),
    )]);
    assert_eq!(read_focus(&port, false), Ok(Some("%2".to_owned())));
}

#[test]
fn the_reason_names_are_stable() {
    assert_eq!(UnknownReason::ManySessions.as_str(), "many_sessions");
    assert_eq!(UnknownReason::RexUnreachable.as_str(), "rex_unreachable");
    assert_eq!(UnknownReason::TmuxNoClient.as_str(), "tmux_no_client");
}

#[test]
fn rex_app_with_no_session_is_unknown_rex_unreachable() {
    let clients =
        r#"{"clients":[{"client_id":"client:app","info":{"kind":"app"},"session_ids":[]}]}"#;
    let port = FakeFocusPort::new(vec![(REX_CLIENTS, Ok(clients))]);
    assert_eq!(read_focus(&port, true), Err(UnknownReason::RexUnreachable));
    let clients = r#"{"clients":[{"client_id":"client:app","info":{"kind":"app"}}]}"#;
    let port = FakeFocusPort::new(vec![(REX_CLIENTS, Ok(clients))]);
    assert_eq!(read_focus(&port, true), Err(UnknownReason::RexUnreachable));
}

#[test]
fn rex_view_without_a_focused_block_is_unknown_rex_unreachable() {
    for view in [
        r#"{"windows":[{"window_id":"window:1","focused_block_id":"b"}]}"#,
        r#"{"active_window_id":"window:9","windows":[{"window_id":"window:1","focused_block_id":"b"}]}"#,
        r#"{"active_window_id":"window:1","windows":[{"window_id":"window:1"}]}"#,
        r#"{"active_window_id":"window:1","windows":[{"window_id":"window:1","focused_block_id":7}]}"#,
        r#"{"active_window_id":"window:1","windows":[{"window_id":"window:1","focused_block_id":"  "}]}"#,
        r#"{"active_window_id":"window:1","windows":[{"window_id":"window:1","focused_block_id":"\u0007"}]}"#,
    ] {
        let port = FakeFocusPort::new(vec![
            (REX_CLIENTS, Ok(CLIENTS_ONE_SESSION)),
            (REX_VIEW, Ok(view)),
        ]);
        assert_eq!(
            read_focus(&port, true),
            Err(UnknownReason::RexUnreachable),
            "{view}"
        );
    }
}
