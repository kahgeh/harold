//! The real ports of the restart check: the multiplexer's inventory, the local clock, the typed
//! line and the phone line.

use std::sync::Arc;

use super::{Deliver, PaneView, TimeSource, ViewError};
use crate::agent::inventory::AgentInventoryPort;
use crate::restart_check::runtime::Checker;
use crate::restart_reminder::TurnEnds;
use crate::settings::Settings;
use crate::timed_prompts::{LineOutcome, Ports, Writer, type_line_once};

/// The name of the writer that types the FAIL lines.
const WRITER: &str = "restart_check";

/// The multiplexer's view of a pane, from the inventory that the agent monitor uses.
pub(crate) struct InventoryView(pub(crate) Arc<dyn AgentInventoryPort>);

impl PaneView for InventoryView {
    fn agent_pid(&self, pane: &str) -> Result<Option<u32>, ViewError> {
        match self.0.resolve(pane) {
            Ok(found) => Ok(found.map(|seen| seen.incarnation.agent_pid)),
            Err(_) => Err(ViewError),
        }
    }

    fn session_label(&self, pane: &str) -> Option<String> {
        self.0.resolve(pane).ok()??.session_name.into()
    }
}

/// The local time from `/bin/date`, as in the other timed lines of Harold.
pub(crate) struct SystemClock;

impl TimeSource for SystemClock {
    fn now(&self) -> (String, String) {
        let read = |format: &str| {
            std::process::Command::new("/bin/date")
                .arg(format!("+{format}"))
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|text| text.trim().to_owned())
                .unwrap_or_default()
        };
        (read("%Y-%m-%d %H:%M"), read("%Y%m%d-%H%M"))
    }
}

/// Sends one line to the away device from an agent's pane.
#[tonic::async_trait]
pub(crate) trait PhoneLine: Send + Sync {
    async fn send(&self, source_pane: &str, text: &str) -> bool;
}

/// Sends the line through Harold's own `send_message_to_away_device` call.
pub(crate) struct GrpcPhone(pub(crate) Arc<Settings>);

#[tonic::async_trait]
impl PhoneLine for GrpcPhone {
    async fn send(&self, source_pane: &str, text: &str) -> bool {
        let args = crate::cli::AwayMessageArgs {
            src_id: source_pane.to_owned(),
            text: text.to_owned(),
        };
        crate::cli::send_message_to_away_device(&self.0, &args)
            .await
            .is_ok()
    }
}

/// Types into an idle pane as `Writer::Tool("restart_check")`, and sends the phone line.
pub(crate) struct PaneDeliver<P> {
    pub(crate) ports: Arc<P>,
    pub(crate) phone: Arc<dyn PhoneLine>,
}

#[tonic::async_trait]
impl<P: Ports> Deliver for PaneDeliver<P> {
    async fn type_line(&self, pane: &str, text: &str) -> bool {
        matches!(
            type_line_once(&*self.ports, Writer::Tool(WRITER), pane, text).await,
            LineOutcome::Typed { .. }
        )
    }

    async fn phone_line(&self, source_pane: &str, text: &str) -> bool {
        self.phone.send(source_pane, text).await
    }
}

/// Hands each turn end to the check, then to the next one in the chain.
pub(crate) struct CheckTurnEnds {
    pub(crate) checker: Arc<Checker>,
    pub(crate) next: Option<Arc<dyn TurnEnds>>,
}

impl TurnEnds for CheckTurnEnds {
    fn turn_ended(&self, pane_id: &str, session_id: &str, transcript_path: &str) {
        let checker = Arc::clone(&self.checker);
        let (pane, session) = (pane_id.to_owned(), session_id.to_owned());
        tokio::spawn(async move { checker.on_turn_end(&pane, &session).await });
        if let Some(next) = &self.next {
            next.turn_ended(pane_id, session_id, transcript_path);
        }
    }
}
