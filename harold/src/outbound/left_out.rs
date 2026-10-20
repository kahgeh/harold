//! The decisions behind what keeps a message off the user's channels. Rule A: the pane sent
//! a message with `harold send-message-to-away-device` this turn, so its turn-end update is
//! left out. The channel lists: `[notify]` has one list of folders for each of Harold's four
//! channels (`imessage`, `telegram`, `web_app` and `machine_audio_output`), and a pane may
//! use a channel only when its folder, as the latest good listing of tmux panes has it,
//! matches an entry on that channel's list. `chief` names the chief's pane by the same
//! matching; the chief's own turn-end update is not spoken at the desk.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::agent::domain::AgentSnapshot;
use crate::settings::{Channel, EVERYONE, NotifySettings};

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

fn without_one_trailing_slash(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

/// Whether `folder`, a pane's working directory, matches `entry`, an entry of `chief` or of
/// a channel list. The entry rule: `/…` is the folder exactly, `~/…` the same with `~` as
/// the home folder, `*` any folder, and anything else a folder whose last whole path
/// components are the entry's. One trailing `/` is ignored on either side. Symbolic links
/// are not resolved, components are compared whole and as text, and a subfolder of a
/// matching folder does not match.
pub(crate) fn same_folder(folder: &str, entry: &str) -> bool {
    entry_matches(folder, entry, std::env::var("HOME").ok().as_deref())
}

/// `same_folder` with the home folder given.
pub(crate) fn entry_matches(folder: &str, entry: &str, home: Option<&str>) -> bool {
    if entry == EVERYONE {
        return true;
    }
    let folder = without_one_trailing_slash(folder);
    let entry = without_one_trailing_slash(entry);
    if entry.starts_with('/') {
        return folder == entry;
    }
    if let Some(rest) = entry.strip_prefix("~/") {
        return home.is_some_and(|home| {
            folder
                .strip_prefix(without_one_trailing_slash(home))
                .and_then(|tail| tail.strip_prefix('/'))
                == Some(rest)
        });
    }
    let mut folder_parts = folder.rsplit('/');
    entry
        .rsplit('/')
        .all(|part| folder_parts.next() == Some(part))
}

/// The agent panes Harold tracks and the working directory tmux reports for each, from the
/// latest good listing: while a listing fails the snapshot keeps the last good one's panes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TrackedAgents {
    /// Pane id and working directory.
    folder_by_pane: Vec<(String, String)>,
}

impl TrackedAgents {
    /// The panes in `snapshot`.
    pub(crate) fn from_snapshot(snapshot: &AgentSnapshot) -> Self {
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

    fn folder_of(&self, pane_id: &str) -> Option<&str> {
        self.folder_by_pane
            .iter()
            .find(|(id, _)| id == pane_id)
            .map(|(_, folder)| folder.as_str())
    }

    /// Whether `pane_id` may use `channel`: its folder matches an entry on the channel's
    /// list, or the list holds `*`. An empty list lets no pane through, and a pane Harold
    /// has no folder for yet matches only `*`.
    pub(crate) fn may_use(
        &self,
        settings: &NotifySettings,
        channel: Channel,
        pane_id: &str,
    ) -> bool {
        let list = settings.list(channel);
        if list.iter().any(|entry| entry == EVERYONE) {
            return true;
        }
        let Some(folder) = self.folder_of(pane_id) else {
            return false;
        };
        list.iter().any(|entry| same_folder(folder, entry))
    }

    /// The chief's pane: the one pane whose folder matches `chief`. With several matching
    /// panes there is no chief here (see `resolve_chief`).
    pub(crate) fn chief_pane_id(&self, chief: Option<&str>) -> Option<&str> {
        let lookup = resolve_chief(
            self.folder_by_pane
                .iter()
                .map(|(id, folder)| (id.as_str(), folder.as_str())),
            chief,
        );
        match lookup {
            ChiefLookup::Found(pane_id) => self
                .folder_by_pane
                .iter()
                .find(|(id, _)| *id == pane_id)
                .map(|(id, _)| id.as_str()),
            ChiefLookup::Ambiguous { .. } | ChiefLookup::None => None,
        }
    }

    /// Whether `pane_id`'s folder matches `chief`. Unlike `chief_pane_id` it holds while
    /// several panes match, so a message from the chief's folder is still known as the
    /// chief's.
    pub(crate) fn is_in_chief_folder(&self, chief: Option<&str>, pane_id: &str) -> bool {
        let (Some(chief), Some(folder)) = (chief, self.folder_of(pane_id)) else {
            return false;
        };
        same_folder(folder, chief)
    }
}

/// What looking for the chief's pane found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChiefLookup {
    /// One pane is in the chief's folder.
    Found(String),
    /// `count` panes are in it, so there is no chief.
    Ambiguous { count: usize },
    /// No chief folder is set, or no pane is in it.
    None,
}

impl ChiefLookup {
    fn state(&self) -> (&'static str, usize) {
        match self {
            Self::Found(_) => ("found", 1),
            Self::Ambiguous { count } => ("ambiguous", *count),
            Self::None => ("none", 0),
        }
    }
}

/// The panes (pane id and folder) whose folder matches `folder`, as a `ChiefLookup`. Logs
/// nothing.
pub(crate) fn lookup_in_folder<'a>(
    panes: impl IntoIterator<Item = (&'a str, &'a str)>,
    folder: &str,
) -> ChiefLookup {
    let mut matching = panes
        .into_iter()
        .filter(|(_, pane_folder)| same_folder(pane_folder, folder));
    let Some((first, _)) = matching.next() else {
        return ChiefLookup::None;
    };
    match matching.count() {
        0 => ChiefLookup::Found(first.to_string()),
        others => ChiefLookup::Ambiguous { count: others + 1 },
    }
}

#[cfg(not(test))]
static LAST_CHIEF_STATE: Mutex<&'static str> = Mutex::new("none");

#[cfg(test)]
thread_local! {
    static LAST_CHIEF_STATE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("none") };
}

/// Whether `state` differs from the last one logged, taking it as the last.
fn state_changed(state: &'static str) -> bool {
    #[cfg(not(test))]
    {
        let mut last = LAST_CHIEF_STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        return std::mem::replace(&mut *last, state) != state;
    }
    #[cfg(test)]
    LAST_CHIEF_STATE.with(|last| last.replace(state) != state)
}

/// Logs `chief pane changed` (`state`, `count`, no folder) when `lookup`'s state differs
/// from the last one logged. Harold starts with `none` as the last, so a start with no
/// chief logs nothing.
pub(crate) fn note_chief(lookup: &ChiefLookup) {
    let (state, count) = lookup.state();
    if state_changed(state) {
        tracing::info!(state, count, "chief pane changed");
    }
}

/// The one place that finds the chief's pane: the pane whose folder matches `chief`. With
/// no chief folder set it is `None` and nothing is logged.
pub(crate) fn resolve_chief<'a>(
    panes: impl IntoIterator<Item = (&'a str, &'a str)>,
    chief: Option<&str>,
) -> ChiefLookup {
    let Some(chief) = chief else {
        return ChiefLookup::None;
    };
    let lookup = lookup_in_folder(panes, chief);
    note_chief(&lookup);
    lookup
}

/// The log line for a turn-end update held off the phone by the list under `key`.
pub(crate) fn notification_skipped(key: &str) -> String {
    format!("notification skipped (not in notify.{key})")
}

/// The log line for an agent message held off the phone by the list under `key`.
pub(crate) fn agent_message_dropped(key: &str) -> String {
    format!("agent message dropped (not in notify.{key})")
}

/// The log line for a spoken line held off a channel by the list under `key`.
pub(crate) fn spoken_line_dropped(key: &str) -> String {
    format!("spoken line dropped (not in notify.{key})")
}

/// The key of the list that holds `pane_id` off the phone, or `None` when the pane may use
/// it. The phone's order: the app (`via_app`, the app route is open to this pane), then the
/// list of the channel `away_channel` names. The key is the last list checked.
pub(crate) fn phone_held_back(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    via_app: bool,
    pane_id: &str,
) -> Option<&'static str> {
    let away = settings.away();
    if via_app || agents.may_use(settings, away, pane_id) {
        return None;
    }
    Some(away.key())
}

/// The log line for leaving out the turn-end update of `pane_id` at the desk: the chief's
/// own is not spoken, and nor is one from a pane that is not on `machine_audio_output`.
pub(crate) fn desk_update_reason(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<String> {
    if agents.chief_pane_id(settings.chief.as_deref()) == Some(pane_id) {
        return Some("spoken update left out (chief)".to_string());
    }
    if !agents.may_use(settings, Channel::MachineAudioOutput, pane_id) {
        return Some(format!(
            "spoken update left out (not in notify.{})",
            Channel::MachineAudioOutput.key()
        ));
    }
    None
}

/// The log line for leaving out the locked-screen turn-end update of `pane_id`, when either
/// rule applies. Rule A is looked at first, so its line is the one written when both apply.
pub(crate) fn left_out_reason(
    messaged_this_turn: bool,
    settings: &NotifySettings,
    agents: &TrackedAgents,
    via_app: bool,
    pane_id: &str,
) -> Option<String> {
    if messaged_this_turn {
        return Some(MESSAGED_THIS_TURN.to_string());
    }
    let key = phone_held_back(settings, agents, via_app, pane_id)?;
    Some(notification_skipped(key))
}

/// The log line for dropping a message `pane_id` asked Harold to send. Never holds the text.
pub(crate) fn message_dropped_reason(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    via_app: bool,
    pane_id: &str,
) -> Option<String> {
    let key = phone_held_back(settings, agents, via_app, pane_id)?;
    Some(agent_message_dropped(key))
}

/// The log line for dropping a line `pane_id` asked Harold to speak on the phone. Never
/// holds the spoken text.
pub(crate) fn spoken_line_dropped_reason(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    via_app: bool,
    pane_id: &str,
) -> Option<String> {
    let key = phone_held_back(settings, agents, via_app, pane_id)?;
    Some(spoken_line_dropped(key))
}

/// The log line for dropping a line `pane_id` asked Harold to speak at the desk.
pub(crate) fn desk_line_dropped_reason(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<String> {
    let key = Channel::MachineAudioOutput.key();
    (!agents.may_use(settings, Channel::MachineAudioOutput, pane_id))
        .then(|| spoken_line_dropped(key))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        ChiefLookup, MARK_LIFETIME, MESSAGED_THIS_TURN, TrackedAgents, desk_line_dropped_reason,
        desk_update_reason, entry_matches, left_out_reason, mark_is_fresh, message_dropped_reason,
        phone_held_back, resolve_chief, spoken_line_dropped_reason,
    };
    use crate::agent::domain::{
        AgentIncarnation, AgentPaneObservation, AgentPaneProjection, AgentSnapshot,
        EffectiveAgentState, MonitorHealthProjection,
    };
    use crate::settings::{Channel, NotifySettings};
    use crate::store::EventVersion;

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


    const ONE: [(&str, &str); 2] = [("%1", "/w/chief-of-pods"), ("%2", "/w/harold")];
    const TWO: [(&str, &str); 3] = [
        ("%1", "/w/chief-of-pods"),
        ("%2", "/w/harold"),
        ("%3", "/other/chief-of-pods"),
    ];

    #[test]
    fn resolver_returns_found_ambiguous_and_none() {
        assert_eq!(
            resolve_chief(ONE, Some("chief-of-pods")),
            ChiefLookup::Found("%1".into())
        );
        assert_eq!(
            resolve_chief(TWO, Some("chief-of-pods")),
            ChiefLookup::Ambiguous { count: 2 }
        );
        assert_eq!(resolve_chief(ONE, Some("elsewhere")), ChiefLookup::None);
        assert_eq!(resolve_chief(ONE, None), ChiefLookup::None);
    }

    #[test]
    fn ambiguous_found_ambiguous_logs_each_change_once() {
        let logs = crate::outbound::fake::CapturedLogs::start();
        let chief = Some("chief-of-pods");

        for panes in [&TWO[..], &TWO[..], &ONE[..], &ONE[..], &TWO[..], &TWO[..]] {
            resolve_chief(panes.iter().copied(), chief);
        }

        let text = logs.text();
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("chief pane changed"))
            .collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].contains("state=\"ambiguous\"") && lines[0].contains("count=2"));
        assert!(lines[1].contains("state=\"found\"") && lines[1].contains("count=1"));
        assert!(lines[2].contains("state=\"ambiguous\""));
        assert!(!text.contains("chief pane ambiguous"), "{text}");
        assert!(!text.contains("/w/"), "no folder is logged: {text}");
    }

    #[test]
    fn a_folder_in_the_chief_folder_is_known_while_it_is_ambiguous() {
        let agents = TrackedAgents::in_folders(&TWO);
        assert!(agents.is_in_chief_folder(Some("chief-of-pods"), "%3"));
        assert!(!agents.is_in_chief_folder(Some("chief-of-pods"), "%2"));
        assert!(!agents.is_in_chief_folder(None, "%1"));
        assert!(!agents.is_in_chief_folder(Some("chief-of-pods"), "%9"));
    }

    const HOME: Option<&str> = Some("/Users/x");
    const CHIEF: &str = "/Users/x/Dev/p/chief-of-pods";

    #[test]
    fn full_and_home_entries_match_only_that_folder() {
        for entry in [
            CHIEF,
            "~/Dev/p/chief-of-pods",
            "/Users/x/Dev/p/chief-of-pods/",
        ] {
            assert!(entry_matches(CHIEF, entry, HOME), "{entry}");
            assert!(
                entry_matches("/Users/x/Dev/p/chief-of-pods/", entry, HOME),
                "{entry}"
            );
            assert!(!entry_matches("/tmp/chief-of-pods", entry, HOME), "{entry}");
            assert!(!entry_matches(
                "/Users/x/Dev/p/chief-of-pods/tools",
                entry,
                HOME
            ));
        }
        assert!(!entry_matches(CHIEF, "~/Dev/p/chief-of-pods", None));
    }

    #[test]
    fn partial_entries_match_whole_trailing_components() {
        assert!(entry_matches(CHIEF, "chief-of-pods", HOME));
        assert!(entry_matches(
            "/Users/x/Dev/p/chief-of-pods/",
            "chief-of-pods",
            HOME
        ));
        assert!(!entry_matches(
            "/Users/x/chief-of-pods-old",
            "chief-of-pods",
            HOME
        ));
        assert!(!entry_matches(
            "/Users/x/Dev/p/chief-of-pods/tools",
            "chief-of-pods",
            HOME
        ));
        assert!(!entry_matches(
            "/Users/x/Dev/p/Chief-of-pods",
            "chief-of-pods",
            HOME
        ));
        assert!(entry_matches(CHIEF, "p/chief-of-pods", HOME));
        assert!(!entry_matches(
            "/Users/x/Dev/q/chief-of-pods",
            "p/chief-of-pods",
            HOME
        ));
        assert!(entry_matches("/tmp/chief-of-pods", "chief-of-pods", HOME));
    }

    #[test]
    fn star_matches_any_folder() {
        assert!(entry_matches("/anything", "*", HOME));
    }

    fn settings(overrides: &str) -> NotifySettings {
        let default = format!("{}/config/default", env!("CARGO_MANIFEST_DIR"));
        let all: crate::settings::Settings = config::Config::builder()
            .add_source(config::File::new(&default, config::FileFormat::Toml))
            .add_source(config::File::from_str(overrides, config::FileFormat::Toml))
            .build()
            .and_then(config::Config::try_deserialize)
            .expect("settings");
        all.notify
    }

    const ALL_CHANNELS: [Channel; 4] = [
        Channel::Imessage,
        Channel::Telegram,
        Channel::WebApp,
        Channel::MachineAudioOutput,
    ];

    #[test]
    fn with_the_defaults_only_the_chief_may_use_a_channel() {
        let notify = settings("");
        let agents = TrackedAgents::in_folders(&[
            ("%1", "/any/where/chief-of-pods"),
            ("%2", "/work/harold"),
            ("%3", "/any/where/chief-of-pods/tools"),
        ]);
        for channel in ALL_CHANNELS {
            assert!(agents.may_use(&notify, channel, "%1"), "{channel:?}");
            assert!(!agents.may_use(&notify, channel, "%2"), "{channel:?}");
            assert!(!agents.may_use(&notify, channel, "%3"), "{channel:?}");
            assert!(!agents.may_use(&notify, channel, "%9"), "{channel:?}");
        }
    }

    #[test]
    fn an_empty_list_lets_nobody_through_and_star_lets_everyone() {
        let agents = TrackedAgents::in_folders(&[("%1", "/any/where/chief-of-pods"), ("%2", "/w")]);
        for channel in ALL_CHANNELS {
            let key = channel.key();
            let nobody = settings(&format!("[notify]\n{key} = []"));
            let everyone = settings(&format!("[notify]\n{key} = [\"*\"]"));
            for pane in ["%1", "%2", "%9"] {
                assert!(!agents.may_use(&nobody, channel, pane), "{key} {pane}");
                assert!(agents.may_use(&everyone, channel, pane), "{key} {pane}");
            }
        }
    }

    #[test]
    fn each_list_decides_on_its_own() {
        let notify = settings("[notify]\nmachine_audio_output = [\"a\"]\nimessage = [\"b\"]");
        let agents = TrackedAgents::in_folders(&[("%1", "/w/a"), ("%2", "/w/b")]);
        assert!(agents.may_use(&notify, Channel::MachineAudioOutput, "%1"));
        assert!(!agents.may_use(&notify, Channel::Imessage, "%1"));
        assert!(!agents.may_use(&notify, Channel::MachineAudioOutput, "%2"));
        assert!(agents.may_use(&notify, Channel::Imessage, "%2"));
        assert_eq!(
            message_dropped_reason(&notify, &agents, false, "%1").as_deref(),
            Some("agent message dropped (not in notify.imessage)")
        );
        assert_eq!(
            desk_update_reason(&notify, &agents, "%2").as_deref(),
            Some("spoken update left out (not in notify.machine_audio_output)")
        );
        assert_eq!(message_dropped_reason(&notify, &agents, false, "%2"), None);
    }

    #[test]
    fn telegram_list_decides_when_away_channel_is_telegram() {
        let notify = settings(
            "[notify]\naway_channel = \"telegram\"\nimessage = [\"*\"]\ntelegram = [\"a\"]",
        );
        let agents = TrackedAgents::in_folders(&[("%1", "/w/a"), ("%2", "/w/b")]);
        assert_eq!(phone_held_back(&notify, &agents, false, "%1"), None);
        assert_eq!(
            phone_held_back(&notify, &agents, false, "%2"),
            Some("telegram")
        );
    }

    #[test]
    fn app_route_lets_the_pane_through_and_rule_a_comes_first() {
        let notify = settings("");
        let agents = TrackedAgents::in_folders(&[("%2", "/work/harold")]);
        assert_eq!(phone_held_back(&notify, &agents, true, "%2"), None);
        assert_eq!(
            left_out_reason(true, &notify, &agents, false, "%2").as_deref(),
            Some(MESSAGED_THIS_TURN)
        );
        assert_eq!(
            left_out_reason(false, &notify, &agents, false, "%2").as_deref(),
            Some("notification skipped (not in notify.imessage)")
        );
        assert_eq!(left_out_reason(false, &notify, &agents, true, "%2"), None);
        assert_eq!(
            spoken_line_dropped_reason(&notify, &agents, false, "%2").as_deref(),
            Some("spoken line dropped (not in notify.imessage)")
        );
        assert_eq!(
            desk_line_dropped_reason(&notify, &agents, "%2").as_deref(),
            Some("spoken line dropped (not in notify.machine_audio_output)")
        );
    }

    #[test]
    fn chief_is_quiet_at_the_desk_and_two_matching_panes_are_no_chief() {
        let notify = settings("");
        let one = TrackedAgents::in_folders(&[
            ("%1", "/Users/x/Dev/p/chief-of-pods"),
            ("%2", "/work/harold"),
        ]);
        assert_eq!(one.chief_pane_id(notify.chief.as_deref()), Some("%1"));
        assert_eq!(
            desk_update_reason(&notify, &one, "%1").as_deref(),
            Some("spoken update left out (chief)")
        );
        let two = TrackedAgents::in_folders(&[
            ("%1", "/Users/x/Dev/p/chief-of-pods"),
            ("%2", "/tmp/chief-of-pods"),
        ]);
        assert_eq!(two.chief_pane_id(notify.chief.as_deref()), None);
        for channel in ALL_CHANNELS {
            assert!(two.may_use(&notify, channel, "%1"));
            assert!(two.may_use(&notify, channel, "%2"));
        }
        assert_eq!(desk_update_reason(&notify, &two, "%1"), None);
        let gone = TrackedAgents::in_folders(&[("%2", "/tmp/chief-of-pods")]);
        assert_eq!(gone.chief_pane_id(notify.chief.as_deref()), Some("%2"));
    }

    #[test]
    fn no_agent_in_a_listed_folder_holds_every_pane_back() {
        let notify = settings("");
        let agents = TrackedAgents::in_folders(&[("%2", "/work/harold"), ("%3", "/work/b")]);
        for pane in ["%2", "%3", "%9"] {
            for channel in ALL_CHANNELS {
                assert!(!agents.may_use(&notify, channel, pane));
            }
        }
    }

    #[test]
    fn a_failed_listing_keeps_the_last_good_folders() {
        let notify = settings("");
        let snapshot = snapshot_of(false, &[("%1", "/x/chief-of-pods"), ("%7", "/work/harold")]);
        let agents = TrackedAgents::from_snapshot(&snapshot);
        assert!(agents.may_use(&notify, Channel::Imessage, "%1"));
        assert!(!agents.may_use(&notify, Channel::Imessage, "%7"));
    }

    #[test]
    fn a_folder_is_not_kept_past_the_latest_listing() {
        let notify = settings("");
        let before =
            TrackedAgents::from_snapshot(&snapshot_of(true, &[("%5", "/x/chief-of-pods")]));
        let after = TrackedAgents::from_snapshot(&snapshot_of(true, &[("%5", "/work/other")]));
        assert!(before.may_use(&notify, Channel::MachineAudioOutput, "%5"));
        assert!(!after.may_use(&notify, Channel::MachineAudioOutput, "%5"));
        let back = TrackedAgents::from_snapshot(&snapshot_of(true, &[("%5", "/x/chief-of-pods")]));
        assert!(!after.may_use(&notify, Channel::Imessage, "%5"));
        assert!(back.may_use(&notify, Channel::Imessage, "%5"));
    }

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
}
