use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::store::EventVersion;
use tokio::sync::watch;

use super::*;
use crate::agent::domain::{
    AgentIncarnation, AgentPaneObservation, AgentPaneProjection, AgentSnapshot,
    EffectiveAgentState, MonitorHealthProjection,
};
use crate::agent::snapshot::AgentSnapshotHub;
use crate::settings::TimedPromptSettings;
use crate::settings::{AgentMonitorSettings, AgentProviderSettings, ScreenAdapter};
use crate::store::{HaroldStore, TimedPromptHandled, TimedPromptOutcome};

pub(crate) const CHIEF: &str = "/work/chief-of-pods";

// --- the clock ---

#[test]
fn local_now_is_read_from_the_output_of_date() {
    assert_eq!(
        parse_local_now("2026-10-14 09:00:05\n"),
        Some(LocalNow {
            day: "2026-10-14".into(),
            seconds_of_day: 9 * 3600 + 5,
        })
    );
    assert_eq!(
        parse_local_now("2026-10-14 00:00:00")
            .unwrap()
            .seconds_of_day,
        0
    );
    assert_eq!(
        parse_local_now("2026-10-14 23:59:59")
            .unwrap()
            .seconds_of_day,
        86_399
    );
    // A leap second reads as the last second of its minute.
    assert_eq!(
        parse_local_now("2026-12-31 23:59:60")
            .unwrap()
            .seconds_of_day,
        86_399
    );
}

#[test]
fn output_that_is_not_a_date_and_time_is_not_read() {
    for text in [
        "",
        "2026-10-14",
        "09:00:05",
        "2026-10-14T09:00:05",
        "2026-10-14 9:00:05",
        "2026-10-14 24:00:00",
        "2026-10-14 09:61:00",
        "2026/10/14 09:00:05",
        "14-10-2026 09:00:05",
        "2026-10-14 09:00",
        "2026-10-14 09:00:05:01",
        "2026-10-14 +9:00:05",
    ] {
        assert_eq!(parse_local_now(text), None, "{text:?}");
    }
}

// --- the window ---

#[test]
fn prompt_is_due_from_its_time_until_its_window_ends() {
    let window = Window::new(9 * 60, 120);

    assert_eq!(window.phase(8 * 3600 + 59 * 60), Phase::Before);
    assert_eq!(window.phase(9 * 3600 - 1), Phase::Before);
    assert_eq!(window.phase(9 * 3600), Phase::Due);
    assert_eq!(window.phase(11 * 3600 - 1), Phase::Due);
    assert_eq!(window.phase(11 * 3600), Phase::Ended);
    assert_eq!(window.phase(86_399), Phase::Ended);
}

#[test]
fn window_that_would_pass_midnight_ends_at_midnight() {
    let window = Window::new(23 * 60 + 30, 120);

    assert_eq!(window.end_s, 86_400);
    assert_eq!(window.phase(23 * 3600 + 30 * 60 - 1), Phase::Before);
    assert_eq!(window.phase(23 * 3600 + 30 * 60), Phase::Due);
    assert_eq!(window.phase(86_399), Phase::Due);
    // The first second of the next date is before that date's 23:30.
    assert_eq!(window.phase(0), Phase::Before);
}

// --- the input box ---

/// The rows Claude Code 2.1.296 draws around its input box, captured with
/// `tmux capture-pane -p` and shortened to 56 columns. `screen_with_box(&["❯ "])` is the
/// captured screen; the rows passed for a box that holds text follow the same layout.
const RULE: &str = "────────────────────────────────────────────────────────";
const FOOTER: &str = "  -- INSERT -- ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents";

fn screen_with_box(box_rows: &[&str]) -> String {
    let mut rows = vec![
        "❯ an earlier prompt",
        "",
        "⏺ Done.",
        "",
        "✻ Worked for 20s",
        "",
        RULE,
    ];
    rows.extend(box_rows);
    rows.push(RULE);
    rows.push(FOOTER);
    rows.join("\n") + "\n"
}

#[test]
fn input_box_with_nothing_after_the_prompt_mark_is_empty() {
    assert_eq!(read_input_box(&screen_with_box(&["❯ "])), InputBox::Empty);
    assert_eq!(read_input_box(&screen_with_box(&["❯"])), InputBox::Empty);
    assert_eq!(
        read_input_box(&screen_with_box(&["❯    "])),
        InputBox::Empty
    );
    // A non-breaking space is a space.
    assert_eq!(
        read_input_box(&screen_with_box(&["❯\u{a0}"])),
        InputBox::Empty
    );
}

#[test]
fn input_box_with_characters_after_the_prompt_mark_holds_text() {
    assert_eq!(
        read_input_box(&screen_with_box(&["❯ half a sentence"])),
        InputBox::HoldsText
    );
    assert_eq!(
        read_input_box(&screen_with_box(&["❯ x"])),
        InputBox::HoldsText
    );
}

#[test]
fn input_box_more_than_one_row_tall_is_not_found() {
    assert_eq!(
        read_input_box(&screen_with_box(&["❯ first line", "  second line"])),
        InputBox::NotFound
    );
    assert_eq!(
        read_input_box(&screen_with_box(&["❯ ", "  second line"])),
        InputBox::NotFound
    );
}

#[test]
fn screen_without_a_prompt_mark_row_has_no_input_box() {
    let permission_question = [
        "⏺ Bash(cargo test)",
        "",
        RULE,
        " Do you want to proceed?",
        "   1. Yes",
        "   2. No",
        RULE,
    ]
    .join("\n");

    assert_eq!(read_input_box(&permission_question), InputBox::NotFound);
}

#[test]
fn prompt_mark_row_without_a_rule_above_and_below_is_not_the_input_box() {
    let no_rule_above = ["⏺ Done.", "❯ ", RULE, FOOTER].join("\n");
    let no_rule_below = ["⏺ Done.", RULE, "❯ ", FOOTER].join("\n");
    // Only the lowest prompt mark row is looked at: an earlier prompt in the
    // transcript is not the input box even between two rules.
    let lowest_is_transcript = [RULE, "❯ ", RULE, "❯ an earlier prompt", "⏺ Done."].join("\n");

    assert_eq!(read_input_box(&no_rule_above), InputBox::NotFound);
    assert_eq!(read_input_box(&no_rule_below), InputBox::NotFound);
    assert_eq!(read_input_box(&lowest_is_transcript), InputBox::NotFound);
}

#[test]
fn input_box_at_the_edge_of_the_screen_is_not_found() {
    assert_eq!(read_input_box(""), InputBox::NotFound);
    assert_eq!(read_input_box("❯ "), InputBox::NotFound);
    assert_eq!(read_input_box(&format!("❯ \n{RULE}")), InputBox::NotFound);
    assert_eq!(read_input_box(&format!("{RULE}\n❯ ")), InputBox::NotFound);
    // Rows that end in a carriage return read the same as rows that do not.
    let with_carriage_returns = screen_with_box(&["❯ "]).replace('\n', "\r\n");
    assert_eq!(read_input_box(&with_carriage_returns), InputBox::Empty);
}

#[test]
fn box_reason_gives_check_6_and_check_7() {
    assert_eq!(box_reason(None), Some(Reason::InputBoxNotFound));
    assert_eq!(
        box_reason(Some("no input box here")),
        Some(Reason::InputBoxNotFound)
    );
    assert_eq!(
        box_reason(Some(&screen_with_box(&["❯ half a sentence"]))),
        Some(Reason::InputBoxHoldsText)
    );
    assert_eq!(box_reason(Some(&screen_with_box(&["❯ "]))), None);
}

// --- reasons ---

#[test]
fn each_reason_has_the_name_the_log_and_the_event_carry() {
    let names = [
        (Reason::PaneListFailing, "pane_list_failing"),
        (Reason::NoAgentInFolder, "no_agent_in_folder"),
        (
            Reason::SeveralAgentsInFolder { agents: 2 },
            "several_agents_in_folder",
        ),
        (Reason::InputBoxUnreadable, "input_box_unreadable"),
        (Reason::AgentBusy, "agent_busy"),
        (Reason::AgentStateUnknown, "agent_state_unknown"),
        (Reason::InputBoxNotFound, "input_box_not_found"),
        (Reason::InputBoxHoldsText, "input_box_holds_text"),
        (Reason::TypingFailed, "typing_failed"),
        (Reason::WindowPassed, "window_passed"),
    ];
    for (reason, name) in names {
        assert_eq!(reason.as_str(), name);
    }
}

// --- the panes ---

fn chief(pane_id: &str) -> AgentPane {
    AgentPane {
        pane_id: pane_id.into(),
        folder: CHIEF.into(),
        reads_as_claude: true,
        state: EffectiveAgentState::Idle,
        session: "pods".into(),
        window_index: 0,
        pane_index: 1,
        started_at_ms: 1_000,
    }
}

fn view(panes: Vec<AgentPane>) -> AgentsView {
    AgentsView {
        listing_failed: false,
        panes,
    }
}

#[test]
fn the_one_idle_claude_pane_in_the_folder_is_picked() {
    let elsewhere = AgentPane {
        folder: "/work/harold".into(),
        ..chief("%5")
    };
    let subfolder = AgentPane {
        folder: "/work/chief-of-pods/sub".into(),
        ..chief("%6")
    };
    let agents = view(vec![elsewhere, chief("%2"), subfolder]);

    assert_eq!(
        pick_pane(&agents, CHIEF, Chief::default(), &[], false)
            .unwrap()
            .pane_id,
        "%2"
    );
    // One trailing slash on either side names the same folder.
    assert_eq!(
        pick_pane(
            &agents,
            "/work/chief-of-pods/",
            Chief::default(),
            &[],
            false
        )
        .unwrap()
        .pane_id,
        "%2"
    );
}

#[test]
fn each_of_checks_1_to_5_gives_its_reason_in_order() {
    let codex = AgentPane {
        reads_as_claude: false,
        state: EffectiveAgentState::Busy,
        ..chief("%2")
    };
    let busy = AgentPane {
        state: EffectiveAgentState::Busy,
        ..chief("%2")
    };
    let unknown = AgentPane {
        state: EffectiveAgentState::Unknown,
        ..chief("%2")
    };
    let failing = AgentsView {
        listing_failed: true,
        panes: vec![chief("%2")],
    };

    assert_eq!(
        pick_pane(&failing, CHIEF, Chief::default(), &[], false),
        Err(Reason::PaneListFailing)
    );
    assert_eq!(
        pick_pane(&view(vec![]), CHIEF, Chief::default(), &[], false),
        Err(Reason::NoAgentInFolder)
    );
    assert_eq!(
        pick_pane(
            &view(vec![chief("%5")]),
            "/work/harold",
            Chief::default(),
            &[],
            false,
        ),
        Err(Reason::NoAgentInFolder)
    );
    assert_eq!(
        pick_pane(
            &view(vec![chief("%2"), chief("%3"), codex.clone()]),
            CHIEF,
            Chief::default(),
            &[],
            false,
        ),
        Err(Reason::SeveralAgentsInFolder { agents: 3 })
    );
    // A busy Codex pane fails check 4 before check 5.
    assert_eq!(
        pick_pane(&view(vec![codex]), CHIEF, Chief::default(), &[], false),
        Err(Reason::InputBoxUnreadable)
    );
    assert_eq!(
        pick_pane(&view(vec![busy]), CHIEF, Chief::default(), &[], false),
        Err(Reason::AgentBusy)
    );
    assert_eq!(
        pick_pane(&view(vec![unknown]), CHIEF, Chief::default(), &[], false),
        Err(Reason::AgentStateUnknown)
    );
}

#[test]
fn pane_typed_into_this_round_counts_as_busy() {
    let agents = view(vec![chief("%2")]);

    assert_eq!(
        pick_pane(&agents, CHIEF, Chief::default(), &["%2".to_string()], false),
        Err(Reason::AgentBusy)
    );
    assert!(pick_pane(&agents, CHIEF, Chief::default(), &["%9".to_string()], false).is_ok());
}

pub(crate) fn projection(
    pane_id: &str,
    provider_id: &str,
    folder: &str,
    state: EffectiveAgentState,
) -> AgentPaneProjection {
    AgentPaneProjection {
        pane: AgentPaneObservation {
            incarnation: AgentIncarnation {
                pane_id: pane_id.into(),
                pane_pid: 10,
                agent_pid: 20,
                agent_started_at_ms: 1_000,
                provider_id: provider_id.into(),
            },
            tmux_target: "pods:0.1".into(),
            session_name: "pods".into(),
            window_index: 0,
            pane_index: 1,
            working_directory: folder.into(),
            provider_display_name: provider_id.into(),
            observed_at_ms: 2_000,
        },
        hook_state: None,
        hook_observed_at_ms: None,
        lifecycle_adapter_id: None,
        screen_state: None,
        screen_classifier_id: None,
        screen_observed_at_ms: None,
        effective_state: state,
        explicit_work_summary: None,
        explicit_work_summary_updated_at_ms: None,
        screen_work_summary: None,
        screen_work_summary_updated_at_ms: None,
        summary_basis_version: EventVersion::start(),
        generated_work_summary: None,
        generated_summary_basis_version: None,
        work_summary: None,
        last_transition_at_ms: 2_000,
        last_event_version: EventVersion::start(),
    }
}

pub(crate) fn provider(id: &str, screen_adapter: ScreenAdapter) -> AgentProviderSettings {
    AgentProviderSettings {
        id: id.into(),
        display_name: id.into(),
        command_contains: vec![id.into()],
        busy_all: Vec::new(),
        idle_all: Vec::new(),
        summary_line_prefixes: Vec::new(),
        screen_adapter,
        screen_history_lines: 2000,
    }
}

pub(crate) fn snapshot(healthy: Option<bool>, panes: Vec<AgentPaneProjection>) -> AgentSnapshot {
    AgentSnapshot {
        through_event_version: EventVersion::start(),
        server_time_ms: 3_000,
        monitor_health: healthy
            .map(|healthy| MonitorHealthProjection {
                component: "inventory".into(),
                healthy,
                reason_code: (if healthy { "ok" } else { "timeout" }).into(),
                observed_at_ms: 2_500,
                last_event_version: EventVersion::start(),
            })
            .into_iter()
            .collect(),
        panes,
        tts_muted: false,
        messaging_paused: false,
    }
}

#[test]
fn agents_view_gives_each_pane_its_folder_state_and_whether_it_is_claude() {
    let providers = [
        provider("codex", ScreenAdapter::CodexV1),
        provider("claude", ScreenAdapter::ClaudeV1),
        provider("opencode", ScreenAdapter::GenericV1),
    ];
    let snapshot = snapshot(
        None,
        vec![
            projection("%2", "claude", CHIEF, EffectiveAgentState::Idle),
            projection("%3", "codex", "/work/harold", EffectiveAgentState::Busy),
            projection("%4", "opencode", "/work/x", EffectiveAgentState::Unknown),
            projection(
                "%5",
                "removed-provider",
                "/work/y",
                EffectiveAgentState::Idle,
            ),
        ],
    );

    let agents = agents_view(&snapshot, &providers);

    assert!(!agents.listing_failed);
    assert_eq!(agents.panes[0], chief("%2"));
    assert_eq!(
        agents.panes[1],
        AgentPane {
            pane_id: "%3".into(),
            folder: "/work/harold".into(),
            reads_as_claude: false,
            state: EffectiveAgentState::Busy,
            session: "pods".into(),
            window_index: 0,
            pane_index: 1,
            started_at_ms: 1_000,
        }
    );
    assert!(!agents.panes[2].reads_as_claude);
    assert_eq!(agents.panes[2].state, EffectiveAgentState::Unknown);
    assert!(!agents.panes[3].reads_as_claude);
}

#[test]
fn agents_view_says_the_listing_failed_only_while_the_inventory_is_unhealthy() {
    let panes = || vec![projection("%2", "claude", CHIEF, EffectiveAgentState::Idle)];
    let providers = [provider("claude", ScreenAdapter::ClaudeV1)];

    assert!(agents_view(&snapshot(Some(false), panes()), &providers).listing_failed);
    assert!(!agents_view(&snapshot(Some(true), panes()), &providers).listing_failed);
    assert!(!agents_view(&snapshot(None, panes()), &providers).listing_failed);
    let mut screen_unhealthy = snapshot(Some(false), panes());
    screen_unhealthy.monitor_health[0].component = "screen".into();
    assert!(!agents_view(&screen_unhealthy, &providers).listing_failed);
}

// --- the round ---

const DAY: &str = "2026-10-14";
const NEXT_DAY: &str = "2026-10-15";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Key {
    /// Pane id and text.
    Text(String, String),
    /// Pane id.
    Enter(String),
}

pub(crate) struct FakeState {
    now: Option<LocalNow>,
    pub(crate) view: AgentsView,
    pub(crate) screen: Option<String>,
    fail_text: bool,
    pub(crate) fail_enter: bool,
    pub(crate) keys: Vec<Key>,
    now_reads: usize,
    screen_reads: usize,
}

/// Every port, faked. It starts with the chief idle in pane `%2` with an empty box.
#[derive(Clone)]
pub(crate) struct Fake(Arc<Mutex<FakeState>>, Arc<PaneLocks>, Chief<'static>);

impl Fake {
    pub(crate) fn at(day: &str, time: &str) -> Self {
        let fake = Self(
            Arc::new(Mutex::new(FakeState {
                now: None,
                view: view(vec![chief("%2")]),
                screen: Some(screen_with_box(&["❯ "])),
                fail_text: false,
                fail_enter: false,
                keys: Vec::new(),
                now_reads: 0,
                screen_reads: 0,
            })),
            Arc::default(),
            Chief::default(),
        );
        fake.clock(day, time);
        fake
    }

    /// The same fake, with `[notify] chief` and `chief_session` set.
    pub(crate) fn with_chief(
        mut self,
        folder: &'static str,
        session: Option<&'static str>,
    ) -> Self {
        self.2 = Chief {
            folder: Some(folder),
            session,
        };
        self
    }

    pub(crate) fn set(&self, change: impl FnOnce(&mut FakeState)) {
        change(&mut self.0.lock().unwrap());
    }

    fn clock(&self, day: &str, time: &str) {
        let now = parse_local_now(&format!("{day} {time}")).expect("a valid test time");
        self.set(|state| state.now = Some(now));
    }

    pub(crate) fn keys(&self) -> Vec<Key> {
        self.0.lock().unwrap().keys.clone()
    }

    fn screen_reads(&self) -> usize {
        self.0.lock().unwrap().screen_reads
    }

    fn now_reads(&self) -> usize {
        self.0.lock().unwrap().now_reads
    }
}

impl Ports for Fake {
    async fn now(&self) -> Option<LocalNow> {
        let mut state = self.0.lock().unwrap();
        state.now_reads += 1;
        state.now.clone()
    }

    fn agents(&self) -> AgentsView {
        self.0.lock().unwrap().view.clone()
    }

    async fn read_screen(&self, _pane_id: &str) -> Option<String> {
        let mut state = self.0.lock().unwrap();
        state.screen_reads += 1;
        state.screen.clone()
    }

    async fn type_text(&self, pane_id: &str, text: &str) -> Result<(), String> {
        {
            let mut state = self.0.lock().unwrap();
            if state.fail_text {
                return Err("pane closed".into());
            }
            state.keys.push(Key::Text(pane_id.into(), text.into()));
        }
        // Typing takes time: another task may run before Enter is pressed.
        tokio::task::yield_now().await;
        Ok(())
    }

    async fn press_enter(&self, pane_id: &str) -> Result<(), String> {
        let mut state = self.0.lock().unwrap();
        if state.fail_enter {
            return Err("enter refused".into());
        }
        state.keys.push(Key::Enter(pane_id.into()));
        Ok(())
    }

    fn pane_locks(&self) -> &PaneLocks {
        &self.1
    }

    fn chief(&self) -> Chief<'_> {
        self.2
    }
}

/// A real event store in a temporary folder, removed when dropped.
struct TestStore {
    path: PathBuf,
    store: Arc<HaroldStore>,
}

impl TestStore {
    async fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("harold-timed-prompts-{}", uuid::Uuid::new_v4()));
        let store = Arc::new(HaroldStore::open(&path).await.expect("open test store"));
        Self { path, store }
    }
}

impl TestStore {
    /// A store Harold opened before: created, closed and opened again, so it is not new.
    async fn existing() -> Self {
        let path =
            std::env::temp_dir().join(format!("harold-timed-prompts-{}", uuid::Uuid::new_v4()));
        drop(HaroldStore::open(&path).await.expect("create test store"));
        let store = Arc::new(HaroldStore::open(&path).await.expect("reopen test store"));
        Self { path, store }
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The `TimedPromptHandled` events in the store, oldest first.
async fn handled(store: &TestStore) -> Vec<TimedPromptHandled> {
    store
        .store
        .stream()
        .load_after_version(EventVersion::start(), 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.r#type.as_str() == "TimedPromptHandled")
        .map(|event| serde_json::from_value(event.payload).unwrap())
        .collect()
}

fn entry(name: &str, at: &str) -> TimedPromptSettings {
    TimedPromptSettings {
        name: name.into(),
        at: at.into(),
        folder: CHIEF.into(),
        text: "digest".into(),
        give_up_after_minutes: 120,
    }
}

fn prompts(fake: &Fake, store: &TestStore, entries: &[TimedPromptSettings]) -> TimedPrompts<Fake> {
    TimedPrompts::new(
        entries,
        &HashMap::new(),
        fake.clone(),
        Arc::clone(&store.store),
    )
}

fn digest_keys() -> Vec<Key> {
    vec![
        Key::Text("%2".into(), "digest".into()),
        Key::Enter("%2".into()),
    ]
}

fn delayed(name: &str, reason: Reason) -> Report {
    Report::Delayed {
        name: name.into(),
        reason,
    }
}

fn skipped(name: &str, reason: Reason) -> Report {
    Report::Skipped {
        name: name.into(),
        reason,
    }
}

#[tokio::test]
async fn due_prompt_is_typed_once_into_the_right_pane_and_stored() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:04");
    fake.set(|state| {
        state.view.panes.insert(
            0,
            AgentPane {
                folder: "/work/harold".into(),
                ..chief("%1")
            },
        )
    });
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    let reports = prompts.round().await;

    assert_eq!(
        reports,
        [Report::Typed {
            name: "digest".into(),
            pane: "%2".into(),
            late_s: 4,
        }]
    );
    assert_eq!(fake.keys(), digest_keys());
    assert_eq!(
        handled(&store).await,
        [TimedPromptHandled {
            name: "digest".into(),
            day: DAY.into(),
            outcome: TimedPromptOutcome::Typed,
            reason: None,
            pane_id: Some("%2".into()),
        }]
    );

    fake.clock(DAY, "09:00:09");
    assert!(prompts.round().await.is_empty());
    fake.clock(DAY, "10:59:59");
    assert!(prompts.round().await.is_empty());
    assert_eq!(fake.keys(), digest_keys());
    assert_eq!(handled(&store).await.len(), 1);
}

#[tokio::test]
async fn prompt_is_left_alone_before_its_time() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "08:59:59");
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    assert!(prompts.round().await.is_empty());
    assert!(fake.keys().is_empty());
    assert_eq!(fake.screen_reads(), 0);
    assert!(handled(&store).await.is_empty());
}

#[tokio::test]
async fn each_failing_check_types_nothing_and_gives_its_reason() {
    type Break = fn(&mut FakeState);
    // A function argument is where a closure becomes a `fn` pointer.
    fn case(break_one_check: Break, reason: Reason, screen_reads: usize) -> (Break, Reason, usize) {
        (break_one_check, reason, screen_reads)
    }
    // Whether the screen is read tells checks 1 to 5 from checks 6 and 7.
    let cases = [
        case(
            |state| state.view.listing_failed = true,
            Reason::PaneListFailing,
            0,
        ),
        case(|state| state.view.panes.clear(), Reason::NoAgentInFolder, 0),
        case(
            |state| state.view.panes.push(chief("%3")),
            Reason::SeveralAgentsInFolder { agents: 2 },
            0,
        ),
        case(
            |state| state.view.panes[0].reads_as_claude = false,
            Reason::InputBoxUnreadable,
            0,
        ),
        case(
            |state| state.view.panes[0].state = EffectiveAgentState::Busy,
            Reason::AgentBusy,
            0,
        ),
        case(
            |state| state.view.panes[0].state = EffectiveAgentState::Unknown,
            Reason::AgentStateUnknown,
            0,
        ),
        case(|state| state.screen = None, Reason::InputBoxNotFound, 1),
        case(
            |state| state.screen = Some(screen_with_box(&["❯ half a sentence"])),
            Reason::InputBoxHoldsText,
            1,
        ),
    ];
    for (break_one_check, reason, screen_reads) in cases {
        let store = TestStore::new().await;
        let fake = Fake::at(DAY, "09:00:00");
        fake.set(break_one_check);
        let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

        let reports = prompts.round().await;

        assert_eq!(reports, [delayed("digest", reason.clone())], "{reason:?}");
        assert!(fake.keys().is_empty(), "{reason:?}");
        assert_eq!(fake.screen_reads(), screen_reads, "{reason:?}");
        assert!(handled(&store).await.is_empty(), "{reason:?}");
    }
}

#[tokio::test]
async fn waiting_is_reported_once_for_each_reason() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Busy);
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    let mut reports = Vec::new();
    for _ in 0..10 {
        reports.extend(prompts.round().await);
    }
    assert_eq!(reports, [delayed("digest", Reason::AgentBusy)]);

    fake.set(|state| {
        state.view.panes[0].state = EffectiveAgentState::Idle;
        state.screen = Some(screen_with_box(&["❯ half a sentence"]));
    });
    let mut reports = Vec::new();
    for _ in 0..10 {
        reports.extend(prompts.round().await);
    }
    assert_eq!(reports, [delayed("digest", Reason::InputBoxHoldsText)]);

    // Going back to an earlier reason is a change, and is reported.
    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Busy);
    assert_eq!(
        prompts.round().await,
        [delayed("digest", Reason::AgentBusy)]
    );
    assert!(fake.keys().is_empty());
    assert!(handled(&store).await.is_empty());
}

#[tokio::test]
async fn a_changed_count_of_agents_is_not_a_changed_reason() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view.panes.push(chief("%3")));
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    assert_eq!(
        prompts.round().await,
        [delayed(
            "digest",
            Reason::SeveralAgentsInFolder { agents: 2 }
        )]
    );
    fake.set(|state| state.view.panes.push(chief("%4")));
    assert!(prompts.round().await.is_empty());
}

#[tokio::test]
async fn waiting_prompt_is_typed_once_the_checks_pass() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Busy);
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);
    prompts.round().await;

    fake.clock(DAY, "09:07:02");
    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Idle);

    assert_eq!(
        prompts.round().await,
        [Report::Typed {
            name: "digest".into(),
            pane: "%2".into(),
            late_s: 422,
        }]
    );
    assert_eq!(fake.keys(), digest_keys());
}

#[tokio::test]
async fn prompt_still_waiting_when_its_window_ends_is_skipped_with_its_last_reason() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Busy);
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);
    prompts.round().await;
    fake.clock(DAY, "10:59:55");
    fake.set(|state| {
        state.view.panes[0].state = EffectiveAgentState::Idle;
        state.screen = Some(screen_with_box(&["❯ half a sentence"]));
    });
    prompts.round().await;

    fake.clock(DAY, "11:00:00");
    // The box is empty again, one round too late.
    fake.set(|state| state.screen = Some(screen_with_box(&["❯ "])));
    let reports = prompts.round().await;

    assert_eq!(reports, [skipped("digest", Reason::InputBoxHoldsText)]);
    assert!(fake.keys().is_empty());
    assert_eq!(
        handled(&store).await,
        [TimedPromptHandled {
            name: "digest".into(),
            day: DAY.into(),
            outcome: TimedPromptOutcome::Skipped,
            reason: Some("input_box_holds_text".into()),
            pane_id: None,
        }]
    );
    fake.clock(DAY, "11:00:05");
    assert!(prompts.round().await.is_empty());
    assert_eq!(handled(&store).await.len(), 1);
}

#[tokio::test]
async fn prompt_never_seen_due_is_skipped_as_window_passed_and_typed_the_next_day() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "15:00:00");
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    assert_eq!(
        prompts.round().await,
        [skipped("digest", Reason::WindowPassed)]
    );
    assert!(prompts.round().await.is_empty());
    assert!(fake.keys().is_empty());
    let stored = handled(&store).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].outcome, TimedPromptOutcome::Skipped);
    assert_eq!(stored[0].reason.as_deref(), Some("window_passed"));
    assert_eq!(stored[0].pane_id, None);

    fake.clock(NEXT_DAY, "09:00:00");
    assert_eq!(
        prompts.round().await,
        [Report::Typed {
            name: "digest".into(),
            pane: "%2".into(),
            late_s: 0,
        }]
    );
    assert_eq!(fake.keys(), digest_keys());
}

#[tokio::test]
async fn typed_prompt_is_not_typed_again_after_a_restart_and_is_typed_the_next_day() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let entries = [entry("digest", "09:00")];
    prompts(&fake, &store, &entries).round().await;
    assert_eq!(fake.keys(), digest_keys());

    // A restart: Harold reads the whole stream into its state database, then starts
    // the task from the stored rows.
    while store
        .store
        .project_unhandled_events(500)
        .await
        .unwrap()
        .applied
        > 0
    {}
    let last_days = store.store.timed_prompt_last_days().await.unwrap();
    assert_eq!(last_days.get("digest").map(String::as_str), Some(DAY));
    let mut restarted =
        TimedPrompts::new(&entries, &last_days, fake.clone(), Arc::clone(&store.store));

    fake.clock(DAY, "09:05:00");
    assert!(restarted.round().await.is_empty());
    assert_eq!(fake.keys(), digest_keys());

    fake.clock(NEXT_DAY, "09:00:00");
    assert_eq!(restarted.round().await.len(), 1);
    assert_eq!(fake.keys().len(), 4);
    assert_eq!(handled(&store).await.len(), 2);
}

#[tokio::test]
async fn renamed_entry_counts_as_a_new_prompt() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:05:00");
    let last_days = HashMap::from([("digest".to_string(), DAY.to_string())]);
    let mut renamed = TimedPrompts::new(
        &[entry("morning-digest", "09:00")],
        &last_days,
        fake.clone(),
        Arc::clone(&store.store),
    );

    assert_eq!(renamed.round().await.len(), 1);
    assert_eq!(fake.keys(), digest_keys());
}

#[tokio::test]
async fn prompt_waiting_at_midnight_is_skipped_for_the_earlier_date() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "23:59:59");
    fake.set(|state| state.screen = Some(screen_with_box(&["❯ half a sentence"])));
    let mut prompts = prompts(&fake, &store, &[entry("digest", "23:30")]);
    assert_eq!(
        prompts.round().await,
        [delayed("digest", Reason::InputBoxHoldsText)]
    );

    fake.clock(NEXT_DAY, "00:00:04");
    assert_eq!(
        prompts.round().await,
        [skipped("digest", Reason::InputBoxHoldsText)]
    );
    assert!(prompts.round().await.is_empty());
    let stored = handled(&store).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].day, DAY);
    assert_eq!(stored[0].outcome, TimedPromptOutcome::Skipped);
    assert!(fake.keys().is_empty());

    // The earlier date's skip does not stand in for the new date.
    fake.clock(NEXT_DAY, "23:30:00");
    fake.set(|state| state.screen = Some(screen_with_box(&["❯ "])));
    assert_eq!(prompts.round().await.len(), 1);
    assert_eq!(fake.keys(), digest_keys());
    assert_eq!(handled(&store).await[1].day, NEXT_DAY);
}

#[tokio::test]
async fn text_that_fails_to_type_is_stored_as_nothing_and_tried_again() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.fail_text = true);
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    assert_eq!(
        prompts.round().await,
        [delayed("digest", Reason::TypingFailed)]
    );
    assert!(fake.keys().is_empty());
    assert!(handled(&store).await.is_empty());

    fake.set(|state| state.fail_text = false);
    fake.clock(DAY, "09:00:05");
    assert_eq!(prompts.round().await.len(), 1);
    assert_eq!(fake.keys(), digest_keys());
    assert_eq!(handled(&store).await[0].outcome, TimedPromptOutcome::Typed);
}

#[tokio::test]
async fn text_that_cannot_be_typed_is_delayed_once_and_skipped_with_that_reason() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.fail_text = true);
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    let mut reports = Vec::new();
    for _ in 0..10 {
        reports.extend(prompts.round().await);
    }
    assert_eq!(reports, [delayed("digest", Reason::TypingFailed)]);

    fake.clock(DAY, "11:00:00");
    assert_eq!(
        prompts.round().await,
        [skipped("digest", Reason::TypingFailed)]
    );
    assert_eq!(
        handled(&store).await[0].reason.as_deref(),
        Some("typing_failed")
    );
}

#[tokio::test]
async fn enter_that_fails_is_stored_as_failed_and_never_typed_again() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.fail_enter = true);
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    assert_eq!(
        prompts.round().await,
        [Report::Failed {
            name: "digest".into(),
            pane: "%2".into(),
            error: "enter refused".into(),
        }]
    );
    assert_eq!(fake.keys(), [Key::Text("%2".into(), "digest".into())]);
    assert_eq!(
        handled(&store).await,
        [TimedPromptHandled {
            name: "digest".into(),
            day: DAY.into(),
            outcome: TimedPromptOutcome::Failed,
            reason: None,
            pane_id: Some("%2".into()),
        }]
    );

    fake.set(|state| state.fail_enter = false);
    fake.clock(DAY, "09:00:05");
    assert!(prompts.round().await.is_empty());
    assert_eq!(fake.keys().len(), 1);
    assert_eq!(handled(&store).await.len(), 1);
}

#[tokio::test]
async fn prompt_is_not_typed_again_when_its_event_cannot_be_stored() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);
    store.store.fail_next_monitor_append_for_test();

    let reports = prompts.round().await;

    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(matches!(reports[0], Report::Typed { .. }));
    assert!(
        matches!(&reports[1], Report::RecordFailed { name, .. } if name == "digest"),
        "{reports:?}"
    );
    assert!(handled(&store).await.is_empty());

    fake.clock(DAY, "09:00:05");
    assert!(prompts.round().await.is_empty());
    assert_eq!(fake.keys(), digest_keys());
}

#[tokio::test]
async fn two_entries_for_one_folder_and_time_are_typed_in_list_order() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let second = TimedPromptSettings {
        text: "second text".into(),
        ..entry("second", "09:00")
    };
    let mut prompts = prompts(&fake, &store, &[entry("first", "09:00"), second]);

    let reports = prompts.round().await;
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(matches!(&reports[0], Report::Typed { name, .. } if name == "first"));
    assert_eq!(reports[1], delayed("second", Reason::AgentBusy));
    assert_eq!(fake.keys(), digest_keys());

    // The snapshot has not caught up: it still says idle on the next round.
    fake.clock(DAY, "09:00:05");
    assert!(prompts.round().await.is_empty());
    assert_eq!(fake.keys().len(), 2);

    // The agent is working on the first text.
    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Busy);
    fake.clock(DAY, "09:00:10");
    assert!(prompts.round().await.is_empty());
    assert_eq!(fake.keys().len(), 2);

    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Idle);
    fake.clock(DAY, "09:03:00");
    let reports = prompts.round().await;
    assert!(matches!(&reports[..], [Report::Typed { name, .. }] if name == "second"));
    assert_eq!(fake.keys()[2..], second_keys());
}

fn second_keys() -> Vec<Key> {
    vec![
        Key::Text("%2".into(), "second text".into()),
        Key::Enter("%2".into()),
    ]
}

/// Two entries for the chief's folder, both at 09:00, after the round at 09:00:00 that
/// types the first and reports the second as waiting with `agent_busy`.
async fn first_of_two_typed(fake: &Fake, store: &TestStore) -> TimedPrompts<Fake> {
    let second = TimedPromptSettings {
        text: "second text".into(),
        ..entry("second", "09:00")
    };
    let mut prompts = prompts(fake, store, &[entry("first", "09:00"), second]);
    let reports = prompts.round().await;
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert_eq!(reports[1], delayed("second", Reason::AgentBusy));
    assert_eq!(fake.keys(), digest_keys());
    prompts
}

#[tokio::test]
async fn pane_typed_into_counts_as_busy_for_twelve_more_rounds_while_it_reads_idle() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let mut prompts = first_of_two_typed(&fake, &store).await;

    for round in 1..=12 {
        assert!(prompts.round().await.is_empty(), "round {round}");
        assert_eq!(fake.keys(), digest_keys(), "round {round}");
    }

    let reports = prompts.round().await;
    assert!(
        matches!(&reports[..], [Report::Typed { name, .. }] if name == "second"),
        "{reports:?}"
    );
    assert_eq!(fake.keys()[2..], second_keys());
}

#[tokio::test]
async fn pane_typed_into_that_goes_away_and_comes_back_idle_is_free_again() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let mut prompts = first_of_two_typed(&fake, &store).await;

    assert!(prompts.round().await.is_empty());
    fake.set(|state| state.view.panes.clear());
    assert_eq!(
        prompts.round().await,
        [delayed("second", Reason::NoAgentInFolder)]
    );
    assert_eq!(fake.keys(), digest_keys());

    fake.set(|state| state.view.panes.push(chief("%2")));
    let reports = prompts.round().await;
    assert!(
        matches!(&reports[..], [Report::Typed { name, .. }] if name == "second"),
        "{reports:?}"
    );
    assert_eq!(fake.keys()[2..], second_keys());
}

#[tokio::test]
async fn empty_list_does_nothing() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let mut prompts = prompts(&fake, &store, &[]);

    assert!(prompts.is_empty());
    assert!(prompts.round().await.is_empty());
    assert_eq!(fake.now_reads(), 0);
}

#[tokio::test]
async fn unreadable_clock_is_reported_once_and_nothing_is_typed() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.now = None);
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    assert_eq!(prompts.round().await, [Report::ClockUnreadable]);
    assert!(prompts.round().await.is_empty());
    assert!(fake.keys().is_empty());

    fake.clock(DAY, "09:00:10");
    assert_eq!(prompts.round().await.len(), 1);
    fake.set(|state| state.now = None);
    assert_eq!(prompts.round().await, [Report::ClockUnreadable]);
}

#[tokio::test]
async fn no_report_carries_the_text() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let secret = TimedPromptSettings {
        text: "SENTINEL-TEXT".into(),
        ..entry("digest", "09:00")
    };
    let never_typed = TimedPromptSettings {
        text: "SENTINEL-TEXT".into(),
        ..entry("later", "09:00")
    };
    let mut prompts = prompts(&fake, &store, &[secret, never_typed]);

    let mut reports = prompts.round().await;
    fake.clock(DAY, "11:00:00");
    reports.extend(prompts.round().await);

    assert_eq!(reports.len(), 3, "{reports:?}");
    assert!(!format!("{reports:?}").contains("SENTINEL-TEXT"));
    assert!(!format!("{:?}", handled(&store).await).contains("SENTINEL-TEXT"));
}

// --- the real ports and the loop ---

#[tokio::test]
async fn bounded_stdout_gives_the_output_of_a_command_that_succeeds_in_time() {
    let second = Duration::from_secs(1);

    assert_eq!(
        bounded_stdout("/bin/sh", &["-c", "printf safe"], second).await,
        Some("safe".to_string())
    );
    assert_eq!(
        bounded_stdout("/bin/sh", &["-c", "printf lost; exit 3"], second).await,
        None
    );
    assert_eq!(
        bounded_stdout("/nonexistent/harold-test-command", &[], second).await,
        None
    );

    let started = std::time::Instant::now();
    assert_eq!(
        bounded_stdout("/bin/sleep", &["5"], Duration::from_millis(50)).await,
        None
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn the_local_time_is_read_from_bin_date() {
    let text = bounded_stdout(DATE_PROGRAM, &DATE_ARGS, COMMAND_TIMEOUT)
        .await
        .expect("/bin/date runs");
    let now = parse_local_now(&text).expect("its output is a date and a time");

    assert_eq!(now.day.len(), 10);
    assert!(now.seconds_of_day < 86_400);
}

#[test]
fn screen_is_read_as_the_visible_rows_of_one_pane() {
    assert_eq!(capture_args("%7"), ["capture-pane", "-p", "-t", "%7"]);
}

#[test]
fn the_round_runs_every_5_seconds_and_a_capture_may_take_2() {
    assert_eq!(ROUND_INTERVAL, Duration::from_secs(5));
    assert_eq!(COMMAND_TIMEOUT, Duration::from_secs(2));
}

pub(crate) async fn wait_until(what: &str, mut holds: impl FnMut() -> bool) {
    for _ in 0..200 {
        if holds() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting until {what}");
}

#[tokio::test]
async fn task_types_a_due_prompt_and_stops_at_once_on_shutdown() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let task = tokio::spawn(run(
        prompts(&fake, &store, &[entry("digest", "09:00")]),
        Duration::ZERO,
        shutdown_rx,
    ));

    // The first round runs when the task starts, not 5 seconds later.
    wait_until("the prompt is typed", || fake.keys().len() == 2).await;
    assert_eq!(fake.keys(), digest_keys());

    drop(shutdown_tx);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("the task stops within a second of shutdown")
        .unwrap();
}

#[tokio::test]
async fn task_stops_at_once_while_a_prompt_is_waiting() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view.panes[0].state = EffectiveAgentState::Busy);
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let task = tokio::spawn(run(
        prompts(&fake, &store, &[entry("digest", "09:00")]),
        Duration::ZERO,
        shutdown_rx,
    ));
    wait_until("the first round has run", || fake.now_reads() >= 1).await;

    drop(shutdown_tx);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("the task stops within a second of shutdown")
        .unwrap();
    assert!(fake.keys().is_empty());
    assert!(handled(&store).await.is_empty());
}

#[tokio::test]
async fn daemon_ports_read_the_panes_from_the_published_snapshot() {
    let hub = AgentSnapshotHub::new(snapshot(
        Some(false),
        vec![projection("%2", "claude", CHIEF, EffectiveAgentState::Idle)],
    ));
    let ports = TmuxPorts {
        snapshots: hub,
        providers: vec![provider("claude", ScreenAdapter::ClaudeV1)],
        chief_folder: None,
        chief_session: None,
    };

    let agents = ports.agents();

    assert!(agents.listing_failed);
    assert_eq!(agents.panes, [chief("%2")]);
}

#[tokio::test]
async fn start_with_no_prompts_gives_a_task_that_ends_at_once() {
    crate::settings::init_settings_for_test();
    let settings = crate::settings::get_settings();
    // With a prompt configured, `start` would run the real ports. This test must not.
    assert!(settings.timed_prompts.is_empty());
    let store = TestStore::new().await;
    let hub = AgentSnapshotHub::new(snapshot(None, Vec::new()));
    let (_shutdown_tx, shutdown_rx) = watch::channel(());

    let task = start(settings, Arc::clone(&store.store), hub, shutdown_rx)
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("a task with nothing to do ends at once")
        .unwrap();
}

#[tokio::test]
async fn task_with_no_prompts_ends_without_reading_the_clock() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let (_shutdown_tx, shutdown_rx) = watch::channel(());

    tokio::time::timeout(
        Duration::from_secs(1),
        run(prompts(&fake, &store, &[]), Duration::ZERO, shutdown_rx),
    )
    .await
    .expect("a task with nothing to do ends at once");
    assert_eq!(fake.now_reads(), 0);
}

// --- the wait before the first round ---

#[test]
fn startup_wait_covers_two_pane_listings_and_a_screen_pass() {
    let settings_default = AgentMonitorSettings::default();
    assert_eq!(
        startup_wait(&settings_default),
        Duration::from_millis(10_500)
    );

    // Each listing may take its interval and its timeout, and there are two.
    let slower_interval = AgentMonitorSettings {
        inventory_interval_ms: 2_000,
        ..AgentMonitorSettings::default()
    };
    assert_eq!(
        startup_wait(&slower_interval),
        Duration::from_millis(12_500)
    );
    let slower_timeout = AgentMonitorSettings {
        inventory_timeout_ms: 5_000,
        ..AgentMonitorSettings::default()
    };
    assert_eq!(startup_wait(&slower_timeout), Duration::from_millis(14_500));
    let slower_screen = AgentMonitorSettings {
        screen_interval_ms: 1_500,
        ..AgentMonitorSettings::default()
    };
    assert_eq!(startup_wait(&slower_screen), Duration::from_millis(11_500));
}

#[tokio::test]
async fn first_round_waits_until_the_startup_wait_has_passed() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let task = tokio::spawn(run(
        prompts(&fake, &store, &[entry("digest", "09:00")]),
        Duration::from_millis(300),
        shutdown_rx,
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.now_reads(), 0);
    assert!(fake.keys().is_empty());

    wait_until("the prompt is typed", || fake.keys().len() == 2).await;
    assert_eq!(fake.keys(), digest_keys());
    task.abort();
}

#[tokio::test]
async fn task_stops_at_once_on_shutdown_during_the_startup_wait() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:00");
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let task = tokio::spawn(run(
        prompts(&fake, &store, &[entry("digest", "09:00")]),
        Duration::from_secs(30),
        shutdown_rx,
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    drop(shutdown_tx);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("the task stops within a second of shutdown")
        .unwrap();
    assert_eq!(fake.now_reads(), 0);
    assert!(fake.keys().is_empty());
}

#[tokio::test]
async fn a_typed_prompt_starts_the_next_turn_on_its_pane() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:04");
    // A pane no other test uses: the typed prompts are kept process-wide.
    let pane = format!("%{}", uuid::Uuid::new_v4().as_u128() % 10u128.pow(18));
    fake.set(|state| state.view = view(vec![chief(&pane)]));
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    let before = time::OffsetDateTime::now_utc();
    prompts.round().await;

    let now = time::OffsetDateTime::now_utc();
    let (typed_at, started_by, cx) =
        crate::telemetry::take_prompt(&pane, now).expect("the typed prompt is kept");
    assert_eq!(started_by, crate::telemetry::StartedBy::TimedPrompt);
    assert!(before <= typed_at && typed_at <= now, "{typed_at}");
    assert!(cx.is_none(), "a timed prompt starts its own trace");
}

#[tokio::test]
async fn a_prompt_whose_enter_fails_starts_no_turn() {
    let store = TestStore::new().await;
    let fake = Fake::at(DAY, "09:00:04");
    let pane = format!("%{}", uuid::Uuid::new_v4().as_u128() % 10u128.pow(18));
    fake.set(|state| {
        state.view = view(vec![chief(&pane)]);
        state.fail_enter = true;
    });
    let mut prompts = prompts(&fake, &store, &[entry("digest", "09:00")]);

    prompts.round().await;

    assert!(crate::telemetry::take_prompt(&pane, time::OffsetDateTime::now_utc()).is_none());
}

#[tokio::test]
async fn a_new_store_marks_prompts_already_past_and_leaves_later_ones() {
    let store = TestStore::new().await;
    assert!(store.store.is_new());
    let fake = Fake::at("2026-10-16", "08:30:00");
    let mut prompts = prompts(
        &fake,
        &store,
        &[entry("digest", "07:50"), entry("evening", "21:00")],
    );

    let reports = prompts.mark_passed_on_new_store().await;
    assert_eq!(
        reports,
        vec![Report::MarkedOnNewStore {
            name: "digest".into()
        }]
    );
    assert_eq!(
        handled(&store).await,
        vec![TimedPromptHandled {
            name: "digest".into(),
            day: "2026-10-16".into(),
            outcome: TimedPromptOutcome::Skipped,
            reason: Some("new_store".into()),
            pane_id: None,
        }]
    );

    // 08:30 is inside digest's 120-minute window, and it is not typed.
    prompts.round().await;
    assert!(fake.keys().is_empty());

    fake.clock("2026-10-16", "21:00:00");
    let reports = prompts.round().await;
    assert!(
        matches!(reports.as_slice(), [Report::Typed { name, .. }] if name == "evening"),
        "{reports:?}"
    );
}

#[tokio::test]
async fn marking_does_nothing_before_any_prompt_time() {
    let store = TestStore::new().await;
    let fake = Fake::at("2026-10-16", "06:00:00");
    let mut prompts = prompts(&fake, &store, &[entry("digest", "07:50")]);
    assert!(prompts.mark_passed_on_new_store().await.is_empty());
    assert!(handled(&store).await.is_empty());
}

#[tokio::test]
async fn an_existing_store_is_opened_as_it_is_and_no_prompt_is_marked() {
    let store = TestStore::existing().await;
    assert!(!store.store.is_new());
    let fake = Fake::at("2026-10-16", "08:30:00");
    let mut prompts = prompts(&fake, &store, &[entry("digest", "07:50")]);

    let reports = mark_passed_if_new_store(&mut prompts, store.store.is_new()).await;

    assert!(reports.is_empty(), "{reports:?}");
    assert!(handled(&store).await.is_empty());
    assert_eq!(fake.now_reads(), 0, "the clock is not read");
}

#[tokio::test]
async fn a_new_store_runs_the_marking_at_start() {
    let store = TestStore::new().await;
    assert!(store.store.is_new());
    let fake = Fake::at("2026-10-16", "08:30:00");
    let mut prompts = prompts(&fake, &store, &[entry("digest", "07:50")]);

    let reports = mark_passed_if_new_store(&mut prompts, store.store.is_new()).await;

    assert_eq!(
        reports,
        vec![Report::MarkedOnNewStore {
            name: "digest".into()
        }]
    );
    assert_eq!(handled(&store).await.len(), 1);
}

// ---------------------------------------------------------------------------
// The voice writer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn voice_is_not_held_by_another_writers_typing() {
    let fake = Fake::at(DAY, "09:00:00");
    let agents = fake.agents();

    let alert = type_when_ready(&fake, Writer::Alert, CHIEF, "alert line", &agents, &[]).await;
    assert!(matches!(alert, Attempt::Typed { .. }));
    // The pane still reads idle, so the alert's hold is on.
    let voice = type_when_ready(&fake, Writer::Voice, CHIEF, "🎙 hi", &agents, &[]).await;
    assert!(matches!(voice, Attempt::Typed { .. }));
    // The voice's typing holds the pane against the writers that are held.
    let timed = type_when_ready(&fake, Writer::TimedPrompt, CHIEF, "prompt", &agents, &[]).await;
    assert!(matches!(timed, Attempt::Wait(Reason::AgentBusy)));
    assert_eq!(
        fake.keys(),
        vec![
            Key::Text("%2".into(), "alert line".into()),
            Key::Enter("%2".into()),
            Key::Text("%2".into(), "🎙 hi".into()),
            Key::Enter("%2".into()),
        ]
    );
}

fn busy_view() -> AgentsView {
    view(vec![AgentPane {
        state: EffectiveAgentState::Busy,
        ..chief("%2")
    }])
}

#[tokio::test]
async fn a_hand_over_types_into_a_busy_claude_code_pane_and_no_other_writer_does() {
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view = busy_view());
    let agents = fake.agents();

    for writer in [
        Writer::TimedPrompt,
        Writer::Alert,
        Writer::PhoneReply,
        Writer::Voice,
        Writer::Tool("questions"),
    ] {
        let waited = type_when_ready(&fake, writer, CHIEF, "line", &agents, &[]).await;
        assert!(
            matches!(waited, Attempt::Wait(Reason::AgentBusy)),
            "{writer:?}"
        );
    }
    assert!(fake.keys().is_empty());

    let typed = type_when_ready(
        &fake,
        Writer::HandOver,
        CHIEF,
        "🎙 hand-over H1: x",
        &agents,
        &[],
    )
    .await;

    assert!(matches!(typed, Attempt::Typed { ref pane } if pane == "%2"));
    assert_eq!(
        fake.keys(),
        vec![
            Key::Text("%2".into(), "🎙 hand-over H1: x".into()),
            Key::Enter("%2".into()),
        ]
    );
}

#[tokio::test]
async fn a_hand_over_still_waits_for_what_every_writer_waits_for() {
    let hand_over = |fake: &Fake| {
        let agents = fake.agents();
        let fake = fake.clone();
        async move {
            type_when_ready(
                &fake,
                Writer::HandOver,
                CHIEF,
                "🎙 hand-over H1: x",
                &agents,
                &[],
            )
            .await
        }
    };
    let waits = |attempt: Attempt, want: Reason| match attempt {
        Attempt::Wait(reason) => assert_eq!(reason, want),
        _ => panic!("expected a wait for {want:?}"),
    };

    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| {
        state.view = AgentsView {
            listing_failed: true,
            ..busy_view()
        }
    });
    waits(hand_over(&fake).await, Reason::PaneListFailing);

    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view = view(vec![]));
    waits(hand_over(&fake).await, Reason::NoAgentInFolder);

    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| {
        state.view = view(vec![AgentPane {
            state: EffectiveAgentState::Unknown,
            ..chief("%2")
        }])
    });
    waits(hand_over(&fake).await, Reason::AgentStateUnknown);

    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| {
        state.view = view(vec![AgentPane {
            reads_as_claude: false,
            state: EffectiveAgentState::Busy,
            ..chief("%2")
        }])
    });
    waits(hand_over(&fake).await, Reason::InputBoxUnreadable);

    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| {
        state.view = busy_view();
        state.screen = Some(screen_with_box(&["❯ half a sentence"]));
    });
    waits(hand_over(&fake).await, Reason::InputBoxHoldsText);

    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| {
        state.view = busy_view();
        state.screen = Some("no box here".to_string());
    });
    waits(hand_over(&fake).await, Reason::InputBoxNotFound);
    assert!(fake.keys().is_empty());
}

#[tokio::test]
async fn a_hand_over_is_not_held_by_another_writer_and_holds_the_pane_once_typed() {
    let fake = Fake::at(DAY, "09:00:00");
    let agents = fake.agents();

    let alert = type_when_ready(&fake, Writer::Alert, CHIEF, "alert line", &agents, &[]).await;
    assert!(matches!(alert, Attempt::Typed { .. }));
    let hand_over = type_when_ready(
        &fake,
        Writer::HandOver,
        CHIEF,
        "🎙 hand-over H1: x",
        &agents,
        &[],
    )
    .await;
    assert!(matches!(hand_over, Attempt::Typed { .. }));

    // The pane still reads idle, so the hand-over's hold is on for the other writers.
    let timed = type_when_ready(&fake, Writer::TimedPrompt, CHIEF, "prompt", &agents, &[]).await;
    assert!(matches!(timed, Attempt::Wait(Reason::AgentBusy)));

    // While it reads busy they wait anyway.
    fake.set(|state| state.view = busy_view());
    let alert = type_when_ready(
        &fake,
        Writer::Alert,
        CHIEF,
        "alert line",
        &fake.agents(),
        &[],
    )
    .await;
    assert!(matches!(alert, Attempt::Wait(Reason::AgentBusy)));
    assert_eq!(fake.keys().len(), 4);
}

#[tokio::test]
async fn a_spoken_prompt_and_an_alert_released_at_once_give_two_whole_lines() {
    let fake = Fake::at(DAY, "09:00:00");
    let agents = fake.agents();

    let (voice, alert) = tokio::join!(
        type_when_ready(&fake, Writer::Voice, CHIEF, "🎙 hi", &agents, &[]),
        type_when_ready(&fake, Writer::Alert, CHIEF, "alert line", &agents, &[]),
    );
    if let Attempt::Wait(_) = alert {
        // The pane shows busy once, which ends the voice's hold, then idle again.
        fake.set(|state| {
            state.view = view(vec![AgentPane {
                state: EffectiveAgentState::Busy,
                ..chief("%2")
            }])
        });
        let _ = type_when_ready(
            &fake,
            Writer::Alert,
            CHIEF,
            "alert line",
            &fake.agents(),
            &[],
        )
        .await;
        fake.set(|state| state.view = view(vec![chief("%2")]));
        let retried = type_when_ready(
            &fake,
            Writer::Alert,
            CHIEF,
            "alert line",
            &fake.agents(),
            &[],
        )
        .await;
        assert!(matches!(retried, Attempt::Typed { .. }));
    }
    assert!(matches!(voice, Attempt::Typed { .. }));

    let voice_lines = [
        Key::Text("%2".into(), "🎙 hi".into()),
        Key::Enter("%2".into()),
    ];
    let alert_lines = [
        Key::Text("%2".into(), "alert line".into()),
        Key::Enter("%2".into()),
    ];
    let keys = fake.keys();
    assert!(
        keys == [voice_lines.clone(), alert_lines.clone()].concat()
            || keys == [alert_lines, voice_lines].concat(),
        "keys were interleaved: {keys:?}"
    );
}

// ---------------------------------------------------------------------------
// A tool's line
// ---------------------------------------------------------------------------

fn busy_chief(pane_id: &str) -> AgentPane {
    AgentPane {
        state: EffectiveAgentState::Busy,
        ..chief(pane_id)
    }
}

#[tokio::test]
async fn type_line_once_types_into_an_idle_pane() {
    let fake = Fake::at(DAY, "09:00:00");

    let outcome = type_line_once(&fake, Writer::Tool("questions"), "%2", "Q25 revise: no").await;

    assert_eq!(outcome, LineOutcome::Typed);
    assert_eq!(
        fake.keys(),
        vec![
            Key::Text("%2".into(), "Q25 revise: no".into()),
            Key::Enter("%2".into()),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn type_line_once_at_a_busy_pane_is_pane_busy_at_once() {
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view = view(vec![busy_chief("%2")]));
    let started = Instant::now();

    let outcome = type_line_once(&fake, Writer::Tool("questions"), "%2", "tool line").await;

    assert_eq!(outcome, LineOutcome::PaneBusy);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(fake.keys().is_empty());
}

#[tokio::test(start_paused = true)]
async fn type_line_once_into_a_box_holding_text_is_pane_busy_at_once() {
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.screen = Some(screen_with_box(&["❯ half a sentence"])));
    let started = Instant::now();

    let outcome = type_line_once(&fake, Writer::Tool("questions"), "%2", "tool line").await;

    assert_eq!(outcome, LineOutcome::PaneBusy);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(fake.keys().is_empty());
}

#[tokio::test]
async fn type_line_once_while_the_listing_failed_is_pane_busy() {
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.view.listing_failed = true);

    let outcome = type_line_once(&fake, Writer::Tool("questions"), "%2", "tool line").await;

    assert_eq!(outcome, LineOutcome::PaneBusy);
    assert!(fake.keys().is_empty());
}

#[tokio::test]
async fn type_line_once_without_the_pane_is_no_pane() {
    let fake = Fake::at(DAY, "09:00:00");

    let outcome = type_line_once(&fake, Writer::Tool("questions"), "%9", "tool line").await;

    assert_eq!(outcome, LineOutcome::NoPane);
    assert!(fake.keys().is_empty());
}

#[tokio::test]
async fn type_line_once_whose_enter_fails_is_failed() {
    let fake = Fake::at(DAY, "09:00:00");
    fake.set(|state| state.fail_enter = true);

    assert_eq!(
        type_line_once(&fake, Writer::Tool("questions"), "%2", "tool line").await,
        LineOutcome::Failed
    );
    assert_eq!(
        fake.keys(),
        vec![Key::Text("%2".into(), "tool line".into())]
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_line_and_a_voice_and_an_alert_give_whole_lines() {
    let fake = Fake::at(DAY, "09:00:00");
    let agents = fake.agents();

    let (dash, voice, alert) = tokio::join!(
        type_line_once(&fake, Writer::Tool("questions"), "%2", "dash line"),
        type_when_ready(&fake, Writer::Voice, CHIEF, "🎙 hi", &agents, &[]),
        type_when_ready(&fake, Writer::Alert, CHIEF, "alert line", &agents, &[]),
    );
    if let Attempt::Wait(_) = alert {
        // The pane shows busy once, which ends the other writers' holds, then idle again.
        fake.set(|state| state.view = view(vec![busy_chief("%2")]));
        let _ = type_when_ready(
            &fake,
            Writer::Alert,
            CHIEF,
            "alert line",
            &fake.agents(),
            &[],
        )
        .await;
        fake.set(|state| state.view = view(vec![chief("%2")]));
        let retried = type_when_ready(
            &fake,
            Writer::Alert,
            CHIEF,
            "alert line",
            &fake.agents(),
            &[],
        )
        .await;
        assert!(matches!(retried, Attempt::Typed { .. }));
    }
    assert_eq!(dash, LineOutcome::Typed);
    assert!(matches!(voice, Attempt::Typed { .. }));

    let keys = fake.keys();
    let mut texts = 0;
    for (index, key) in keys.iter().enumerate() {
        if let Key::Text(pane, _) = key {
            texts += 1;
            assert_eq!(
                keys.get(index + 1),
                Some(&Key::Enter(pane.clone())),
                "keys were interleaved: {keys:?}"
            );
        }
    }
    assert_eq!(texts, 3, "{keys:?}");
    let dash_lines = keys
        .iter()
        .filter(|key| **key == Key::Text("%2".into(), "dash line".into()))
        .count();
    assert_eq!(dash_lines, 1, "{keys:?}");
}

#[tokio::test]
async fn a_tool_is_held_by_another_writers_typing() {
    let fake = Fake::at(DAY, "09:00:00");
    let agents = fake.agents();

    let alert = type_when_ready(&fake, Writer::Alert, CHIEF, "alert line", &agents, &[]).await;
    assert!(matches!(alert, Attempt::Typed { .. }));
    // The pane still reads idle, so the alert's hold is on.
    let dash = type_when_ready(
        &fake,
        Writer::Tool("questions"),
        CHIEF,
        "dash line",
        &agents,
        &[],
    )
    .await;

    assert!(matches!(dash, Attempt::Wait(Reason::AgentBusy)));
}

#[tokio::test]
async fn two_tools_hold_the_pane_against_each_other() {
    let fake = Fake::at(DAY, "09:00:00");
    let agents = fake.agents();

    let first = type_when_ready(
        &fake,
        Writer::Tool("questions"),
        CHIEF,
        "first line",
        &agents,
        &[],
    )
    .await;
    assert!(matches!(first, Attempt::Typed { .. }));
    // The pane still reads idle, so the first tool's hold is on.
    let second = type_when_ready(
        &fake,
        Writer::Tool("backlog"),
        CHIEF,
        "second line",
        &agents,
        &[],
    )
    .await;

    assert!(matches!(second, Attempt::Wait(Reason::AgentBusy)));
}

#[tokio::test]
async fn a_panicking_typing_step_is_an_error_not_a_panic() {
    let result = typing_step(|| panic!("tmux step blew up")).await;

    println!("evidence: {result:?}");
    assert!(result.is_err());
}

#[tokio::test]
async fn two_agents_in_the_chiefs_folder_report_once_and_type_into_the_first_pane() {
    let fake = Fake::at(DAY, "09:00:00").with_chief(CHIEF, None);
    let two = view(vec![
        AgentPane {
            pane_index: 1,
            ..chief("%3")
        },
        AgentPane {
            pane_index: 0,
            ..chief("%2")
        },
    ]);
    fake.set(|state| state.view = two.clone());
    let logs = crate::outbound::fake::CapturedLogs::start();

    let alert = type_when_ready(&fake, Writer::Alert, CHIEF, "a", &two, &[]).await;
    let again = type_when_ready(&fake, Writer::Alert, CHIEF, "b", &two, &["%2".into()]).await;

    assert!(matches!(alert, Attempt::Typed { ref pane } if pane == "%2"));
    assert!(matches!(again, Attempt::Wait(Reason::AgentBusy)));
    let text = logs.text();
    assert_eq!(text.matches("chief pane changed").count(), 1, "{text}");
    assert!(
        text.contains("state=\"found\"")
            && text.contains("count=2")
            && text.contains("session_by=\"longest\""),
        "{text}"
    );
}

#[tokio::test]
async fn another_folder_with_two_agents_still_waits_and_a_second_chief_pane_is_not_typed_into() {
    let fake = Fake::at(DAY, "09:00:00").with_chief(CHIEF, None);
    let other = |pane_id: &str| AgentPane {
        folder: "/work/harold".into(),
        ..chief(pane_id)
    };
    let panes = view(vec![
        chief("%2"),
        AgentPane {
            pane_index: 2,
            ..chief("%3")
        },
        other("%5"),
        other("%6"),
    ]);
    fake.set(|state| state.view = panes.clone());

    let waits = type_when_ready(&fake, Writer::TimedPrompt, "/work/harold", "p", &panes, &[]).await;
    assert!(matches!(
        waits,
        Attempt::Wait(Reason::SeveralAgentsInFolder { agents: 2 })
    ));
    assert_eq!(
        type_line_once(&fake, Writer::Tool("t"), "%3", "line").await,
        LineOutcome::PaneBusy
    );
    assert!(fake.keys().is_empty());
    assert_eq!(
        type_line_once(&fake, Writer::Tool("t"), "%2", "line").await,
        LineOutcome::Typed
    );
}

#[tokio::test]
async fn a_home_relative_chief_entry_still_finds_the_first_pane_with_two_agents() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let fake = Fake::at(DAY, "09:00:00").with_chief("~/Dev/chief-of-pods", None);
    let panes = view(vec![
        AgentPane {
            folder: format!("{home}/Dev/chief-of-pods"),
            ..chief("%2")
        },
        AgentPane {
            folder: format!("{home}/Dev/chief-of-pods"),
            pane_index: 2,
            ..chief("%3")
        },
    ]);
    fake.set(|state| state.view = panes.clone());

    let alert = type_when_ready(
        &fake,
        Writer::Alert,
        "~/Dev/chief-of-pods",
        "a",
        &panes,
        &[],
    )
    .await;

    assert!(matches!(alert, Attempt::Typed { ref pane } if pane == "%2"));
}
