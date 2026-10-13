use events::EventStreamVersion;

use super::*;
use crate::agent::domain::{
    AgentIncarnation, AgentPaneObservation, AgentPaneProjection, AgentSnapshot,
    EffectiveAgentState, MonitorHealthProjection,
};
use crate::settings::{AgentProviderSettings, ScreenAdapter};

const CHIEF: &str = "/work/chief-of-pods";

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

    assert_eq!(pick_pane(&agents, CHIEF, &[]).unwrap().pane_id, "%2");
    // One trailing slash on either side names the same folder.
    assert_eq!(
        pick_pane(&agents, "/work/chief-of-pods/", &[])
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
        pick_pane(&failing, CHIEF, &[]),
        Err(Reason::PaneListFailing)
    );
    assert_eq!(
        pick_pane(&view(vec![]), CHIEF, &[]),
        Err(Reason::NoAgentInFolder)
    );
    assert_eq!(
        pick_pane(&view(vec![chief("%5")]), "/work/harold", &[]),
        Err(Reason::NoAgentInFolder)
    );
    assert_eq!(
        pick_pane(
            &view(vec![chief("%2"), chief("%3"), codex.clone()]),
            CHIEF,
            &[]
        ),
        Err(Reason::SeveralAgentsInFolder { agents: 3 })
    );
    // A busy Codex pane fails check 4 before check 5.
    assert_eq!(
        pick_pane(&view(vec![codex]), CHIEF, &[]),
        Err(Reason::InputBoxUnreadable)
    );
    assert_eq!(
        pick_pane(&view(vec![busy]), CHIEF, &[]),
        Err(Reason::AgentBusy)
    );
    assert_eq!(
        pick_pane(&view(vec![unknown]), CHIEF, &[]),
        Err(Reason::AgentStateUnknown)
    );
}

#[test]
fn pane_typed_into_this_round_counts_as_busy() {
    let agents = view(vec![chief("%2")]);

    assert_eq!(
        pick_pane(&agents, CHIEF, &["%2".to_string()]),
        Err(Reason::AgentBusy)
    );
    assert!(pick_pane(&agents, CHIEF, &["%9".to_string()]).is_ok());
}

fn projection(
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
        summary_basis_version: EventStreamVersion::start(),
        generated_work_summary: None,
        generated_summary_basis_version: None,
        work_summary: None,
        last_transition_at_ms: 2_000,
        last_event_version: EventStreamVersion::start(),
    }
}

fn provider(id: &str, screen_adapter: ScreenAdapter) -> AgentProviderSettings {
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

fn snapshot(healthy: Option<bool>, panes: Vec<AgentPaneProjection>) -> AgentSnapshot {
    AgentSnapshot {
        through_event_version: EventStreamVersion::start(),
        server_time_ms: 3_000,
        monitor_health: healthy
            .map(|healthy| MonitorHealthProjection {
                component: "inventory".into(),
                healthy,
                reason_code: (if healthy { "ok" } else { "timeout" }).into(),
                observed_at_ms: 2_500,
                last_event_version: EventStreamVersion::start(),
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
