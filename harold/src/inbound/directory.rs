// ---------------------------------------------------------------------------
// AgentAddress — the address *is* the inbound channel
// ---------------------------------------------------------------------------

/// How to reach an agent session. Each variant knows how to relay a message
/// to the agent it represents.
#[derive(Debug, Clone)]
pub enum AgentAddress {
    TmuxPane { pane_id: String, label: String },
}

impl AgentAddress {
    pub fn label(&self) -> &str {
        match self {
            AgentAddress::TmuxPane { label, .. } => label,
        }
    }

    pub(crate) fn same_target(&self, other: &AgentAddress) -> bool {
        match (self, other) {
            (
                AgentAddress::TmuxPane { pane_id: a, .. },
                AgentAddress::TmuxPane { pane_id: b, .. },
            ) => a == b,
        }
    }

    /// Relay a message to this agent via its native transport.
    pub fn relay(&self, text: &str) {
        match self {
            AgentAddress::TmuxPane { pane_id, .. } => {
                super::tmux::relay_to_tmux_pane(pane_id, text);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn pane_id(&self) -> &str {
        match self {
            AgentAddress::TmuxPane { pane_id, .. } => pane_id,
        }
    }
}

// ---------------------------------------------------------------------------
// AgentDirectory — discovers live agents
// ---------------------------------------------------------------------------

pub enum AgentDirectory {
    TmuxProcessScan,
}

impl AgentDirectory {
    pub fn discover(&self) -> Vec<AgentAddress> {
        match self {
            AgentDirectory::TmuxProcessScan => super::tmux::scan_live_panes(),
        }
    }

    pub fn is_alive(&self, addr: &AgentAddress) -> bool {
        match self {
            AgentDirectory::TmuxProcessScan => match addr {
                AgentAddress::TmuxPane { pane_id, .. } => super::tmux::is_pane_alive(pane_id),
            },
        }
    }
}
