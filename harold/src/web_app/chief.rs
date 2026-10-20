//! Which agent pane is the chief's: the one agent pane whose folder `[notify] chief`
//! matches. The web app's thread belongs to it.

use crate::agent::domain::{AgentSnapshot, EffectiveAgentState};
use crate::outbound::left_out::{ChiefLookup, resolve_chief};
use crate::timed_prompts::agents_view;

/// The chief, as the latest snapshot sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChiefPane {
    /// One agent runs in the folder. `busy` is true only while its state is busy, so an
    /// unknown state reads as not busy.
    One { pane_id: String, busy: bool },
    /// No folder is set, or no agent runs in it.
    NoChief,
    /// More than one agent runs in a folder `chief` matches, so there is no chief.
    Several,
    /// Harold's last listing of tmux panes failed, so the snapshot's panes may be stale.
    ListingFailed,
}

/// The chief's pane in `snapshot`, with `chief_folder` as the folder setting.
pub(crate) fn find_chief(snapshot: &AgentSnapshot, chief_folder: Option<&str>) -> ChiefPane {
    let Some(folder) = chief_folder else {
        return ChiefPane::NoChief;
    };
    let view = agents_view(snapshot, &[]);
    if view.listing_failed {
        return ChiefPane::ListingFailed;
    }
    let found = resolve_chief(
        view.panes
            .iter()
            .map(|pane| (pane.pane_id.as_str(), pane.folder.as_str())),
        Some(folder),
    );
    match found {
        ChiefLookup::None => ChiefPane::NoChief,
        ChiefLookup::Found(pane_id) => {
            let busy = view
                .panes
                .iter()
                .any(|pane| pane.pane_id == pane_id && pane.state == EffectiveAgentState::Busy);
            ChiefPane::One { pane_id, busy }
        }
        ChiefLookup::Ambiguous { .. } => ChiefPane::Several,
    }
}

/// False while the listing fails, with no folder set, or when several agents run in a
/// folder `chief` matches; otherwise true for the one pane in it.
pub(crate) fn is_chief_pane(
    snapshot: &AgentSnapshot,
    chief_folder: Option<&str>,
    pane_id: &str,
) -> bool {
    matches!(
        find_chief(snapshot, chief_folder),
        ChiefPane::One { pane_id: chief, .. } if chief == pane_id
    )
}

#[cfg(test)]
#[path = "chief_tests.rs"]
mod tests;
