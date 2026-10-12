//! The decisions behind the two rules that leave a turn-end update out of the away
//! device. Rule A: the pane sent a message with `harold send-message-to-away-device`
//! this turn. Rule B: `[notify] away_updates_only_from` names a folder with an agent
//! running in it, and the pane is outside that folder.

use std::time::{Duration, Instant};

use crate::agent::domain::AgentSnapshot;

/// How long after a pane's requested message its mark still leaves a turn-end update
/// out. Harold is not told about every turn end, so without a limit a mark could hide the
/// update of a later, unrelated turn.
pub(crate) const MARK_LIFETIME: Duration = Duration::from_secs(600);

pub(crate) const MESSAGED_THIS_TURN: &str =
    "notification skipped (pane messaged the phone this turn)";

/// Whether a mark set at `marked_at` still counts at `now`.
pub(crate) fn mark_is_fresh(marked_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(marked_at) < MARK_LIFETIME
}

/// The monitor health component that reports whether the last listing of tmux panes
/// worked (`set_health("inventory", ..)` in `agent/runtime.rs`).
const INVENTORY_COMPONENT: &str = "inventory";

fn without_one_trailing_slash(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

/// Whether two paths name the same folder: equal as text once one trailing `/` is taken
/// off each. Symbolic links are not resolved, and a subfolder is another folder.
pub(crate) fn same_folder(a: &str, b: &str) -> bool {
    without_one_trailing_slash(a) == without_one_trailing_slash(b)
}

/// The agent panes Harold tracks and the working directory tmux reports for each.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TrackedAgents {
    /// Pane id and working directory.
    folder_by_pane: Vec<(String, String)>,
}

impl TrackedAgents {
    /// The panes in `snapshot`. Empty while the monitor's last listing of tmux panes
    /// failed: the snapshot then holds panes Harold saw earlier, and rule B leaves an
    /// update out only for an agent Harold can see now.
    pub(crate) fn from_snapshot(snapshot: &AgentSnapshot) -> Self {
        let listing_failed = snapshot
            .monitor_health
            .iter()
            .any(|health| health.component == INVENTORY_COMPONENT && !health.healthy);
        if listing_failed {
            return Self::default();
        }
        Self {
            folder_by_pane: snapshot
                .panes
                .iter()
                .map(|projection| {
                    (
                        projection.pane.incarnation.pane_id.clone(),
                        projection.pane.working_directory.clone(),
                    )
                })
                .collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn in_folders(panes: &[(&str, &str)]) -> Self {
        Self {
            folder_by_pane: panes
                .iter()
                .map(|(pane_id, folder)| (pane_id.to_string(), folder.to_string()))
                .collect(),
        }
    }

    fn any_in(&self, folder: &str) -> bool {
        self.folder_by_pane
            .iter()
            .any(|(_, pane_folder)| same_folder(pane_folder, folder))
    }

    fn pane_is_in(&self, pane_id: &str, folder: &str) -> bool {
        self.folder_by_pane
            .iter()
            .any(|(id, pane_folder)| id == pane_id && same_folder(pane_folder, folder))
    }
}

/// Rule B. The log line for leaving out the update of `pane_id`, when all three hold:
/// `update_folder` is set, a tracked agent pane is in it, and `pane_id` is not in it.
pub(crate) fn only_from_reason(
    update_folder: Option<&str>,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<String> {
    let folder = update_folder?;
    if !agents.any_in(folder) || agents.pane_is_in(pane_id, folder) {
        return None;
    }
    Some(format!(
        "notification skipped (updates come from {folder} only)"
    ))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{MARK_LIFETIME, TrackedAgents, mark_is_fresh, only_from_reason, same_folder};

    #[test]
    fn mark_counts_until_it_is_10_minutes_old() {
        let marked_at = Instant::now();

        assert_eq!(MARK_LIFETIME, Duration::from_secs(600));
        assert!(mark_is_fresh(marked_at, marked_at));
        assert!(mark_is_fresh(
            marked_at,
            marked_at + Duration::from_secs(599)
        ));
        assert!(!mark_is_fresh(
            marked_at,
            marked_at + Duration::from_secs(600)
        ));
        assert!(!mark_is_fresh(
            marked_at,
            marked_at + Duration::from_secs(3_600)
        ));
    }

    const CHIEF: &str = "/work/chief-of-pods";

    #[test]
    fn same_folder_ignores_one_trailing_slash_on_either_side() {
        assert!(same_folder(CHIEF, CHIEF));
        assert!(same_folder("/work/chief-of-pods/", CHIEF));
        assert!(same_folder(CHIEF, "/work/chief-of-pods/"));
        assert!(same_folder("/", "/"));
        assert!(!same_folder("/work/chief-of-pods//", CHIEF));
    }

    #[test]
    fn same_folder_is_a_text_comparison_of_the_whole_path() {
        assert!(!same_folder("/work/chief-of-pods/sub", CHIEF));
        assert!(!same_folder("/work", CHIEF));
        assert!(!same_folder("/work/chief-of-pods-old", CHIEF));
        assert!(!same_folder("/Work/chief-of-pods", CHIEF));
        assert!(!same_folder("/work/harold", CHIEF));
    }

    fn reason() -> Option<String> {
        Some("notification skipped (updates come from /work/chief-of-pods only)".to_string())
    }

    #[test]
    fn rule_b_does_nothing_without_the_setting() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold")]);
        assert_eq!(only_from_reason(None, &agents, "%2"), None);
    }

    #[test]
    fn pane_outside_the_folder_is_left_out_while_an_agent_is_in_it() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold")]);
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%2"), reason());
    }

    #[test]
    fn panes_in_the_folder_are_never_left_out() {
        let agents =
            TrackedAgents::in_folders(&[("%1", CHIEF), ("%3", CHIEF), ("%2", "/work/harold")]);
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%1"), None);
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%3"), None);
    }

    #[test]
    fn no_pane_is_left_out_while_no_agent_is_in_the_folder() {
        let agents = TrackedAgents::in_folders(&[("%2", "/work/harold")]);
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%2"), None);
        assert_eq!(
            only_from_reason(Some(CHIEF), &TrackedAgents::default(), "%2"),
            None
        );
    }

    #[test]
    fn pane_in_a_subfolder_is_outside_the_folder() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/chief-of-pods/sub")]);
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%2"), reason());
    }

    #[test]
    fn untracked_pane_is_outside_the_folder() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF)]);
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%9"), reason());
    }

    #[test]
    fn setting_with_a_trailing_slash_matches_the_same_panes_and_is_logged_as_written() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold")]);
        let setting = "/work/chief-of-pods/";

        assert_eq!(only_from_reason(Some(setting), &agents, "%1"), None);
        assert_eq!(
            only_from_reason(Some(setting), &agents, "%2"),
            Some("notification skipped (updates come from /work/chief-of-pods/ only)".to_string())
        );
    }
}
