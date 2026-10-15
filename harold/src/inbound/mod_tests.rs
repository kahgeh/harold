use std::cell::RefCell;
use std::sync::Mutex;

use crate::inbound::ownership::Ownership;
use crate::inbound::{
    AgentAddress, RouteReport, RoutingEnv, clear_routing_state, parse_tag, resolve_pane,
    route_if_owned, route_owned_reply, set_last_away_notification_source_agent,
};
use crate::outbound::DeliveryOutcome;
use crate::settings::init_settings_for_test;
use crate::telemetry::{ResolvedBy, Routing};

/// Serialises tests that mutate global routing state.
static ROUTING_TEST_LOCK: Mutex<()> = Mutex::new(());

fn tmux(pane_id: &str, label: &str) -> AgentAddress {
    AgentAddress::TmuxPane {
        pane_id: pane_id.into(),
        label: label.into(),
    }
}

#[test]
fn parse_tag_with_tag() {
    let (tag, body) = parse_tag("[main] hello world");
    assert_eq!(tag, Some("main"));
    assert_eq!(body, "hello world");
}

#[test]
fn parse_tag_without_tag() {
    let (tag, body) = parse_tag("just a message");
    assert_eq!(tag, None);
    assert_eq!(body, "just a message");
}

#[test]
fn parse_tag_unclosed_bracket() {
    let (tag, body) = parse_tag("[unclosed message");
    assert_eq!(tag, None);
    assert_eq!(body, "[unclosed message");
}

#[test]
fn resolve_pane_exact_match() {
    let panes = vec![tmux("%1", "work:0.0"), tmux("%2", "home:0.1")];
    let (pane, _, resolved_by) = resolve_pane(Some("work:0.0"), "hi", &panes).unwrap();
    assert_eq!(pane.pane_id(), "%1");
    assert_eq!(resolved_by, ResolvedBy::ExactTag);
}

#[test]
fn resolve_pane_substring_match() {
    let panes = vec![tmux("%1", "work:0.0"), tmux("%2", "home:0.1")];
    let (pane, _, resolved_by) = resolve_pane(Some("home"), "hi", &panes).unwrap();
    assert_eq!(pane.pane_id(), "%2");
    assert_eq!(resolved_by, ResolvedBy::TagSubstring);
}

#[test]
fn resolve_pane_no_tag_falls_back_to_my_agent() {
    let _lock = ROUTING_TEST_LOCK.lock().unwrap();
    clear_routing_state();
    let panes = vec![tmux("%1", "my-agent:0.0")];
    let (pane, _, resolved_by) = resolve_pane(None, "hi", &panes).unwrap();
    assert_eq!(pane.pane_id(), "%1");
    assert_eq!(resolved_by, ResolvedBy::MyAgent);
}

#[test]
fn resolve_pane_last_away_notification_source_beats_my_agent() {
    let _lock = ROUTING_TEST_LOCK.lock().unwrap();
    init_settings_for_test();
    clear_routing_state();
    let panes = vec![tmux("%3", "alir-app:0.1"), tmux("%4", "my-agent:0.0")];
    set_last_away_notification_source_agent(tmux("%3", "alir-app:0.1"));
    let (pane, _, resolved_by) = resolve_pane(None, "hi", &panes).unwrap();
    assert_eq!(pane.pane_id(), "%3");
    assert_eq!(resolved_by, ResolvedBy::LatestNotifier);
}

#[test]
fn resolve_pane_no_match_returns_none() {
    let panes = vec![tmux("%1", "work:0.0")];
    let result = resolve_pane(Some("nonexistent"), "hi", &panes);
    assert!(result.is_none());
}

#[test]
fn reply_owned_by_another_mac_is_skipped_without_routing() {
    let mut routed = false;

    let another_macs = Ownership::NotOwned {
        owner: "laptop".to_string(),
    };

    let report = route_if_owned("go on", another_macs, |_, _| {
        routed = true;
        Ok(typed_report())
    })
    .unwrap();

    assert_eq!(report.outcome, DeliveryOutcome::Skipped);
    assert_eq!(report.routing, Routing::NotOwned { tagged: false });
    assert!(!routed, "an unowned reply reached pane discovery");
}

#[test]
fn a_reply_another_mac_owns_is_tagged_only_when_its_tag_named_that_mac() {
    let cases = [
        ("[laptop/work:0.1] revert that", true),
        ("[Laptop] revert that", true),
        ("[ laptop / work:0.1 ] revert that", true),
        // No tag, or a pane tag: the reply followed the latest notifier there.
        ("revert that", false),
        ("[work:0.1] revert that", false),
    ];
    for (text, tagged) in cases {
        let ownership = Ownership::NotOwned {
            owner: "laptop".to_string(),
        };
        let report = route_if_owned(text, ownership, |_, _| Ok(typed_report())).unwrap();
        assert_eq!(report.routing, Routing::NotOwned { tagged }, "{text}");
    }
}

#[test]
fn owned_reply_is_routed_with_its_pane_tag_and_body() {
    let mut routed = Vec::new();
    let ownership = Ownership::Owned {
        pane_tag: Some("alir-app main:0.1"),
        body: "revert that",
    };

    let report = route_if_owned(
        "[alir-app main:0.1] revert that",
        ownership,
        |pane_tag, body| {
            routed.push((pane_tag.map(String::from), body.to_string()));
            Ok(typed_report())
        },
    );

    assert_eq!(report, Ok(typed_report()));
    assert_eq!(
        routed,
        [(
            Some("alir-app main:0.1".to_string()),
            "revert that".to_string()
        )]
    );
}

#[test]
fn routing_failure_of_an_owned_reply_is_returned() {
    let ownership = Ownership::Owned {
        pane_tag: None,
        body: "go",
    };

    let report = route_if_owned("go", ownership, |_, _| Err("tmux failed".to_string()));

    assert_eq!(report, Err("tmux failed".to_string()));
}

/// What routing returns for a reply typed into pane `%1` by exact tag.
fn typed_report() -> RouteReport {
    RouteReport {
        outcome: DeliveryOutcome::Delivered,
        routing: Routing::Typed {
            pane_id: "%1".into(),
            resolved_by: ResolvedBy::ExactTag,
            tagged: true,
            typed_at: time::OffsetDateTime::UNIX_EPOCH,
        },
    }
}

/// The pane, `resolved_by` and tag flag of a typed reply; `None` for any other routing.
fn chosen(routing: &Routing) -> Option<(&str, ResolvedBy, bool)> {
    match routing {
        Routing::Typed {
            pane_id,
            resolved_by,
            tagged,
            ..
        } => Some((pane_id.as_str(), *resolved_by, *tagged)),
        Routing::NotOwned { .. } | Routing::Paused | Routing::NoPane => None,
    }
}

/// A machine with the given panes that records what is relayed and replied.
struct FakeEnv {
    panes: Vec<AgentAddress>,
    alive: bool,
    /// How long telling the user takes.
    reply_delay: std::time::Duration,
    relayed: RefCell<Vec<String>>,
    replies: RefCell<Vec<String>>,
}

impl FakeEnv {
    fn with_panes(panes: Vec<AgentAddress>) -> Self {
        Self {
            panes,
            alive: true,
            reply_delay: std::time::Duration::ZERO,
            relayed: RefCell::new(Vec::new()),
            replies: RefCell::new(Vec::new()),
        }
    }
}

impl RoutingEnv for FakeEnv {
    fn discover(&self) -> Vec<AgentAddress> {
        self.panes.clone()
    }

    fn is_alive(&self, _: &AgentAddress) -> bool {
        self.alive
    }

    fn relay(&self, _: &AgentAddress, text: &str) -> Result<(), String> {
        self.relayed.borrow_mut().push(text.to_string());
        Ok(())
    }

    fn send_reply(&self, text: &str) -> Result<(), String> {
        std::thread::sleep(self.reply_delay);
        self.replies.borrow_mut().push(text.to_string());
        Ok(())
    }
}

#[test]
fn delivered_reply_is_confirmed_with_the_full_address_of_the_pane() {
    let env = FakeEnv::with_panes(vec![tmux("%1", "alir-app main:0.1")]);

    let report = route_owned_reply(&env, "studio", Some("alir"), "revert that").unwrap();

    assert_eq!(report.outcome, DeliveryOutcome::Delivered);
    assert_eq!(*env.relayed.borrow(), ["📱 revert that"]);
    assert_eq!(
        *env.replies.borrow(),
        ["✓ Delivered to [studio/alir-app main:0.1]"]
    );
}

#[test]
fn error_replies_name_the_machine_that_sends_them() {
    let _lock = ROUTING_TEST_LOCK.lock().unwrap();
    clear_routing_state();

    let none = FakeEnv::with_panes(Vec::new());
    assert_eq!(
        route_owned_reply(&none, "studio", None, "go").map(|report| report.outcome),
        Ok(DeliveryOutcome::Skipped)
    );
    assert_eq!(
        *none.replies.borrow(),
        ["[studio] No active agent sessions found."]
    );

    let work = FakeEnv::with_panes(vec![tmux("%1", "work:0.0")]);
    route_owned_reply(&work, "studio", Some("nonexistent"), "go").unwrap();
    assert_eq!(
        *work.replies.borrow(),
        ["[studio] No pane matching 'nonexistent'. Available: work:0.0"]
    );

    let untagged = FakeEnv::with_panes(vec![tmux("%1", "work:0.0")]);
    route_owned_reply(&untagged, "studio", None, "go").unwrap();
    assert_eq!(
        *untagged.replies.borrow(),
        ["[studio] No active pane found. Available: work:0.0"]
    );

    let mut gone = FakeEnv::with_panes(vec![tmux("%1", "work:0.0")]);
    gone.alive = false;
    route_owned_reply(&gone, "studio", Some("work"), "go").unwrap();
    assert_eq!(
        *gone.replies.borrow(),
        ["[studio] Pane work:0.0 is no longer active. Available: "]
    );
    assert!(gone.relayed.borrow().is_empty());
}

#[test]
fn routing_reports_how_the_pane_was_chosen() {
    let env = FakeEnv::with_panes(vec![tmux("%1", "harold:1.0"), tmux("%2", "chief:1.0")]);

    let report = route_owned_reply(&env, "PasM4MBP", Some("chief"), "hello").unwrap();
    assert_eq!(
        chosen(&report.routing),
        Some(("%2", ResolvedBy::TagSubstring, true))
    );
    let report = route_owned_reply(&env, "PasM4MBP", Some("chief:1.0"), "hello").unwrap();
    assert_eq!(
        chosen(&report.routing),
        Some(("%2", ResolvedBy::ExactTag, true))
    );
    let report =
        route_owned_reply(&FakeEnv::with_panes(Vec::new()), "PasM4MBP", None, "hello").unwrap();
    assert_eq!(report.routing, Routing::NoPane);
}

#[test]
fn an_untagged_reply_reports_the_latest_notifier_or_the_my_agent_fallback() {
    let _lock = ROUTING_TEST_LOCK.lock().unwrap();
    init_settings_for_test();
    clear_routing_state();
    let env = FakeEnv::with_panes(vec![tmux("%3", "alir-app:0.1"), tmux("%4", "my-agent:0.0")]);

    let report = route_owned_reply(&env, "PasM4MBP", None, "hello").unwrap();
    assert_eq!(
        chosen(&report.routing),
        Some(("%4", ResolvedBy::MyAgent, false))
    );

    set_last_away_notification_source_agent(tmux("%3", "alir-app:0.1"));
    let report = route_owned_reply(&env, "PasM4MBP", None, "hello").unwrap();
    assert_eq!(
        chosen(&report.routing),
        Some(("%3", ResolvedBy::LatestNotifier, false))
    );
    clear_routing_state();
}

#[test]
fn a_tag_no_pane_matches_or_a_pane_that_is_gone_reports_no_pane() {
    let work = FakeEnv::with_panes(vec![tmux("%1", "work:0.0")]);
    let report = route_owned_reply(&work, "studio", Some("nonexistent"), "go").unwrap();
    assert_eq!(report.routing, Routing::NoPane);
    assert_eq!(report.outcome, DeliveryOutcome::Skipped);

    let mut gone = FakeEnv::with_panes(vec![tmux("%1", "work:0.0")]);
    gone.alive = false;
    let report = route_owned_reply(&gone, "studio", Some("work"), "go").unwrap();
    assert_eq!(report.routing, Routing::NoPane);
}

#[test]
fn a_typed_reply_ends_when_enter_is_pressed_not_when_the_user_is_told() {
    use crate::telemetry::{self, PhoneBook, test_support};
    let channel: &'static str =
        Box::leak(format!("test-{}", uuid::Uuid::new_v4().simple()).into_boxed_str());
    let mut env = FakeEnv::with_panes(vec![tmux("%1", "work:0.0")]);
    env.reply_delay = std::time::Duration::from_millis(300);
    let reply = telemetry::open_phone_request(channel, time::OffsetDateTime::now_utc());

    let report = route_owned_reply(&env, "studio", Some("work"), "go").unwrap();
    let handled_at = time::OffsetDateTime::now_utc();
    telemetry::finish_phone_request_in(
        &mut PhoneBook::default(),
        &reply,
        &report.routing,
        handled_at,
    );

    let Routing::Typed { typed_at, .. } = report.routing else {
        panic!("the reply was typed: {:?}", report.routing);
    };
    assert!(
        handled_at - typed_at >= time::Duration::milliseconds(300),
        "the typing time was taken before the confirmation was sent"
    );
    let requests: Vec<_> = test_support::spans()
        .into_iter()
        .filter(|span| {
            span.name == "harold.phone_request"
                && test_support::span_attr(span, "channel").as_deref() == Some(channel)
        })
        .collect();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].end_time, std::time::SystemTime::from(typed_at));
    let latency =
        test_support::histogram_sum("harold.phone_request.latency", &[("channel", channel)])
            .expect("the latency is recorded");
    assert!(latency < 0.3, "{latency}");
}
