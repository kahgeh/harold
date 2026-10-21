//! The decisions behind what keeps a message off the user's channels. Rule A: the pane sent
//! a message with `harold send-message-to-away-device` this turn, so its turn-end update is
//! left out. The channel lists: `[notify]` has one list of folders for each of Harold's four
//! channels (`imessage`, `web_app` and `machine_audio_output`), and a pane may
//! use a channel only when its folder, as the latest good listing of tmux panes has it,
//! matches an entry on that channel's list. `chief` names the chief's pane by the same
//! matching; the chief's own turn-end update is not spoken at the desk.

#[cfg(not(test))]
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
    panes: Vec<TrackedPane>,
}

/// One tracked agent pane: where it runs and where it sits in tmux.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedPane {
    pane_id: String,
    /// The working directory tmux reports for the pane.
    folder: String,
    session: String,
    window_index: u32,
    pane_index: u32,
    started_at_ms: i64,
}

impl TrackedAgents {
    /// The panes in `snapshot`.
    pub(crate) fn from_snapshot(snapshot: &AgentSnapshot) -> Self {
        Self {
            panes: snapshot
                .panes
                .iter()
                .map(|projection| TrackedPane {
                    pane_id: projection.pane.incarnation.pane_id.clone(),
                    folder: projection.pane.working_directory.clone(),
                    session: projection.pane.session_name.clone(),
                    window_index: projection.pane.window_index,
                    pane_index: projection.pane.pane_index,
                    started_at_ms: projection.pane.incarnation.agent_started_at_ms,
                })
                .collect(),
        }
    }

    /// Panes in one session, in listing order, all started at the same time.
    #[cfg(test)]
    pub(crate) fn in_folders(panes: &[(&str, &str)]) -> Self {
        Self {
            panes: panes
                .iter()
                .enumerate()
                .map(|(index, (pane_id, folder))| TrackedPane {
                    pane_id: pane_id.to_string(),
                    folder: folder.to_string(),
                    session: String::new(),
                    window_index: 0,
                    pane_index: index as u32,
                    started_at_ms: 0,
                })
                .collect(),
        }
    }

    #[cfg(test)]
    fn with_positions(panes: &[(&str, &str, &str, u32, u32, i64)]) -> Self {
        Self {
            panes: panes
                .iter()
                .map(
                    |&(pane_id, folder, session, window_index, pane_index, started_at_ms)| {
                        TrackedPane {
                            pane_id: pane_id.into(),
                            folder: folder.into(),
                            session: session.into(),
                            window_index,
                            pane_index,
                            started_at_ms,
                        }
                    },
                )
                .collect(),
        }
    }

    /// The `agent` of a notification from `pane_id` (see `agent_label`).
    pub(crate) fn agent_of(&self, pane_id: &str) -> String {
        agent_label(
            self.folder_of(pane_id),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    fn folder_of(&self, pane_id: &str) -> Option<&str> {
        self.panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .map(|pane| pane.folder.as_str())
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

    /// The chief's pane (see `resolve_chief`).
    pub(crate) fn chief_pane_id(
        &self,
        chief: Option<&str>,
        chief_session: Option<&str>,
    ) -> Option<&str> {
        let lookup = resolve_chief(
            self.panes.iter().map(|pane| PanePosition {
                pane_id: &pane.pane_id,
                folder: &pane.folder,
                session: &pane.session,
                window_index: pane.window_index,
                pane_index: pane.pane_index,
                started_at_ms: pane.started_at_ms,
            }),
            chief,
            chief_session,
        );
        match lookup {
            ChiefLookup::Found(pane_id) => self
                .panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .map(|pane| pane.pane_id.as_str()),
            ChiefLookup::Ambiguous { .. } | ChiefLookup::None => None,
        }
    }

    /// Whether `pane_id`'s folder matches `chief`. Unlike `chief_pane_id` it holds for
    /// every pane in the folder, so a message from the chief's folder is still known as
    /// the chief's while another pane is the chief.
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
    /// The pane found.
    Found(String),
    /// `count` panes are in the folder, and the rule for it picks none (`lookup_in_folder`
    /// only; `resolve_chief` never returns this).
    Ambiguous { count: usize },
    /// No chief folder is set, or no pane is in it.
    None,
}

/// Where the chief is: `[notify] chief` and `chief_session`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Chief<'a> {
    pub folder: Option<&'a str>,
    pub session: Option<&'a str>,
}

/// One agent pane with where it runs and where it sits in tmux, for `resolve_chief`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PanePosition<'a> {
    pub pane_id: &'a str,
    pub folder: &'a str,
    pub session: &'a str,
    pub window_index: u32,
    pub pane_index: u32,
    pub started_at_ms: i64,
}

/// How the chief's tmux session was picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionBy {
    /// `[notify] chief_session` named a session with an agent in the folder.
    Setting,
    /// The session of the longest-running agent in the folder.
    Longest,
}

impl SessionBy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Setting => "setting",
            Self::Longest => "longest",
        }
    }
}

struct ChiefPick<'a> {
    pane_id: &'a str,
    /// The agents in the chief's folder.
    count: usize,
    session_by: SessionBy,
}

/// The chief among `panes`: see the rule in `resolve_chief`.
fn pick_chief<'a>(
    panes: impl IntoIterator<Item = PanePosition<'a>>,
    chief: &str,
    chief_session: Option<&str>,
) -> Option<ChiefPick<'a>> {
    let in_folder: Vec<PanePosition<'a>> = panes
        .into_iter()
        .filter(|pane| same_folder(pane.folder, chief))
        .collect();
    let named = chief_session.filter(|name| in_folder.iter().any(|pane| pane.session == *name));
    let (session, session_by) = match named {
        Some(name) => (name, SessionBy::Setting),
        None => {
            let oldest = in_folder
                .iter()
                .min_by_key(|pane| (pane.started_at_ms, pane.session))?;
            (oldest.session, SessionBy::Longest)
        }
    };
    let first = in_folder
        .iter()
        .filter(|pane| pane.session == session)
        .min_by_key(|pane| (pane.window_index, pane.pane_index))?;
    Some(ChiefPick {
        pane_id: first.pane_id,
        count: in_folder.len(),
        session_by,
    })
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

/// What `note_chief` last saw: the pane picked and how its session was picked, both empty
/// for no chief.
type LastChief = (String, &'static str);

#[cfg(not(test))]
static LAST_CHIEF: Mutex<Option<LastChief>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    static LAST_CHIEF: std::cell::RefCell<Option<LastChief>> = const { std::cell::RefCell::new(None) };
}

/// Whether `now` differs from the last one noted (no chief, at the start), taking it as
/// the last.
#[cfg(not(test))]
fn chief_changed(now: LastChief) -> bool {
    let mut last = LAST_CHIEF
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    last.replace(now.clone()).unwrap_or_default() != now
}

#[cfg(test)]
fn chief_changed(now: LastChief) -> bool {
    LAST_CHIEF.with(|last| last.borrow_mut().replace(now.clone()).unwrap_or_default() != now)
}

/// Logs `chief pane changed` (`state`, `count`, `pane_id`, `session_by`, no folder) when
/// the pane picked or how its session was picked differs from the last one logged. Harold
/// starts with no chief as the last, so a start with none logs nothing.
fn note_chief(pick: Option<&ChiefPick<'_>>) {
    let (state, pane_id, session_by, count) = match pick {
        Some(pick) => ("found", pick.pane_id, pick.session_by.as_str(), pick.count),
        None => ("none", "", "", 0),
    };
    if chief_changed((pane_id.to_string(), session_by)) {
        tracing::info!(state, count, pane_id, session_by, "chief pane changed");
    }
}

/// The one place that finds the chief's pane. Among the agent panes whose folder matches
/// `chief`, the chief's tmux session is the one `chief_session` names when an agent there
/// runs in a session with exactly that name, else the session of the longest-running
/// agent (earliest start). The chief is that session's first pane in the folder, by window
/// index, then pane index. It is `None` and nothing is logged with no chief folder set,
/// and never `Ambiguous`.
pub(crate) fn resolve_chief<'a>(
    panes: impl IntoIterator<Item = PanePosition<'a>>,
    chief: Option<&str>,
    chief_session: Option<&str>,
) -> ChiefLookup {
    let Some(chief) = chief else {
        return ChiefLookup::None;
    };
    let pick = pick_chief(panes, chief, chief_session);
    note_chief(pick.as_ref());
    match pick {
        Some(pick) => ChiefLookup::Found(pick.pane_id.to_string()),
        None => ChiefLookup::None,
    }
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

/// What is held back: a turn-end update (left out), an agent message or a spoken line
/// (dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateKind {
    TurnEndUpdate,
    AgentMessage,
    SpokenLine,
}

impl UpdateKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TurnEndUpdate => "turn_end_update",
            Self::AgentMessage => "agent_message",
            Self::SpokenLine => "spoken_line",
        }
    }
}

/// `reason` of a hold that is the channel's list.
pub(crate) const NOT_LISTED: &str = "not_listed";
/// `reason` of the chief's turn-end update left out at the desk.
pub(crate) const CHIEF_REASON: &str = "chief";
/// `reason` of an update left out because its pane sent a message this turn.
pub(crate) const MESSAGED_REASON: &str = "messaged_this_turn";

/// Something a channel's rules keep from going out: the log line, the channel whose rule
/// it was and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Held {
    pub line: String,
    pub channel: Channel,
    pub reason: &'static str,
    /// The pane's `agent` (see `agent_label`).
    pub agent: String,
}

impl Held {
    pub(crate) fn not_listed(line: String, channel: Channel, agent: String) -> Self {
        Self {
            line,
            channel,
            reason: NOT_LISTED,
            agent,
        }
    }

    /// Whether the channel's list kept it, as against the chief rule or the user's own
    /// choice (a message sent this turn).
    pub(crate) fn is_kept(&self) -> bool {
        self.reason == NOT_LISTED
    }

    /// Whether it is held back for `outcome = held_back`: the list and the chief rule. A
    /// pane that sent a message this turn is the user's choice, not a rule that held it.
    pub(crate) fn is_held_back(&self) -> bool {
        self.reason != MESSAGED_REASON
    }

    /// Writes the line, naming the pane and the agent and never the text.
    pub(crate) fn log(&self, kind: UpdateKind, pane_id: &str) {
        tracing::info!(
            pane_id = %pane_id,
            channel = self.channel.key(),
            kind = kind.as_str(),
            reason = self.reason,
            agent = self.agent.as_str(),
            "{}",
            self.line
        );
    }
}

/// A pane's folder as the `agent` of a notification: below the home folder, so it holds
/// no user name; `~` for the home folder itself; the folder as it is when it is outside
/// home; `unknown` when Harold has no folder for the pane.
pub(crate) fn agent_label(folder: Option<&str>, home: Option<&str>) -> String {
    let Some(folder) = folder else {
        return "unknown".to_string();
    };
    let folder = without_one_trailing_slash(folder);
    let relative = home.and_then(|home| {
        let home = without_one_trailing_slash(home);
        if folder == home {
            return Some("~");
        }
        folder.strip_prefix(home)?.strip_prefix('/')
    });
    relative.unwrap_or(folder).to_string()
}

/// Whether `channel` is set up (`set_up`) and turned on: `machine_audio_output` while the
/// voice mute is off, and the phone channels (`imessage` and `web_app`) while messaging is
/// not paused. `machine_audio_output` is always set up.
pub(crate) fn channel_live(
    set_up: bool,
    channel: Channel,
    voice_muted: bool,
    messaging_paused: bool,
) -> bool {
    match channel {
        Channel::MachineAudioOutput => !voice_muted,
        Channel::Imessage | Channel::WebApp => set_up && !messaging_paused,
    }
}

/// The channel that holds `pane_id` off the phone, or `None` when the pane may use it.
/// The phone's order: the app (`via_app`, the app route is open to this pane), then the
/// iMessage list. The channel is the last list checked.
pub(crate) fn phone_held_back(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    via_app: bool,
    pane_id: &str,
) -> Option<Channel> {
    let away = Channel::Imessage;
    if via_app || agents.may_use(settings, away, pane_id) {
        return None;
    }
    Some(away)
}

/// The hold on the turn-end update of `pane_id` at the desk: the chief's own is not
/// spoken, and nor is one from a pane that is not on `machine_audio_output`.
pub(crate) fn desk_update_reason(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<Held> {
    if agents.chief_pane_id(settings.chief.as_deref(), settings.chief_session.as_deref())
        == Some(pane_id)
    {
        return Some(Held {
            line: "spoken update left out (chief)".to_string(),
            channel: Channel::MachineAudioOutput,
            reason: CHIEF_REASON,
            agent: agents.agent_of(pane_id),
        });
    }
    if !agents.may_use(settings, Channel::MachineAudioOutput, pane_id) {
        return Some(Held::not_listed(
            format!(
                "spoken update left out (not in notify.{})",
                Channel::MachineAudioOutput.key()
            ),
            Channel::MachineAudioOutput,
            agents.agent_of(pane_id),
        ));
    }
    None
}

/// The hold on the locked-screen turn-end update of `pane_id`, when either rule applies.
/// Rule A is looked at first, so its line is the one written when both apply.
pub(crate) fn left_out_reason(
    messaged_this_turn: bool,
    settings: &NotifySettings,
    agents: &TrackedAgents,
    via_app: bool,
    pane_id: &str,
) -> Option<Held> {
    if messaged_this_turn {
        return Some(Held {
            line: MESSAGED_THIS_TURN.to_string(),
            channel: Channel::Imessage,
            reason: MESSAGED_REASON,
            agent: agents.agent_of(pane_id),
        });
    }
    let channel = phone_held_back(settings, agents, via_app, pane_id)?;
    Some(Held::not_listed(
        notification_skipped(channel.key()),
        channel,
        agents.agent_of(pane_id),
    ))
}

/// The hold on a message `pane_id` asked Harold to send. Never holds the text.
pub(crate) fn message_dropped_reason(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    via_app: bool,
    pane_id: &str,
) -> Option<Held> {
    let channel = phone_held_back(settings, agents, via_app, pane_id)?;
    Some(Held::not_listed(
        agent_message_dropped(channel.key()),
        channel,
        agents.agent_of(pane_id),
    ))
}

/// The hold on a line `pane_id` asked Harold to speak on the phone. Never holds the
/// spoken text.
pub(crate) fn spoken_line_dropped_reason(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    via_app: bool,
    pane_id: &str,
) -> Option<Held> {
    let channel = phone_held_back(settings, agents, via_app, pane_id)?;
    Some(Held::not_listed(
        spoken_line_dropped(channel.key()),
        channel,
        agents.agent_of(pane_id),
    ))
}

/// The hold on a line `pane_id` asked Harold to speak at the desk.
pub(crate) fn desk_line_dropped_reason(
    settings: &NotifySettings,
    agents: &TrackedAgents,
    pane_id: &str,
) -> Option<Held> {
    let channel = Channel::MachineAudioOutput;
    (!agents.may_use(settings, channel, pane_id)).then(|| {
        Held::not_listed(
            spoken_line_dropped(channel.key()),
            channel,
            agents.agent_of(pane_id),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        ChiefLookup, MARK_LIFETIME, MESSAGED_THIS_TURN, PanePosition, TrackedAgents, agent_label,
        channel_live, desk_line_dropped_reason, desk_update_reason, entry_matches, left_out_reason,
        mark_is_fresh, message_dropped_reason, phone_held_back, resolve_chief,
        spoken_line_dropped_reason,
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

    const CHIEF_SET: Option<&str> = Some("chief-of-pods");

    fn at<'a>(
        pane_id: &'a str,
        folder: &'a str,
        session: &'a str,
        window_index: u32,
        pane_index: u32,
        started_at_ms: i64,
    ) -> PanePosition<'a> {
        PanePosition {
            pane_id,
            folder,
            session,
            window_index,
            pane_index,
            started_at_ms,
        }
    }

    fn found(pane: &str) -> ChiefLookup {
        ChiefLookup::Found(pane.into())
    }

    #[test]
    fn one_agent_gives_that_pane() {
        let panes = [
            at("%1", "/w/chief-of-pods", "chief", 0, 0, 10),
            at("%2", "/w/harold", "pod", 0, 0, 5),
        ];
        assert_eq!(resolve_chief(panes, CHIEF_SET, None), found("%1"));
    }

    #[test]
    fn a_second_pane_in_the_session_with_a_higher_index_leaves_the_first() {
        let panes = [
            at("%2", "/w/chief-of-pods", "chief", 0, 1, 20),
            at("%1", "/w/chief-of-pods", "chief", 0, 0, 10),
        ];
        assert_eq!(resolve_chief(panes, CHIEF_SET, None), found("%1"));
    }

    #[test]
    fn a_lower_window_index_in_the_session_becomes_the_chief() {
        let panes = [
            at("%1", "/w/chief-of-pods", "chief", 1, 0, 10),
            at("%2", "/w/chief-of-pods", "chief", 0, 3, 20),
        ];
        assert_eq!(resolve_chief(panes, CHIEF_SET, None), found("%2"));
    }

    #[test]
    fn no_agent_or_no_chief_setting_gives_no_chief() {
        let panes = [at("%1", "/w/chief-of-pods", "chief", 0, 0, 10)];
        assert_eq!(
            resolve_chief(panes, Some("elsewhere"), None),
            ChiefLookup::None
        );
        assert_eq!(resolve_chief(panes, None, None), ChiefLookup::None);
        assert_eq!(resolve_chief([], CHIEF_SET, None), ChiefLookup::None);
    }

    #[test]
    fn the_named_session_wins_over_an_older_agent_elsewhere() {
        let panes = [
            at("%1", "/w/chief-of-pods", "other", 0, 0, 1),
            at("%2", "/w/chief-of-pods", "chief", 0, 0, 99),
        ];
        assert_eq!(resolve_chief(panes, CHIEF_SET, Some("chief")), found("%2"));
    }

    #[test]
    fn unset_picks_the_session_of_the_longest_running_agent() {
        let panes = [
            at("%1", "/w/chief-of-pods", "newer", 0, 0, 50),
            at("%2", "/w/chief-of-pods", "older", 0, 4, 10),
            at("%3", "/w/chief-of-pods", "older", 0, 2, 70),
        ];
        assert_eq!(resolve_chief(panes, CHIEF_SET, None), found("%3"));
    }

    #[test]
    fn the_session_match_is_exact() {
        let panes = [
            at("%1", "/w/chief-of-pods", "chief-2", 0, 0, 5),
            at("%2", "/w/chief-of-pods", "chie", 0, 0, 6),
            at("%3", "/w/chief-of-pods", "x", 0, 0, 7),
        ];
        assert_eq!(resolve_chief(panes, CHIEF_SET, Some("chief")), found("%1"));
    }

    #[test]
    fn equal_start_times_pick_the_lower_session_name() {
        let panes = [
            at("%1", "/w/chief-of-pods", "b", 0, 0, 5),
            at("%2", "/w/chief-of-pods", "a", 0, 0, 5),
        ];
        assert_eq!(resolve_chief(panes, CHIEF_SET, None), found("%2"));
    }

    fn change_lines(logs: &crate::outbound::fake::CapturedLogs) -> Vec<String> {
        logs.text()
            .lines()
            .filter(|line| line.contains("chief pane changed"))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_pane_change_logs_one_found_line_and_no_change_logs_nothing() {
        let logs = crate::outbound::fake::CapturedLogs::start();
        let first = [
            at("%1", "/w/chief-of-pods", "chief", 0, 0, 10),
            at("%2", "/w/chief-of-pods", "chief", 0, 1, 20),
        ];
        let moved = [at("%2", "/w/chief-of-pods", "chief", 0, 1, 20)];

        for panes in [&first[..], &first[..], &moved[..], &moved[..]] {
            resolve_chief(panes.iter().copied(), CHIEF_SET, Some("chief"));
        }

        let lines = change_lines(&logs);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("state=\"found\"") && lines[0].contains("count=2"));
        assert!(lines[0].contains("pane_id=%1") || lines[0].contains("pane_id=\"%1\""));
        assert!(lines[0].contains("session_by=\"setting\""));
        assert!(lines[1].contains("count=1") && lines[1].contains("%2"));
        assert!(!logs.text().contains("/w/"), "no folder is logged");
    }

    #[test]
    fn a_fallback_logs_session_by_longest_and_a_vanished_chief_logs_none() {
        let logs = crate::outbound::fake::CapturedLogs::start();
        let panes = [at("%1", "/w/chief-of-pods", "other", 0, 0, 10)];

        resolve_chief(panes, CHIEF_SET, Some("chief"));
        resolve_chief([], CHIEF_SET, Some("chief"));

        let lines = change_lines(&logs);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("session_by=\"longest\""));
        assert!(lines[1].contains("state=\"none\"") && lines[1].contains("count=0"));
    }

    #[test]
    fn a_folder_in_the_chief_folder_is_known_while_another_pane_is_the_chief() {
        let agents = TrackedAgents::in_folders(&[
            ("%1", "/w/chief-of-pods"),
            ("%2", "/w/harold"),
            ("%3", "/other/chief-of-pods"),
        ]);
        assert!(agents.is_in_chief_folder(Some("chief-of-pods"), "%3"));
        assert!(!agents.is_in_chief_folder(Some("chief-of-pods"), "%2"));
        assert!(!agents.is_in_chief_folder(None, "%1"));
        assert!(!agents.is_in_chief_folder(Some("chief-of-pods"), "%9"));
    }

    #[test]
    fn tracked_agents_use_the_session_setting() {
        let agents = TrackedAgents::with_positions(&[
            ("%1", "/w/chief-of-pods", "old", 0, 0, 1),
            ("%2", "/w/chief-of-pods", "chief", 0, 0, 9),
        ]);
        assert_eq!(agents.chief_pane_id(CHIEF_SET, None), Some("%1"));
        assert_eq!(agents.chief_pane_id(CHIEF_SET, Some("chief")), Some("%2"));
    }

    #[test]
    fn agent_is_the_folder_below_home() {
        let home = Some("/Users/x");
        assert_eq!(
            agent_label(Some("/Users/x/Dev/p/harold/main"), home),
            "Dev/p/harold/main"
        );
        assert_eq!(
            agent_label(Some("/Users/x/Dev/p/harold/"), home),
            "Dev/p/harold"
        );
        assert_eq!(agent_label(Some("/Users/x"), home), "~");
        assert_eq!(agent_label(Some("/tmp/a"), home), "/tmp/a");
        assert_eq!(
            agent_label(Some("/Users/xavier/a"), home),
            "/Users/xavier/a"
        );
        assert_eq!(agent_label(None, home), "unknown");
    }

    #[test]
    fn a_channel_is_live_when_set_up_and_turned_on() {
        // (channel, set_up, voice_muted, paused, live)
        let table = [
            (Channel::Imessage, true, false, false, true),
            (Channel::Imessage, false, false, false, false),
            (Channel::Imessage, true, false, true, false),
            (Channel::WebApp, true, false, false, true),
            (Channel::WebApp, false, false, false, false),
            (Channel::WebApp, true, false, true, false),
            (Channel::WebApp, true, true, false, true),
            (Channel::MachineAudioOutput, true, false, false, true),
            (Channel::MachineAudioOutput, true, true, false, false),
            (Channel::MachineAudioOutput, true, false, true, true),
        ];
        for (channel, set_up, muted, paused, live) in table {
            assert_eq!(
                channel_live(set_up, channel, muted, paused),
                live,
                "{channel:?} set_up={set_up} muted={muted} paused={paused}"
            );
        }
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

    const ALL_CHANNELS: [Channel; 3] = [
        Channel::Imessage,
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
            message_dropped_reason(&notify, &agents, false, "%1")
                .map(|held| held.line)
                .as_deref(),
            Some("agent message dropped (not in notify.imessage)")
        );
        assert_eq!(
            desk_update_reason(&notify, &agents, "%2")
                .map(|held| held.line)
                .as_deref(),
            Some("spoken update left out (not in notify.machine_audio_output)")
        );
        assert_eq!(message_dropped_reason(&notify, &agents, false, "%2"), None);
    }

    #[test]
    fn imessage_list_decides_who_reaches_the_phone() {
        let notify = settings("[notify]\nimessage = [\"a\"]");
        let agents = TrackedAgents::in_folders(&[("%1", "/w/a"), ("%2", "/w/b")]);
        assert_eq!(phone_held_back(&notify, &agents, false, "%1"), None);
        assert_eq!(
            phone_held_back(&notify, &agents, false, "%2"),
            Some(Channel::Imessage)
        );
    }

    #[test]
    fn app_route_lets_the_pane_through_and_rule_a_comes_first() {
        let notify = settings("");
        let agents = TrackedAgents::in_folders(&[("%2", "/work/harold")]);
        assert_eq!(phone_held_back(&notify, &agents, true, "%2"), None);
        assert_eq!(
            left_out_reason(true, &notify, &agents, false, "%2")
                .map(|held| held.line)
                .as_deref(),
            Some(MESSAGED_THIS_TURN)
        );
        assert_eq!(
            left_out_reason(false, &notify, &agents, false, "%2")
                .map(|held| held.line)
                .as_deref(),
            Some("notification skipped (not in notify.imessage)")
        );
        assert_eq!(left_out_reason(false, &notify, &agents, true, "%2"), None);
        assert_eq!(
            spoken_line_dropped_reason(&notify, &agents, false, "%2")
                .map(|held| held.line)
                .as_deref(),
            Some("spoken line dropped (not in notify.imessage)")
        );
        assert_eq!(
            desk_line_dropped_reason(&notify, &agents, "%2")
                .map(|held| held.line)
                .as_deref(),
            Some("spoken line dropped (not in notify.machine_audio_output)")
        );
    }

    #[test]
    fn chief_is_quiet_at_the_desk_and_the_first_of_two_matching_panes_is_the_chief() {
        let notify = settings("");
        let one = TrackedAgents::in_folders(&[
            ("%1", "/Users/x/Dev/p/chief-of-pods"),
            ("%2", "/work/harold"),
        ]);
        assert_eq!(one.chief_pane_id(notify.chief.as_deref(), None), Some("%1"));
        assert_eq!(
            desk_update_reason(&notify, &one, "%1")
                .map(|held| held.line)
                .as_deref(),
            Some("spoken update left out (chief)")
        );
        let two = TrackedAgents::in_folders(&[
            ("%1", "/Users/x/Dev/p/chief-of-pods"),
            ("%2", "/tmp/chief-of-pods"),
        ]);
        assert_eq!(two.chief_pane_id(notify.chief.as_deref(), None), Some("%1"));
        for channel in ALL_CHANNELS {
            assert!(two.may_use(&notify, channel, "%1"));
            assert!(two.may_use(&notify, channel, "%2"));
        }
        assert_eq!(
            desk_update_reason(&notify, &two, "%1")
                .map(|held| held.line)
                .as_deref(),
            Some("spoken update left out (chief)")
        );
        assert_eq!(desk_update_reason(&notify, &two, "%2"), None);
        let gone = TrackedAgents::in_folders(&[("%2", "/tmp/chief-of-pods")]);
        assert_eq!(
            gone.chief_pane_id(notify.chief.as_deref(), None),
            Some("%2")
        );
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
