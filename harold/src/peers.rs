//! Calls between the hub and its members.
//!
//! Telegram lets one process read a bot's replies, so one Mac, the hub, reads them all. A
//! member tells the hub when it is about to notify (`ReportNotification`), which is how the
//! hub knows which Mac notified last. The hub hands a member each reply that member owns
//! (`ForwardReply`). Both calls carry a shared secret.

use sha2::{Digest, Sha256};

use crate::inbound::ownership::same_machine;
use crate::settings::{PeerHub, PeerMember, PeerRole, PeersSettings};

/// Which of the two peer RPCs this Harold answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PeerEndpoint {
    /// Neither. The away channel is one every Mac reads (iMessage), where each Mac works
    /// out ownership from the conversation and no Mac tells another.
    Off,
    /// `ReportNotification`, from these members.
    Hub { members: Vec<PeerMember> },
    /// `ForwardReply`, from this hub.
    Member { hub: PeerHub },
}

impl PeerEndpoint {
    pub(crate) fn from_settings(away_channel: &str, peers: &PeersSettings) -> Self {
        if away_channel != "telegram" {
            return Self::Off;
        }
        match (peers.role, &peers.hub) {
            (PeerRole::Hub, _) => Self::Hub {
                members: peers.members.clone(),
            },
            (PeerRole::Member, Some(hub)) => Self::Member { hub: hub.clone() },
            // `validate` refuses this configuration before Harold starts.
            (PeerRole::Member, None) => Self::Off,
        }
    }
}

/// Whether `given` is the secret `expected`. An empty secret never matches. The digests are
/// compared in full whatever the first difference, so the time taken does not tell a caller
/// how much of a guess was right.
pub(crate) fn secrets_match(expected: &str, given: &str) -> bool {
    if expected.is_empty() || given.is_empty() {
        return false;
    }
    let expected = Sha256::digest(expected.as_bytes());
    let given = Sha256::digest(given.as_bytes());
    expected
        .iter()
        .zip(given.iter())
        .fold(0u8, |differing, (a, b)| differing | (a ^ b))
        == 0
}

/// Why a peer call was refused. Logged; the caller is told only that it was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Denied {
    NotTheHub,
    UnknownMember,
    NotAMember,
    WrongSecret,
}

impl Denied {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::NotTheHub => "not_the_hub",
            Self::UnknownMember => "unknown_member",
            Self::NotAMember => "not_a_member",
            Self::WrongSecret => "wrong_secret",
        }
    }
}

/// The member that may report a notification as `machine` with `secret`.
pub(crate) fn authorize_report<'a>(
    endpoint: &'a PeerEndpoint,
    machine: &str,
    secret: &str,
) -> Result<&'a PeerMember, Denied> {
    let PeerEndpoint::Hub { members } = endpoint else {
        return Err(Denied::NotTheHub);
    };
    let member = members
        .iter()
        .find(|member| same_machine(&member.name, machine))
        .ok_or(Denied::UnknownMember)?;
    if !secrets_match(&member.secret, secret) {
        return Err(Denied::WrongSecret);
    }
    Ok(member)
}

/// Whether a reply forwarded with `secret` comes from this member's hub.
pub(crate) fn authorize_forward(endpoint: &PeerEndpoint, secret: &str) -> Result<(), Denied> {
    let PeerEndpoint::Member { hub } = endpoint else {
        return Err(Denied::NotAMember);
    };
    if !secrets_match(&hub.secret, secret) {
        return Err(Denied::WrongSecret);
    }
    Ok(())
}

#[cfg(test)]
#[path = "peers_tests.rs"]
mod tests;
