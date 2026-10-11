//! Calls between the hub and its members.
//!
//! Telegram lets one process read a bot's replies, so one Mac, the hub, reads them all. A
//! member tells the hub when it is about to notify (`ReportNotification`), which is how the
//! hub knows which Mac notified last. The hub hands a member each reply that member owns
//! (`ForwardReply`). Both calls carry a shared secret.

use std::time::Duration;

use sha2::{Digest, Sha256};
use tonic::transport::{Channel, Endpoint};

use crate::harold::harold_client::HaroldClient;
use crate::harold::{ForwardReplyRequest, ReportNotificationRequest};

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

// ---------------------------------------------------------------------------
// Calling another Mac
// ---------------------------------------------------------------------------

/// How long connecting to a peer may take, and how long the call may take after that.
#[allow(dead_code, reason = "called by the Telegram channel in the next task")]
pub(crate) const PEER_TIMEOUT: Duration = Duration::from_secs(5);

/// A new connection to the Harold at `url`. Each call opens its own and drops it: calls are
/// rare, and a peer that slept or restarted is found out at once.
async fn connect(url: &str, timeout: Duration) -> Result<HaroldClient<Channel>, String> {
    let endpoint = Endpoint::from_shared(url.to_string())
        .map_err(|error| format!("invalid peer url: {error}"))?
        .connect_timeout(timeout)
        .timeout(timeout);
    let channel = endpoint
        .connect()
        .await
        .map_err(|error| format!("could not connect: {error}"))?;
    Ok(HaroldClient::new(channel))
}

/// A refused or failed call, without the request: the request holds the secret.
fn status_error(status: tonic::Status) -> String {
    format!("{:?}: {}", status.code(), status.message())
}

fn accepted(accepted: bool) -> Result<(), String> {
    if accepted {
        Ok(())
    } else {
        Err("the peer did not accept the call".into())
    }
}

/// Tell the hub that `machine`, this member, is about to send a notification. One attempt.
#[allow(dead_code, reason = "called by the Telegram channel in the next task")]
pub(crate) async fn report_notification(hub: &PeerHub, machine: &str) -> Result<(), String> {
    report_notification_within(hub, machine, PEER_TIMEOUT).await
}

async fn report_notification_within(
    hub: &PeerHub,
    machine: &str,
    timeout: Duration,
) -> Result<(), String> {
    let mut client = connect(&hub.url, timeout).await?;
    let response = client
        .report_notification(ReportNotificationRequest {
            machine: machine.to_string(),
            secret: hub.secret.clone(),
        })
        .await
        .map_err(status_error)?;
    accepted(response.into_inner().accepted)
}

/// `report_notification` for a blocking thread of the tokio runtime, which is where the
/// event handler sends notifications. On any other thread it reports an error.
#[allow(dead_code, reason = "called by the Telegram channel in the next task")]
pub(crate) fn report_notification_blocking(hub: &PeerHub, machine: &str) -> Result<(), String> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| "no async runtime on this thread".to_string())?;
    runtime.block_on(report_notification(hub, machine))
}

/// Hand `member` a reply it owns, as the user typed it. One attempt.
#[allow(dead_code, reason = "called by the Telegram channel in the next task")]
pub(crate) async fn forward_reply(member: &PeerMember, text: &str) -> Result<(), String> {
    let mut client = connect(&member.url, PEER_TIMEOUT).await?;
    let response = client
        .forward_reply(ForwardReplyRequest {
            text: text.to_string(),
            secret: member.secret.clone(),
        })
        .await
        .map_err(status_error)?;
    accepted(response.into_inner().accepted)
}

#[cfg(test)]
#[path = "peers_tests.rs"]
mod tests;
