//! The decisions behind the two rules that keep a message off the away device. Rule A: the
//! pane sent a message with `harold send-message-to-away-device` this turn, so its
//! turn-end update is left out. Rule B: `[notify] away_messages_only_from` names a folder
//! with an agent running in it, and the pane is outside that folder, so its turn-end
//! update is left out and its requested message is dropped. The quiet rule: while an
//! agent runs in that folder, the turn-end updates of the chief's panes, build workers and
//! pod sessions are not spoken at the desk.

use std::path::Path;
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
    /// failed: the snapshot then holds panes Harold saw earlier, and rule B acts only for
    /// an agent Harold can see now.
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

    fn folder_of(&self, pane_id: &str) -> Option<&str> {
        self.folder_by_pane
            .iter()
            .find(|(id, _)| id == pane_id)
            .map(|(_, folder)| folder.as_str())
    }
}

/// Rule B's condition. The folder, as written in the setting, when all three hold:
/// `only_from` is set, a tracked agent pane is in it, and `pane_id` is not in it.
fn folder_pane_is_outside<'a>(
    only_from: Option<&'a str>,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<&'a str> {
    let folder = only_from?;
    if !agents.any_in(folder) || agents.pane_is_in(pane_id, folder) {
        return None;
    }
    Some(folder)
}

/// Rule B for a turn-end update. The log line for leaving out the update of `pane_id`.
pub(crate) fn only_from_reason(
    only_from: Option<&str>,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<String> {
    let folder = folder_pane_is_outside(only_from, agents, pane_id)?;
    Some(format!(
        "notification skipped (updates come from {folder} only)"
    ))
}

/// Rule B for a message a pane asked Harold to send. The log line for dropping the
/// message of `pane_id`. The line never holds the message's text.
pub(crate) fn message_dropped_reason(
    only_from: Option<&str>,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<String> {
    let folder = folder_pane_is_outside(only_from, agents, pane_id)?;
    Some(format!(
        "away message dropped (messages come from {folder} only)"
    ))
}

/// Rule B for a line a pane asked Harold to speak. The log line for dropping the line of
/// `pane_id`. The line never holds the spoken text.
pub(crate) fn spoken_line_dropped_reason(
    only_from: Option<&str>,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<String> {
    let folder = folder_pane_is_outside(only_from, agents, pane_id)?;
    Some(format!(
        "spoken line dropped (lines come from {folder} only)"
    ))
}

/// The file in a build worker's folder that names its pane, written by baton.
const BATON_AGENT_PANE: &str = ".superpowers/baton/agent-pane";
/// The file in a product's base checkout, where pod sessions run, that lists their panes,
/// written by `pod start`.
const POD_AGENT_PANES: &str = ".superpowers/pod/agent-panes";

/// Whether the file at `path` has a line that is `pane_id` once trimmed. A missing or
/// unreadable file names no pane.
fn names_pane(path: &Path, pane_id: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.lines().any(|line| line.trim() == pane_id))
}

/// The quiet rule. The log line for not speaking the turn-end update of `pane_id` at the
/// desk: while an agent runs in `only_from`, a pane in that folder is the chief's, a pane
/// whose folder's baton file names it is a build worker's, and a pane listed in its own
/// folder's `agent-panes` is a pod session's (pod sessions run in the base checkout). Any
/// other pane, and every pane while no agent runs in the folder, is spoken.
pub(crate) fn quiet_reason(
    only_from: Option<&str>,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<String> {
    let folder = only_from?;
    if !agents.any_in(folder) {
        return None;
    }
    let role = if agents.pane_is_in(pane_id, folder) {
        "chief"
    } else {
        let pane_folder = Path::new(agents.folder_of(pane_id)?);
        if names_pane(&pane_folder.join(BATON_AGENT_PANE), pane_id) {
            "build worker"
        } else if names_pane(&pane_folder.join(POD_AGENT_PANES), pane_id) {
            "pod session"
        } else {
            return None;
        }
    };
    Some(format!("spoken update left out ({role})"))
}

/// The log line for leaving out the turn-end update of `pane_id`, when either rule
/// applies. Rule A is looked at first, so its line is the one written when both apply.
pub(crate) fn left_out_reason(
    messaged_this_turn: bool,
    only_from: Option<&str>,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<String> {
    if messaged_this_turn {
        return Some(MESSAGED_THIS_TURN.to_string());
    }
    only_from_reason(only_from, agents, pane_id)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        MARK_LIFETIME, MESSAGED_THIS_TURN, TrackedAgents, left_out_reason, mark_is_fresh,
        message_dropped_reason, names_pane, only_from_reason, same_folder,
        spoken_line_dropped_reason,
    };

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

    #[test]
    fn rule_a_line_is_the_one_written_when_both_rules_apply() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold")]);

        // Pane %2 is outside the update folder, so rule B applies to it as well.
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%2"), reason());
        assert_eq!(
            left_out_reason(true, Some(CHIEF), &agents, "%2"),
            Some(MESSAGED_THIS_TURN.to_string())
        );
        assert_eq!(left_out_reason(false, Some(CHIEF), &agents, "%2"), reason());
        assert_eq!(left_out_reason(false, Some(CHIEF), &agents, "%1"), None);
    }

    fn dropped() -> Option<String> {
        Some("away message dropped (messages come from /work/chief-of-pods only)".to_string())
    }

    #[test]
    fn message_rule_does_nothing_without_the_setting() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold")]);
        assert_eq!(message_dropped_reason(None, &agents, "%2"), None);
    }

    #[test]
    fn message_from_outside_the_folder_is_dropped_while_an_agent_is_in_it() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold")]);
        assert_eq!(
            message_dropped_reason(Some(CHIEF), &agents, "%2"),
            dropped()
        );
    }

    #[test]
    fn messages_from_panes_in_the_folder_are_never_dropped() {
        let agents =
            TrackedAgents::in_folders(&[("%1", CHIEF), ("%3", CHIEF), ("%2", "/work/harold")]);
        assert_eq!(message_dropped_reason(Some(CHIEF), &agents, "%1"), None);
        assert_eq!(message_dropped_reason(Some(CHIEF), &agents, "%3"), None);
    }

    #[test]
    fn no_message_is_dropped_while_no_agent_is_in_the_folder() {
        let agents = TrackedAgents::in_folders(&[("%2", "/work/harold")]);
        assert_eq!(message_dropped_reason(Some(CHIEF), &agents, "%2"), None);
        assert_eq!(
            message_dropped_reason(Some(CHIEF), &TrackedAgents::default(), "%2"),
            None
        );
    }

    #[test]
    fn message_from_a_subfolder_is_dropped() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/chief-of-pods/sub")]);
        assert_eq!(
            message_dropped_reason(Some(CHIEF), &agents, "%2"),
            dropped()
        );
    }

    #[test]
    fn message_from_an_untracked_pane_is_dropped_while_an_agent_is_in_the_folder() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF)]);
        assert_eq!(
            message_dropped_reason(Some(CHIEF), &agents, "%9"),
            dropped()
        );
    }

    #[test]
    fn message_rule_matches_a_setting_with_a_trailing_slash_and_logs_it_as_written() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold")]);
        let setting = "/work/chief-of-pods/";

        assert_eq!(message_dropped_reason(Some(setting), &agents, "%1"), None);
        assert_eq!(
            message_dropped_reason(Some(setting), &agents, "%2"),
            Some("away message dropped (messages come from /work/chief-of-pods/ only)".to_string())
        );
    }

    #[test]
    fn spoken_line_from_outside_the_folder_is_dropped_while_an_agent_is_in_it() {
        let agents = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold")]);
        assert_eq!(
            spoken_line_dropped_reason(Some(CHIEF), &agents, "%2"),
            Some("spoken line dropped (lines come from /work/chief-of-pods only)".to_string())
        );
        assert_eq!(spoken_line_dropped_reason(Some(CHIEF), &agents, "%1"), None);
        assert_eq!(spoken_line_dropped_reason(None, &agents, "%2"), None);
        let no_chief = TrackedAgents::in_folders(&[("%2", "/work/harold")]);
        assert_eq!(
            spoken_line_dropped_reason(Some(CHIEF), &no_chief, "%2"),
            None
        );
    }

    #[test]
    fn a_pane_file_names_a_pane_on_any_line_ignoring_whitespace() {
        let path = std::env::temp_dir().join(format!("harold-panes-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, "%2610\n  %7  \n").unwrap();
        assert!(names_pane(&path, "%2610"));
        assert!(names_pane(&path, "%7"));
        assert!(!names_pane(&path, "%26"));
        assert!(!names_pane(std::path::Path::new("/no/such/file"), "%1"));
        let _ = std::fs::remove_file(path);
    }
}
