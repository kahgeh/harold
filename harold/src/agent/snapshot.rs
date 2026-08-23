use tokio::sync::watch;

use super::domain::AgentSnapshot;

#[derive(Clone)]
pub(crate) struct AgentSnapshotHub {
    sender: watch::Sender<AgentSnapshot>,
    _receiver: watch::Receiver<AgentSnapshot>,
}

impl AgentSnapshotHub {
    pub(crate) fn new(initial: AgentSnapshot) -> Self {
        let (sender, receiver) = watch::channel(initial);
        Self {
            sender,
            _receiver: receiver,
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<AgentSnapshot> {
        self.sender.subscribe()
    }

    pub(crate) fn publish_committed(&self, snapshot: AgentSnapshot) {
        self.sender.send_if_modified(|current| {
            if snapshot.through_event_version.get() <= current.through_event_version.get() {
                return false;
            }
            *current = snapshot;
            true
        });
    }
}
