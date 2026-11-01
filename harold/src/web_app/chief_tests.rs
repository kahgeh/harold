use super::{ChiefPane, find_chief, is_chief_pane};
use crate::agent::domain::{
    AgentIncarnation, AgentPaneObservation, AgentPaneProjection, AgentSnapshot, EffectiveAgentState,
};
use crate::store::EventVersion;
use crate::timed_prompts::tests::snapshot;

const CHIEF: &str = "/a/chief";

fn pane_in(pane_id: &str, folder: &str, state: EffectiveAgentState) -> AgentPaneProjection {
    AgentPaneProjection {
        pane: AgentPaneObservation {
            incarnation: AgentIncarnation {
                pane_id: pane_id.into(),
                pane_pid: 10,
                agent_pid: 20,
                agent_started_at_ms: 1_000,
                provider_id: "claude".into(),
            },
            tmux_target: "pods:0.1".into(),
            session_name: "pods".into(),
            window_index: 0,
            pane_index: 1,
            working_directory: folder.into(),
            provider_display_name: "Claude".into(),
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

/// A healthy listing with `%1` in the chief's folder and `%2` in another.
fn chief_and_pod(chief_state: EffectiveAgentState) -> AgentSnapshot {
    snapshot(
        Some(true),
        vec![
            pane_in("%1", CHIEF, chief_state),
            pane_in("%2", "/a/harold", EffectiveAgentState::Working),
        ],
    )
}

#[test]
fn find_chief_names_the_one_agent_in_the_folder_and_whether_it_is_busy() {
    for (state, busy) in [
        (EffectiveAgentState::Idle, false),
        (EffectiveAgentState::Working, true),
        (EffectiveAgentState::Unknown, false),
    ] {
        assert_eq!(
            find_chief(&chief_and_pod(state), Some(CHIEF), None),
            ChiefPane::One {
                pane_id: "%1".into(),
                busy,
            },
            "{state:?}"
        );
    }
}

#[test]
fn find_chief_reports_none_and_a_failing_listing_and_picks_the_first_of_several() {
    let only_a_pod = snapshot(
        Some(true),
        vec![pane_in("%2", "/a/harold", EffectiveAgentState::Idle)],
    );
    assert_eq!(
        find_chief(&only_a_pod, Some(CHIEF), None),
        ChiefPane::NoChief
    );
    assert_eq!(
        find_chief(&chief_and_pod(EffectiveAgentState::Idle), None, None),
        ChiefPane::NoChief
    );

    let two_chiefs = snapshot(
        Some(true),
        vec![
            pane_in("%1", CHIEF, EffectiveAgentState::Idle),
            pane_in("%3", "/a/chief/", EffectiveAgentState::Idle),
        ],
    );
    assert_eq!(
        find_chief(&two_chiefs, Some(CHIEF), None),
        ChiefPane::One {
            pane_id: "%1".into(),
            busy: false,
        }
    );

    let listing_failed = snapshot(
        Some(false),
        vec![pane_in("%1", CHIEF, EffectiveAgentState::Idle)],
    );
    assert_eq!(
        find_chief(&listing_failed, Some(CHIEF), None),
        ChiefPane::ListingFailed
    );

    // No inventory row yet: the listing has not failed.
    let not_yet_reported = snapshot(None, vec![pane_in("%1", CHIEF, EffectiveAgentState::Idle)]);
    assert_eq!(
        find_chief(&not_yet_reported, Some(CHIEF), None),
        ChiefPane::One {
            pane_id: "%1".into(),
            busy: false,
        }
    );
}

#[test]
fn trailing_slash_and_other_folders() {
    let chief = ChiefPane::One {
        pane_id: "%1".into(),
        busy: false,
    };
    let with_slash = snapshot(
        Some(true),
        vec![pane_in("%1", "/a/chief/", EffectiveAgentState::Idle)],
    );
    assert_eq!(find_chief(&with_slash, Some("/a/chief"), None), chief);
    assert!(is_chief_pane(&with_slash, Some("/a/chief"), None, "%1"));

    let without_slash = snapshot(
        Some(true),
        vec![pane_in("%1", "/a/chief", EffectiveAgentState::Idle)],
    );
    assert_eq!(find_chief(&without_slash, Some("/a/chief/"), None), chief);
    assert!(is_chief_pane(&without_slash, Some("/a/chief/"), None, "%1"));

    let subfolder = snapshot(
        Some(true),
        vec![pane_in("%4", "/a/chief/sub", EffectiveAgentState::Idle)],
    );
    assert_eq!(
        find_chief(&subfolder, Some("/a/chief"), None),
        ChiefPane::NoChief
    );
    assert!(!is_chief_pane(&subfolder, Some("/a/chief"), None, "%4"));
}

#[test]
fn is_chief_pane_is_false_while_the_listing_fails_or_without_a_folder() {
    let healthy = chief_and_pod(EffectiveAgentState::Idle);
    assert!(is_chief_pane(&healthy, Some(CHIEF), None, "%1"));
    assert!(!is_chief_pane(&healthy, Some(CHIEF), None, "%2"));
    assert!(!is_chief_pane(&healthy, Some(CHIEF), None, "%9"));
    assert!(!is_chief_pane(&healthy, None, None, "%1"));

    let listing_failed = snapshot(
        Some(false),
        vec![pane_in("%1", CHIEF, EffectiveAgentState::Idle)],
    );
    assert!(!is_chief_pane(&listing_failed, Some(CHIEF), None, "%1"));

    // With two panes in the chief's folder the first is the chief.
    let two_chiefs = snapshot(
        Some(true),
        vec![
            pane_in("%1", CHIEF, EffectiveAgentState::Idle),
            pane_in("%3", CHIEF, EffectiveAgentState::Working),
        ],
    );
    assert!(is_chief_pane(&two_chiefs, Some(CHIEF), None, "%1"));
    assert!(!is_chief_pane(&two_chiefs, Some(CHIEF), None, "%3"));
}

#[test]
fn a_partial_chief_entry_finds_the_chief_and_two_matches_give_the_first() {
    let one = snapshot(
        Some(true),
        vec![pane_in(
            "%1",
            "/Users/x/code/chief-of-pods",
            EffectiveAgentState::Idle,
        )],
    );
    assert!(is_chief_pane(&one, Some("chief-of-pods"), None, "%1"));
    assert!(!is_chief_pane(&one, Some("chief-of-pods-old"), None, "%1"));

    let two = snapshot(
        Some(true),
        vec![
            pane_in(
                "%1",
                "/Users/x/code/chief-of-pods",
                EffectiveAgentState::Idle,
            ),
            pane_in("%2", "/tmp/chief-of-pods", EffectiveAgentState::Idle),
        ],
    );
    assert_eq!(
        find_chief(&two, Some("chief-of-pods"), None),
        ChiefPane::One {
            pane_id: "%1".into(),
            busy: false,
        }
    );
}

#[test]
fn find_chief_logs_the_pane_once_per_change() {
    let logs = crate::outbound::fake::CapturedLogs::start();
    let one = snapshot(
        Some(true),
        vec![pane_in("%1", CHIEF, EffectiveAgentState::Idle)],
    );
    let two = snapshot(
        Some(true),
        vec![
            pane_in("%1", CHIEF, EffectiveAgentState::Idle),
            pane_in("%3", "/b/chief", EffectiveAgentState::Idle),
        ],
    );
    let second_only = snapshot(
        Some(true),
        vec![pane_in("%3", "/b/chief", EffectiveAgentState::Idle)],
    );
    // `chief` is a partial entry here, so both panes in `two` match it. The first stays
    // the chief until it goes, and the log notes each change of pane once.
    for snapshot in [&two, &two, &one, &second_only, &second_only, &two] {
        find_chief(snapshot, Some("chief"), None);
    }
    let text = logs.text();
    assert_eq!(text.matches("chief pane changed").count(), 3, "{text}");
    assert!(text.contains("state=\"found\""), "{text}");
    assert!(!text.contains("state=\"ambiguous\""), "{text}");
}

/// The phone app reads panes only to find the chief: a pane id and a busy flag. The result
/// has no step field. This fails if one is added.
#[test]
fn the_phone_apps_chief_result_has_no_current_step_field() {
    // The chief works and reports a current step and a recap. Both live beside the snapshot.
    // The chief pane `%1` has the agent pid 20 in the fixture.
    let steps = crate::agent::rex::CurrentSteps::default();
    steps.set("%1", 20, Some("LEAKCHECK-step-7f3a".into()));
    let recaps = crate::agent::recap::Recaps::default();
    recaps.put_for_test("%1", 20, "LEAKCHECK-recap-9c1d", 3_000);
    let found = find_chief(
        &chief_and_pod(EffectiveAgentState::Working),
        Some(CHIEF),
        None,
    );
    // The destructure stops compiling when a field is added, so a step cannot enter without
    // this test.
    let ChiefPane::One { pane_id, busy: _ } = &found else {
        panic!("the chief is found: {found:?}");
    };
    assert!(!pane_id.contains("LEAKCHECK-step-7f3a"), "{found:?}");
    // The step is stored for this pane, so a leak would show its text.
    assert_eq!(steps.get("%1", 20).as_deref(), Some("LEAKCHECK-step-7f3a"));
    assert!(recaps.get("%1", 20).is_some());
    assert!(
        !format!("{found:?}").contains("LEAKCHECK-step-7f3a"),
        "{found:?}"
    );
}

/// The phone app reads panes only to find the chief. The result has no recap field.
#[test]
fn the_phone_apps_agent_rows_have_no_recap() {
    // This guards against a recap field being added, because this code cannot reach the store.
    // The chief pane `%1` has the agent pid 20 in the fixture.
    let steps = crate::agent::rex::CurrentSteps::default();
    steps.set("%1", 20, Some("LEAKCHECK-step-7f3a".into()));
    let recaps = crate::agent::recap::Recaps::default();
    recaps.put_for_test("%1", 20, "LEAKCHECK-recap-9c1d", 3_000);
    let found = find_chief(
        &chief_and_pod(EffectiveAgentState::Working),
        Some(CHIEF),
        None,
    );
    let text = format!("{found:?}");
    // The recap is stored for this pane, so a leak would show its text.
    assert_eq!(
        recaps.get("%1", 20).map(|line| line.text).as_deref(),
        Some("LEAKCHECK-recap-9c1d")
    );
    assert_eq!(steps.get("%1", 20).as_deref(), Some("LEAKCHECK-step-7f3a"));
    assert!(text.contains("%1"), "{text}");
    assert!(!text.contains("LEAKCHECK-recap-9c1d"), "{text}");
    assert!(!text.contains("LEAKCHECK-step-7f3a"), "{text}");
}
