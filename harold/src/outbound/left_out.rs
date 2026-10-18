//! The decisions behind the two rules that keep a message off the away device. Rule A: the
//! pane sent a message with `harold send-message-to-away-device` this turn, so its
//! turn-end update is left out. Rule B: `[notify] away_messages_only_from` names a folder
//! with an agent running in it, and the pane is outside that folder, so its turn-end
//! update is left out and its requested message is dropped. The quiet rule: while an
//! agent runs in that folder, the turn-end updates of the chief's panes, build workers and
//! pod sessions are not spoken at the desk.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

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
    /// The last listing of tmux panes failed, so `folder_by_pane` is the last good one's.
    listing_failed: bool,
    /// Where pod and baton files are looked for, and when the tmux server started.
    pod_files: PodFiles,
}

impl TrackedAgents {
    /// The panes in `snapshot`. While the monitor's last listing of tmux panes failed the
    /// snapshot holds panes Harold saw earlier: they are kept for folder lookups, but
    /// rule B's view of the chief ignores them, so it acts only for an agent Harold can
    /// see now (and for panes a pod file names).
    pub(crate) fn from_snapshot(snapshot: &AgentSnapshot) -> Self {
        let listing_failed = snapshot
            .monitor_health
            .iter()
            .any(|health| health.component == INVENTORY_COMPONENT && !health.healthy);
        Self {
            listing_failed,
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
            pod_files: PodFiles::default(),
        }
    }

    /// These agents with the places pod and baton files are looked for.
    #[allow(dead_code)] // Used by the environment plumbing in the next task.
    pub(crate) fn with_pod_files(self, pod_files: PodFiles) -> Self {
        Self { pod_files, ..self }
    }

    #[cfg(test)]
    pub(crate) fn in_folders(panes: &[(&str, &str)]) -> Self {
        Self {
            folder_by_pane: panes
                .iter()
                .map(|(pane_id, folder)| (pane_id.to_string(), folder.to_string()))
                .collect(),
            listing_failed: false,
            pod_files: PodFiles::default(),
        }
    }

    /// Rule B's and the quiet rule's chief view: whether an agent Harold can see now runs
    /// in `folder`. Nothing is seen while the listing has failed.
    fn any_in(&self, folder: &str) -> bool {
        !self.listing_failed
            && self
                .folder_by_pane
                .iter()
                .any(|(_, pane_folder)| same_folder(pane_folder, folder))
    }

    fn pane_is_in(&self, pane_id: &str, folder: &str) -> bool {
        !self.listing_failed
            && self
                .folder_by_pane
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
/// `only_from` is set, a tracked agent pane is in it, and `pane_id` is not in it. Also the
/// folder when `only_from` is set and a pod file names `pane_id`, whether or not the chief
/// runs and whether or not the listing worked.
fn folder_pane_is_outside<'a>(
    only_from: Option<&'a str>,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<&'a str> {
    let folder = only_from?;
    if pod_role(&agents.pod_files, agents.folder_of(pane_id), pane_id).is_some() {
        return Some(folder);
    }
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

/// What a pane's role in a pod is, as a file says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PodRole {
    /// Named in a worktree's baton `agent-pane`.
    BuildWorker,
    /// Listed in a base checkout's pod `agent-panes`.
    PodSession,
}

/// Where pod files can be, and when the tmux server started. tmux numbers panes from `%0`
/// again after a restart, so a file's entry is trusted only as far as these allow.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PodFiles {
    /// The base checkout of each product in the pod registry. Empty when the registry is
    /// unset or unreadable.
    bases: Vec<PathBuf>,
    /// When the tmux server started, if Harold could read it.
    tmux_started: Option<SystemTime>,
}

impl PodFiles {
    #[allow(dead_code)] // Used by the environment plumbing in the next task.
    pub(crate) fn new(bases: Vec<PathBuf>, tmux_started: Option<SystemTime>) -> Self {
        Self {
            bases,
            tmux_started,
        }
    }
}

/// Why a file's entry for a pane was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ignored {
    /// A tracked pane whose folder is no product's base checkout or worktree.
    OutsidePodFolders,
    /// An untracked pane named in a file last changed before the tmux server started.
    OlderThanTmuxServer,
    /// An untracked pane, and the tmux start time is unknown.
    NoTmuxStartTime,
}

impl Ignored {
    fn reason(self) -> &'static str {
        match self {
            Self::OutsidePodFolders => "outside_pod_folders",
            Self::OlderThanTmuxServer => "older_than_tmux_server",
            Self::NoTmuxStartTime => "no_tmux_start_time",
        }
    }
}

/// The role a file in `folder` gives `pane_id`: its baton file first, then `agent-panes`.
fn role_in_folder(folder: &Path, pane_id: &str) -> Option<PodRole> {
    if names_pane(&folder.join(BATON_AGENT_PANE), pane_id) {
        Some(PodRole::BuildWorker)
    } else if names_pane(&folder.join(POD_AGENT_PANES), pane_id) {
        Some(PodRole::PodSession)
    } else {
        None
    }
}

/// Whether `folder` is a base checkout, or a folder beside one (where baton puts worktrees).
fn is_pod_folder(files: &PodFiles, folder: &str) -> bool {
    let folder_parent = Path::new(without_one_trailing_slash(folder)).parent();
    files.bases.iter().any(|base| {
        let base = base.to_string_lossy();
        same_folder(&base, folder)
            || Path::new(without_one_trailing_slash(&base)).parent() == folder_parent
    })
}

/// Whether `file` was changed after the tmux server started, or why not.
fn file_is_current(files: &PodFiles, file: &Path) -> Result<(), Ignored> {
    let started = files.tmux_started.ok_or(Ignored::NoTmuxStartTime)?;
    let changed = std::fs::metadata(file).and_then(|meta| meta.modified());
    match changed {
        Ok(changed) if changed > started => Ok(()),
        _ => Err(Ignored::OlderThanTmuxServer),
    }
}

/// The files that can name a pane Harold has no folder for, with the role each gives:
/// each base checkout's `agent-panes`, and the baton file of each folder beside a base.
fn candidate_files(files: &PodFiles) -> Vec<(PathBuf, PodRole)> {
    let mut candidates: Vec<(PathBuf, PodRole)> = Vec::new();
    let mut add = |path: PathBuf, role: PodRole| {
        if !candidates.iter().any(|(known, _)| *known == path) {
            candidates.push((path, role));
        }
    };
    for base in &files.bases {
        add(base.join(POD_AGENT_PANES), PodRole::PodSession);
        let base = PathBuf::from(without_one_trailing_slash(&base.to_string_lossy()));
        let Some(parent) = base.parent() else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(parent) else {
            continue;
        };
        for entry in entries.flatten() {
            add(entry.path().join(BATON_AGENT_PANE), PodRole::BuildWorker);
        }
    }
    candidates
}

/// The decision behind `pod_role`, with the refusal as a value.
fn classify(
    files: &PodFiles,
    tracked_folder: Option<&str>,
    pane_id: &str,
) -> Result<Option<PodRole>, Ignored> {
    if let Some(folder) = tracked_folder {
        let Some(role) = role_in_folder(Path::new(folder), pane_id) else {
            return Ok(None);
        };
        if !files.bases.is_empty() && !is_pod_folder(files, folder) {
            return Err(Ignored::OutsidePodFolders);
        }
        return Ok(Some(role));
    }
    let mut refused = None;
    for (file, role) in candidate_files(files) {
        if !names_pane(&file, pane_id) {
            continue;
        }
        match file_is_current(files, &file) {
            Ok(()) => return Ok(Some(role)),
            Err(ignored) => refused = refused.or(Some(ignored)),
        }
    }
    refused.map_or(Ok(None), Err)
}

/// Whether a file names `pane_id` as a pod session or a build worker. `tracked_folder` is
/// the pane's own folder when Harold knows it. A refused entry is logged and counts for
/// nothing.
fn pod_role(files: &PodFiles, tracked_folder: Option<&str>, pane_id: &str) -> Option<PodRole> {
    match classify(files, tracked_folder, pane_id) {
        Ok(role) => role,
        Err(ignored) => {
            tracing::info!(pane_id, reason = ignored.reason(), "pod pane entry ignored");
            None
        }
    }
}

/// The quiet rule. The log line for not speaking the turn-end update of `pane_id` at the
/// desk: a pane a pod file names is a pod session's or a build worker's, whether or not an
/// agent runs in `only_from`; and while an agent runs there, a pane in that folder is the
/// chief's. Any other pane is spoken.
pub(crate) fn quiet_reason(
    only_from: Option<&str>,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<String> {
    let folder = only_from?;
    let role = match pod_role(&agents.pod_files, agents.folder_of(pane_id), pane_id) {
        Some(PodRole::BuildWorker) => "build worker",
        Some(PodRole::PodSession) => "pod session",
        None if agents.pane_is_in(pane_id, folder) => "chief",
        None => return None,
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
        BATON_AGENT_PANE, Ignored, MARK_LIFETIME, MESSAGED_THIS_TURN, POD_AGENT_PANES, PodFiles,
        PodRole, TrackedAgents, classify, left_out_reason, mark_is_fresh, message_dropped_reason,
        names_pane, only_from_reason, same_folder, spoken_line_dropped_reason,
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

    /// A folder under the system temp folder, removed when dropped.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("harold-pod-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn folder(&self, name: &str) -> std::path::PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_in(folder: &std::path::Path, file: &str, text: &str) {
        let path = folder.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn hour_ago() -> Option<std::time::SystemTime> {
        Some(std::time::SystemTime::now() - Duration::from_secs(3600))
    }

    fn hour_ahead() -> Option<std::time::SystemTime> {
        Some(std::time::SystemTime::now() + Duration::from_secs(3600))
    }

    fn path_text(path: &std::path::Path) -> &str {
        path.to_str().unwrap()
    }

    #[test]
    fn tracked_pane_in_base_checkout_listed_in_agent_panes_is_a_pod_session() {
        let temp = TempDir::new();
        let base = temp.folder("harold");
        write_in(&base, POD_AGENT_PANES, "%5\n");
        let files = PodFiles::new(vec![base.clone()], None);

        assert_eq!(
            classify(&files, Some(path_text(&base)), "%5"),
            Ok(Some(PodRole::PodSession))
        );
        assert_eq!(classify(&files, Some(path_text(&base)), "%6"), Ok(None));
    }

    #[test]
    fn tracked_pane_in_worktree_beside_base_named_by_baton_file_is_a_build_worker() {
        let temp = TempDir::new();
        let base = temp.folder("harold");
        let worktree = temp.folder("feature");
        write_in(&worktree, BATON_AGENT_PANE, "%9\n");
        let files = PodFiles::new(vec![format!("{}/", base.display()).into()], None);

        assert_eq!(
            classify(&files, Some(path_text(&worktree)), "%9"),
            Ok(Some(PodRole::BuildWorker))
        );
    }

    #[test]
    fn tracked_pane_in_unrelated_folder_is_ignored_with_outside_pod_folders() {
        let temp = TempDir::new();
        let base = temp.folder("products/harold");
        let elsewhere = temp.folder("other/place");
        write_in(&elsewhere, POD_AGENT_PANES, "%5\n");
        write_in(&elsewhere, BATON_AGENT_PANE, "%6\n");
        let files = PodFiles::new(vec![base], hour_ago());

        for pane in ["%5", "%6"] {
            let result = classify(&files, Some(path_text(&elsewhere)), pane);
            assert_eq!(result, Err(Ignored::OutsidePodFolders));
            assert_eq!(Ignored::OutsidePodFolders.reason(), "outside_pod_folders");
            assert_eq!(
                super::pod_role(&files, Some(path_text(&elsewhere)), pane),
                None
            );
        }
    }

    #[test]
    fn untracked_pane_listed_in_a_base_checkout_file_newer_than_tmux_start_is_quiet() {
        let temp = TempDir::new();
        let base = temp.folder("harold");
        write_in(&base, POD_AGENT_PANES, "%5\n");
        let files = PodFiles::new(vec![base], hour_ago());

        assert_eq!(classify(&files, None, "%5"), Ok(Some(PodRole::PodSession)));
        assert_eq!(
            super::pod_role(&files, None, "%5"),
            Some(PodRole::PodSession)
        );
        assert_eq!(classify(&files, None, "%6"), Ok(None));
    }

    #[test]
    fn untracked_pane_named_in_a_worktree_baton_file_is_a_build_worker() {
        let temp = TempDir::new();
        let base = temp.folder("harold");
        let worktree = temp.folder("feature");
        temp.folder("empty-sibling");
        write_in(&worktree, BATON_AGENT_PANE, "%9\n");
        let files = PodFiles::new(vec![base], hour_ago());

        assert_eq!(classify(&files, None, "%9"), Ok(Some(PodRole::BuildWorker)));
    }

    #[test]
    fn untracked_pane_in_file_older_than_tmux_start_is_ignored_older_than_tmux_server() {
        let temp = TempDir::new();
        let base = temp.folder("harold");
        write_in(&base, POD_AGENT_PANES, "%5\n");
        let files = PodFiles::new(vec![base], hour_ahead());

        let result = classify(&files, None, "%5");
        assert_eq!(result, Err(Ignored::OlderThanTmuxServer));
        assert_eq!(
            Ignored::OlderThanTmuxServer.reason(),
            "older_than_tmux_server"
        );
        assert_eq!(super::pod_role(&files, None, "%5"), None);
    }

    #[test]
    fn untracked_pane_with_no_tmux_start_time_is_ignored_no_tmux_start_time() {
        let temp = TempDir::new();
        let base = temp.folder("harold");
        write_in(&base, POD_AGENT_PANES, "%5\n");
        let files = PodFiles::new(vec![base], None);

        assert_eq!(classify(&files, None, "%5"), Err(Ignored::NoTmuxStartTime));
        assert_eq!(Ignored::NoTmuxStartTime.reason(), "no_tmux_start_time");
    }

    #[test]
    fn pane_23_does_not_match_a_file_holding_238() {
        let temp = TempDir::new();
        let base = temp.folder("harold");
        write_in(&base, POD_AGENT_PANES, "%238\r\n\n  %2380  \n");
        let files = PodFiles::new(vec![base.clone()], hour_ago());

        assert_eq!(classify(&files, None, "%23"), Ok(None));
        assert_eq!(classify(&files, Some(path_text(&base)), "%23"), Ok(None));
        assert_eq!(
            classify(&files, None, "%238"),
            Ok(Some(PodRole::PodSession))
        );
    }

    #[test]
    fn no_registry_bases_checks_only_the_panes_own_folder() {
        let temp = TempDir::new();
        let folder = temp.folder("anywhere");
        write_in(&folder, POD_AGENT_PANES, "%5\n");
        let files = PodFiles::default();

        assert_eq!(
            classify(&files, Some(path_text(&folder)), "%5"),
            Ok(Some(PodRole::PodSession))
        );
        assert_eq!(classify(&files, None, "%5"), Ok(None));
    }

    // -- Pod and build-worker panes, whether or not the chief runs ----------------------

    use super::{pod_role, quiet_reason};
    use crate::agent::domain::{
        AgentIncarnation, AgentPaneObservation, AgentPaneProjection, AgentSnapshot,
        EffectiveAgentState, MonitorHealthProjection,
    };
    use crate::store::EventVersion;

    fn snapshot_of(inventory_healthy: bool, panes: &[(&str, &str)]) -> AgentSnapshot {
        let version = EventVersion::from_column(3).expect("valid revision");
        AgentSnapshot {
            through_event_version: version,
            server_time_ms: 1_000,
            monitor_health: vec![MonitorHealthProjection {
                component: "inventory".into(),
                healthy: inventory_healthy,
                reason_code: "listing_failed".into(),
                observed_at_ms: 900,
                last_event_version: version,
            }],
            panes: panes
                .iter()
                .map(|(pane_id, folder)| AgentPaneProjection {
                    pane: AgentPaneObservation {
                        incarnation: AgentIncarnation {
                            pane_id: pane_id.to_string(),
                            pane_pid: 1,
                            agent_pid: 2,
                            agent_started_at_ms: 3,
                            provider_id: "codex".into(),
                        },
                        tmux_target: "s:0.0".into(),
                        session_name: "s".into(),
                        window_index: 0,
                        pane_index: 0,
                        working_directory: folder.to_string(),
                        provider_display_name: "Codex".into(),
                        observed_at_ms: 4,
                    },
                    hook_state: None,
                    hook_observed_at_ms: None,
                    lifecycle_adapter_id: None,
                    screen_state: None,
                    screen_classifier_id: None,
                    screen_observed_at_ms: None,
                    effective_state: EffectiveAgentState::Busy,
                    explicit_work_summary: None,
                    explicit_work_summary_updated_at_ms: None,
                    screen_work_summary: None,
                    screen_work_summary_updated_at_ms: None,
                    summary_basis_version: EventVersion::start(),
                    generated_work_summary: None,
                    generated_summary_basis_version: None,
                    work_summary: None,
                    last_transition_at_ms: 5,
                    last_event_version: version,
                })
                .collect(),
            tts_muted: false,
            messaging_paused: false,
        }
    }

    /// A base checkout %2 (pod session) runs in, a worktree beside it where baton worker
    /// %3 runs, and a registry naming the base.
    struct Pod {
        _temp: TempDir,
        base: String,
        worktree: String,
        files: PodFiles,
    }

    fn pod_fixture() -> Pod {
        let temp = TempDir::new();
        let base = temp.folder("harold");
        let worktree = temp.folder("feature");
        write_in(&base, POD_AGENT_PANES, "%2\n%8\n");
        write_in(&worktree, BATON_AGENT_PANE, "%3\n");
        let files = PodFiles::new(vec![base.clone()], hour_ago());
        Pod {
            base: path_text(&base).to_string(),
            worktree: path_text(&worktree).to_string(),
            _temp: temp,
            files,
        }
    }

    const POD_LINE: &str = "spoken update left out (pod session)";
    const WORKER_LINE: &str = "spoken update left out (build worker)";

    #[test]
    fn chief_absent_pod_pane_is_left_out_as_pod_session() {
        let pod = pod_fixture();
        let agents = TrackedAgents::in_folders(&[("%2", &pod.base), ("%7", "/work/harold")])
            .with_pod_files(pod.files.clone());

        assert_eq!(
            quiet_reason(Some(CHIEF), &agents, "%2"),
            Some(POD_LINE.to_string())
        );
        assert_eq!(quiet_reason(Some(CHIEF), &agents, "%7"), None);
    }

    #[test]
    fn chief_absent_baton_pane_is_left_out_as_build_worker() {
        let pod = pod_fixture();
        let agents =
            TrackedAgents::in_folders(&[("%3", &pod.worktree)]).with_pod_files(pod.files.clone());

        assert_eq!(
            quiet_reason(Some(CHIEF), &agents, "%3"),
            Some(WORKER_LINE.to_string())
        );
    }

    #[test]
    fn failed_listing_still_leaves_out_a_pod_pane_from_the_last_listing() {
        let pod = pod_fixture();
        let snapshot = snapshot_of(
            false,
            &[
                ("%2", &pod.base),
                ("%3", &pod.worktree),
                ("%7", "/work/other"),
            ],
        );
        let agents = TrackedAgents::from_snapshot(&snapshot).with_pod_files(pod.files.clone());

        assert_eq!(
            quiet_reason(Some(CHIEF), &agents, "%2"),
            Some(POD_LINE.to_string())
        );
        assert_eq!(
            quiet_reason(Some(CHIEF), &agents, "%3"),
            Some(WORKER_LINE.to_string())
        );
        assert_eq!(quiet_reason(Some(CHIEF), &agents, "%7"), None);
    }

    #[test]
    fn untracked_pane_in_pod_file_is_left_out() {
        let pod = pod_fixture();
        let agents = TrackedAgents::default().with_pod_files(pod.files.clone());

        // %8 is listed in the base's agent-panes; %3 is named by the worktree's baton file.
        assert_eq!(
            quiet_reason(Some(CHIEF), &agents, "%8"),
            Some(POD_LINE.to_string())
        );
        assert_eq!(
            quiet_reason(Some(CHIEF), &agents, "%3"),
            Some(WORKER_LINE.to_string())
        );
        assert_eq!(quiet_reason(Some(CHIEF), &agents, "%9"), None);
    }

    #[test]
    fn no_pod_files_speaks_every_pane_chief_present_or_absent() {
        let present =
            TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold"), ("%3", "/work/x")]);
        let absent = TrackedAgents::in_folders(&[("%2", "/work/harold"), ("%3", "/work/x")]);

        for pane in ["%2", "%3", "%9"] {
            assert_eq!(quiet_reason(Some(CHIEF), &present, pane), None);
            assert_eq!(quiet_reason(Some(CHIEF), &absent, pane), None);
        }
    }

    #[test]
    fn chiefs_own_tracked_pane_is_still_quiet() {
        let present = TrackedAgents::in_folders(&[("%1", CHIEF), ("%2", "/work/harold")]);

        assert_eq!(
            quiet_reason(Some(CHIEF), &present, "%1"),
            Some("spoken update left out (chief)".to_string())
        );
        assert_eq!(quiet_reason(None, &present, "%1"), None);
    }

    #[test]
    fn stale_named_entry_outside_pod_folders_is_spoken() {
        let temp = TempDir::new();
        let base = temp.folder("products/harold");
        let elsewhere = temp.folder("other/place");
        write_in(&elsewhere, POD_AGENT_PANES, "%5\n");
        let files = PodFiles::new(vec![base], hour_ago());
        let agents = TrackedAgents::in_folders(&[("%5", path_text(&elsewhere))])
            .with_pod_files(files.clone());

        assert_eq!(quiet_reason(Some(CHIEF), &agents, "%5"), None);
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%5"), None);
        assert_eq!(pod_role(&files, Some(path_text(&elsewhere)), "%5"), None);
    }

    #[test]
    fn chief_absent_pod_pane_update_skipped_message_and_spoken_line_dropped() {
        let pod = pod_fixture();
        let agents = TrackedAgents::in_folders(&[("%2", &pod.base), ("%3", &pod.worktree)])
            .with_pod_files(pod.files.clone());

        for pane in ["%2", "%3"] {
            assert_eq!(only_from_reason(Some(CHIEF), &agents, pane), reason());
            assert_eq!(
                message_dropped_reason(Some(CHIEF), &agents, pane),
                dropped()
            );
            assert_eq!(
                spoken_line_dropped_reason(Some(CHIEF), &agents, pane),
                Some("spoken line dropped (lines come from /work/chief-of-pods only)".to_string())
            );
            assert_eq!(only_from_reason(None, &agents, pane), None);
        }
    }

    #[test]
    fn chief_absent_unnamed_pane_still_reaches_phone() {
        let pod = pod_fixture();
        let agents = TrackedAgents::in_folders(&[("%2", &pod.base), ("%7", "/work/harold")])
            .with_pod_files(pod.files.clone());

        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%7"), None);
        assert_eq!(message_dropped_reason(Some(CHIEF), &agents, "%7"), None);
        assert_eq!(spoken_line_dropped_reason(Some(CHIEF), &agents, "%9"), None);
    }

    #[test]
    fn failed_listing_rule_b_follows_named_panes_only() {
        let pod = pod_fixture();
        let snapshot = snapshot_of(
            false,
            &[("%1", CHIEF), ("%2", &pod.base), ("%7", "/work/harold")],
        );
        let agents = TrackedAgents::from_snapshot(&snapshot).with_pod_files(pod.files.clone());

        // A pane a file names: left out, message and spoken line dropped.
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%2"), reason());
        assert_eq!(
            message_dropped_reason(Some(CHIEF), &agents, "%2"),
            dropped()
        );
        assert!(spoken_line_dropped_reason(Some(CHIEF), &agents, "%2").is_some());
        // Panes no file names: rule B does nothing while the listing has failed, as today.
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%7"), None);
        assert_eq!(only_from_reason(Some(CHIEF), &agents, "%9"), None);
        assert_eq!(message_dropped_reason(Some(CHIEF), &agents, "%7"), None);
        // The healthy listing of the same panes acts on %7 too.
        let healthy = TrackedAgents::from_snapshot(&snapshot_of(
            true,
            &[("%1", CHIEF), ("%7", "/work/harold")],
        ));
        assert_eq!(only_from_reason(Some(CHIEF), &healthy, "%7"), reason());
    }
}
