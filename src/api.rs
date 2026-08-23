use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use harold_api::harold::{
    AgentMonitorHealth, AgentPaneState, AgentState as ProtoAgentState, AgentStateSnapshot,
    MonitorHealthState as ProtoMonitorHealthState, WatchAgentStatesRequest,
    harold_client::HaroldClient,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tonic::transport::Endpoint;

use crate::app::{
    AgentIncarnation, AgentRow, AgentState, MonitorHealth, MonitorHealthState, Snapshot,
};
use crate::text::sanitize_display;

const PANE_FIELD_LIMIT: usize = 256;
const DIRECTORY_LIMIT: usize = 1_024;
const SUMMARY_LIMIT: usize = 160;
const MONITOR_FIELD_LIMIT: usize = 64;
const ERROR_LIMIT: usize = 512;
const SOURCE_CHANNEL_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    ZeroPanePid,
    ZeroAgentPid,
    NegativeAgentStartedAt,
    NegativeLastTransitionAt,
    DuplicatePaneId,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroPanePid => "snapshot contains a zero pane PID",
            Self::ZeroAgentPid => "snapshot contains a zero agent PID",
            Self::NegativeAgentStartedAt => "snapshot contains a negative agent start time",
            Self::NegativeLastTransitionAt => "snapshot contains a negative agent transition time",
            Self::DuplicatePaneId => "snapshot contains a duplicate pane ID",
        })
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    Transport(String),
    Watch(String),
    Stream(String),
    Protocol(ProtocolError),
}

impl SourceError {
    pub fn detail(&self) -> &str {
        match self {
            Self::Transport(detail) | Self::Watch(detail) | Self::Stream(detail) => detail,
            Self::Protocol(error) => match error {
                ProtocolError::ZeroPanePid => "snapshot contains a zero pane PID",
                ProtocolError::ZeroAgentPid => "snapshot contains a zero agent PID",
                ProtocolError::NegativeAgentStartedAt => {
                    "snapshot contains a negative agent start time"
                }
                ProtocolError::NegativeLastTransitionAt => {
                    "snapshot contains a negative agent transition time"
                }
                ProtocolError::DuplicatePaneId => "snapshot contains a duplicate pane ID",
            },
        }
    }

    fn transport(detail: impl AsRef<str>) -> Self {
        Self::Transport(sanitize_display(detail.as_ref(), ERROR_LIMIT))
    }

    fn watch(detail: impl AsRef<str>) -> Self {
        Self::Watch(sanitize_display(detail.as_ref(), ERROR_LIMIT))
    }

    fn stream(detail: impl AsRef<str>) -> Self {
        Self::Stream(sanitize_display(detail.as_ref(), ERROR_LIMIT))
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(detail) => write!(formatter, "failed to connect to Harold: {detail}"),
            Self::Watch(detail) => write!(formatter, "Harold rejected WatchAgentStates: {detail}"),
            Self::Stream(detail) => write!(formatter, "Harold state stream failed: {detail}"),
            Self::Protocol(error) => write!(formatter, "invalid Harold snapshot: {error}"),
        }
    }
}

impl std::error::Error for SourceError {}

impl From<ProtocolError> for SourceError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

pub struct SourceStream {
    receiver: mpsc::Receiver<Result<Snapshot, SourceError>>,
    reader: JoinHandle<()>,
}

trait SnapshotReader: Send + 'static {
    fn message(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentStateSnapshot>, tonic::Status>> + Send + '_>>;
}

impl SnapshotReader for tonic::Streaming<AgentStateSnapshot> {
    fn message(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<AgentStateSnapshot>, tonic::Status>> + Send + '_>>
    {
        Box::pin(tonic::Streaming::message(self))
    }
}

fn spawn_reader<R>(mut stream: R) -> SourceStream
where
    R: SnapshotReader,
{
    let (sender, receiver) = mpsc::channel(SOURCE_CHANNEL_CAPACITY);
    let reader = tokio::spawn(async move {
        loop {
            let item = match stream.message().await {
                Ok(Some(snapshot)) => map_snapshot(snapshot).map_err(SourceError::from),
                Ok(None) => break,
                Err(error) => Err(SourceError::stream(error.to_string())),
            };
            let closes_stream = item.is_err();
            if sender.send(item).await.is_err() || closes_stream {
                break;
            }
        }
    });

    SourceStream { receiver, reader }
}

impl SourceStream {
    pub async fn recv(&mut self) -> Option<Result<Snapshot, SourceError>> {
        self.receiver.recv().await
    }
}

impl Drop for SourceStream {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

#[derive(Debug, Clone)]
pub struct AgentStateSource {
    endpoint: Endpoint,
}

impl AgentStateSource {
    pub const fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    pub fn open(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<SourceStream, SourceError>> + Send + '_>> {
        Box::pin(async move {
            let mut client = HaroldClient::connect(self.endpoint.clone())
                .await
                .map_err(|error| SourceError::transport(error.to_string()))?;
            let stream = client
                .watch_agent_states(WatchAgentStatesRequest {})
                .await
                .map_err(|error| SourceError::watch(error.to_string()))?
                .into_inner();

            Ok(spawn_reader(stream))
        })
    }
}

pub fn map_snapshot(snapshot: AgentStateSnapshot) -> Result<Snapshot, ProtocolError> {
    let monitor_health = snapshot
        .monitor_health
        .into_iter()
        .map(map_monitor_health)
        .collect();
    let mut pane_ids = HashSet::with_capacity(snapshot.panes.len());
    let mut rows = Vec::with_capacity(snapshot.panes.len());

    for pane in snapshot.panes {
        let row = map_pane(pane)?;
        if !pane_ids.insert(row.incarnation.pane_id.clone()) {
            return Err(ProtocolError::DuplicatePaneId);
        }
        rows.push(row);
    }

    Ok(Snapshot {
        through_event_version: snapshot.through_event_version,
        server_time_ms: snapshot.server_time_ms,
        monitor_health,
        rows,
    })
}

fn map_pane(pane: AgentPaneState) -> Result<AgentRow, ProtocolError> {
    if pane.pane_pid == 0 {
        return Err(ProtocolError::ZeroPanePid);
    }
    if pane.agent_pid == 0 {
        return Err(ProtocolError::ZeroAgentPid);
    }
    if pane.agent_started_at_ms < 0 {
        return Err(ProtocolError::NegativeAgentStartedAt);
    }
    if pane.last_transition_at_ms < 0 {
        return Err(ProtocolError::NegativeLastTransitionAt);
    }

    Ok(AgentRow {
        incarnation: AgentIncarnation {
            pane_id: sanitize_display(&pane.pane_id, PANE_FIELD_LIMIT),
            pane_pid: pane.pane_pid,
            agent_pid: pane.agent_pid,
            agent_started_at_ms: pane.agent_started_at_ms,
            provider_id: sanitize_display(&pane.provider_id, PANE_FIELD_LIMIT),
        },
        provider_display_name: sanitize_display(&pane.provider_display_name, PANE_FIELD_LIMIT),
        tmux_target: sanitize_display(&pane.tmux_target, PANE_FIELD_LIMIT),
        session_name: sanitize_display(&pane.session_name, PANE_FIELD_LIMIT),
        window_index: pane.window_index,
        pane_index: pane.pane_index,
        working_directory: sanitize_display(&pane.working_directory, DIRECTORY_LIMIT),
        work_summary: pane
            .work_summary
            .map(|summary| sanitize_display(&summary, SUMMARY_LIMIT)),
        state: map_agent_state(pane.state),
        last_transition_at_ms: pane.last_transition_at_ms,
    })
}

fn map_monitor_health(health: AgentMonitorHealth) -> MonitorHealth {
    MonitorHealth {
        component: sanitize_display(&health.component, MONITOR_FIELD_LIMIT),
        state: map_monitor_health_state(health.state),
        reason_code: sanitize_display(&health.reason_code, MONITOR_FIELD_LIMIT),
        observed_at_ms: health.observed_at_ms,
    }
}

fn map_agent_state(state: i32) -> AgentState {
    match ProtoAgentState::try_from(state) {
        Ok(ProtoAgentState::Busy) => AgentState::Busy,
        Ok(ProtoAgentState::Idle) => AgentState::Idle,
        Ok(ProtoAgentState::Unspecified | ProtoAgentState::Unknown) | Err(_) => AgentState::Unknown,
    }
}

fn map_monitor_health_state(state: i32) -> MonitorHealthState {
    match ProtoMonitorHealthState::try_from(state) {
        Ok(ProtoMonitorHealthState::Healthy) => MonitorHealthState::Healthy,
        Ok(ProtoMonitorHealthState::Degraded) => MonitorHealthState::Degraded,
        Ok(ProtoMonitorHealthState::Unspecified) | Err(_) => MonitorHealthState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use harold_api::harold::{
        AgentMonitorHealth, AgentPaneState, AgentState as ProtoAgentState, AgentStateSnapshot,
        MonitorHealthState as ProtoMonitorHealthState,
    };
    use tokio::sync::oneshot;

    use crate::app::{AgentState, MonitorHealthState};

    use super::{ProtocolError, SnapshotReader, SourceError, map_snapshot, spawn_reader};

    #[test]
    fn maps_snapshot_metadata_every_pane_field_and_every_known_agent_state() {
        let panes = [
            (ProtoAgentState::Busy, AgentState::Busy),
            (ProtoAgentState::Idle, AgentState::Idle),
            (ProtoAgentState::Unknown, AgentState::Unknown),
            (ProtoAgentState::Unspecified, AgentState::Unknown),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (state, _))| AgentPaneState {
            pane_id: format!("%{index}"),
            tmux_target: format!("agents:3.{index}"),
            session_name: "agents".into(),
            window_index: 3,
            pane_index: index as u32,
            pane_pid: 100 + index as u32,
            agent_pid: 200 + index as u32,
            agent_started_at_ms: 1_000 + index as i64,
            provider_id: format!("provider-{index}"),
            provider_display_name: format!("Provider {index}"),
            working_directory: format!("/work/{index}"),
            state: state.into(),
            last_transition_at_ms: 2_000 + index as i64,
            work_summary: Some(format!("task {index}")),
        })
        .collect();

        let mapped = map_snapshot(AgentStateSnapshot {
            through_event_version: 42,
            server_time_ms: 9_876,
            monitor_health: Vec::new(),
            panes,
        })
        .expect("valid generated snapshot");

        assert_eq!(mapped.through_event_version, 42);
        assert_eq!(mapped.server_time_ms, 9_876);
        assert_eq!(mapped.rows.len(), 4);
        for (index, (_, expected_state)) in [
            (ProtoAgentState::Busy, AgentState::Busy),
            (ProtoAgentState::Idle, AgentState::Idle),
            (ProtoAgentState::Unknown, AgentState::Unknown),
            (ProtoAgentState::Unspecified, AgentState::Unknown),
        ]
        .into_iter()
        .enumerate()
        {
            let row = &mapped.rows[index];
            assert_eq!(row.incarnation.pane_id, format!("%{index}"));
            assert_eq!(row.tmux_target, format!("agents:3.{index}"));
            assert_eq!(row.session_name, "agents");
            assert_eq!(row.window_index, 3);
            assert_eq!(row.pane_index, index as u32);
            assert_eq!(row.incarnation.pane_pid, 100 + index as u32);
            assert_eq!(row.incarnation.agent_pid, 200 + index as u32);
            assert_eq!(row.incarnation.agent_started_at_ms, 1_000 + index as i64);
            assert_eq!(row.incarnation.provider_id, format!("provider-{index}"));
            assert_eq!(row.provider_display_name, format!("Provider {index}"));
            assert_eq!(row.working_directory, format!("/work/{index}"));
            assert_eq!(row.state, expected_state);
            assert_eq!(row.last_transition_at_ms, 2_000 + index as i64);
            assert_eq!(
                row.work_summary.as_deref(),
                Some(format!("task {index}").as_str())
            );
        }
    }

    #[test]
    fn maps_every_monitor_health_state_and_unknown_enum_values_to_unknown() {
        let mapped = map_snapshot(AgentStateSnapshot {
            through_event_version: 1,
            server_time_ms: 2,
            monitor_health: vec![
                proto_health(
                    ProtoMonitorHealthState::Healthy.into(),
                    "inventory",
                    "ok",
                    10,
                ),
                proto_health(
                    ProtoMonitorHealthState::Degraded.into(),
                    "capture",
                    "failed",
                    11,
                ),
                proto_health(
                    ProtoMonitorHealthState::Unspecified.into(),
                    "hooks",
                    "none",
                    12,
                ),
                proto_health(99, "future", "future", 13),
            ],
            panes: vec![AgentPaneState {
                state: 99,
                ..proto_pane("%1")
            }],
        })
        .expect("unknown enums are forward compatible");

        assert_eq!(mapped.rows[0].state, AgentState::Unknown);
        assert_eq!(
            mapped
                .monitor_health
                .iter()
                .map(|health| health.state)
                .collect::<Vec<_>>(),
            vec![
                MonitorHealthState::Healthy,
                MonitorHealthState::Degraded,
                MonitorHealthState::Unknown,
                MonitorHealthState::Unknown,
            ]
        );
        assert_eq!(mapped.monitor_health[1].component, "capture");
        assert_eq!(mapped.monitor_health[1].reason_code, "failed");
        assert_eq!(mapped.monitor_health[1].observed_at_ms, 11);
    }

    #[test]
    fn preserves_summary_presence_and_caps_at_unicode_scalar_boundary() {
        let summaries = [
            None,
            Some(String::new()),
            Some("界".repeat(160)),
            Some(format!("{}z", "界".repeat(160))),
        ];
        let panes = summaries
            .into_iter()
            .enumerate()
            .map(|(index, summary)| AgentPaneState {
                pane_id: format!("%{index}"),
                pane_pid: 10 + index as u32,
                agent_pid: 20 + index as u32,
                work_summary: summary,
                ..proto_pane("ignored")
            })
            .collect();

        let mapped = map_snapshot(AgentStateSnapshot {
            through_event_version: 1,
            server_time_ms: 2,
            monitor_health: Vec::new(),
            panes,
        })
        .expect("valid summaries");

        assert_eq!(mapped.rows[0].work_summary, None);
        assert_eq!(mapped.rows[1].work_summary.as_deref(), Some(""));
        assert_eq!(
            mapped.rows[2]
                .work_summary
                .as_ref()
                .unwrap()
                .chars()
                .count(),
            160
        );
        assert_eq!(
            mapped.rows[3]
                .work_summary
                .as_ref()
                .unwrap()
                .chars()
                .count(),
            160
        );
        assert!(
            mapped.rows[3]
                .work_summary
                .as_ref()
                .unwrap()
                .chars()
                .all(|c| c == '界')
        );
    }

    #[test]
    fn sanitizes_then_caps_every_external_text_field() {
        let over_256 = format!("\x1b[31m{}z", "a".repeat(256));
        let over_1024 = format!("\x1b]0;hidden\x07{}z", "d".repeat(1_024));
        let over_160 = format!("\x1bPprivate\x1b\\{}z", "s".repeat(160));
        let over_64 = format!("\u{9b}31m{}z", "m".repeat(64));
        let pane = AgentPaneState {
            pane_id: over_256.clone(),
            tmux_target: over_256.clone(),
            session_name: over_256.clone(),
            provider_id: over_256.clone(),
            provider_display_name: over_256,
            working_directory: over_1024,
            work_summary: Some(over_160),
            ..proto_pane("ignored")
        };

        let mapped = map_snapshot(AgentStateSnapshot {
            through_event_version: 1,
            server_time_ms: 2,
            monitor_health: vec![proto_health(1, &over_64, &over_64, 3)],
            panes: vec![pane],
        })
        .expect("sanitized snapshot");
        let row = &mapped.rows[0];

        for value in [
            row.incarnation.pane_id.as_str(),
            row.tmux_target.as_str(),
            row.session_name.as_str(),
            row.incarnation.provider_id.as_str(),
            row.provider_display_name.as_str(),
        ] {
            assert_eq!(value.chars().count(), 256);
            assert!(value.chars().all(|character| character == 'a'));
        }
        assert_eq!(row.working_directory.chars().count(), 1_024);
        assert!(
            row.working_directory
                .chars()
                .all(|character| character == 'd')
        );
        assert_eq!(row.work_summary.as_ref().unwrap().chars().count(), 160);
        assert!(
            row.work_summary
                .as_ref()
                .unwrap()
                .chars()
                .all(|character| character == 's')
        );
        assert_eq!(mapped.monitor_health[0].component.chars().count(), 64);
        assert_eq!(mapped.monitor_health[0].reason_code.chars().count(), 64);
        assert!(
            mapped.monitor_health[0]
                .component
                .chars()
                .all(|character| character == 'm')
        );
    }

    #[test]
    fn rejects_zero_pids_negative_process_times_and_duplicate_sanitized_pane_ids() {
        let mut zero_pane_pid = proto_pane("%1");
        zero_pane_pid.pane_pid = 0;
        assert_eq!(
            mapping_error(vec![zero_pane_pid]),
            ProtocolError::ZeroPanePid
        );

        let mut zero_agent_pid = proto_pane("%1");
        zero_agent_pid.agent_pid = 0;
        assert_eq!(
            mapping_error(vec![zero_agent_pid]),
            ProtocolError::ZeroAgentPid
        );

        let mut negative_start = proto_pane("%1");
        negative_start.agent_started_at_ms = -1;
        assert_eq!(
            mapping_error(vec![negative_start]),
            ProtocolError::NegativeAgentStartedAt
        );

        let mut negative_transition = proto_pane("%1");
        negative_transition.last_transition_at_ms = -1;
        assert_eq!(
            mapping_error(vec![negative_transition]),
            ProtocolError::NegativeLastTransitionAt
        );

        let mut first = proto_pane("%1");
        let mut second = proto_pane("%1\x1b[31m");
        first.pane_pid = 1;
        first.agent_pid = 2;
        second.pane_pid = 3;
        second.agent_pid = 4;
        assert_eq!(
            mapping_error(vec![first, second]),
            ProtocolError::DuplicatePaneId
        );
    }

    #[tokio::test]
    async fn reader_maps_generated_snapshot_before_delivery_then_closes_at_eof() {
        let generated = AgentStateSnapshot {
            through_event_version: 7,
            server_time_ms: 8,
            monitor_health: Vec::new(),
            panes: vec![AgentPaneState {
                pane_id: "%1\x1b[31m".into(),
                ..proto_pane("ignored")
            }],
        };
        let mut stream = spawn_reader(FakeReader::new([Ok(Some(generated)), Ok(None)]));

        let mapped = stream.recv().await.expect("snapshot item").unwrap();

        assert_eq!(mapped.through_event_version, 7);
        assert_eq!(mapped.server_time_ms, 8);
        assert_eq!(mapped.rows[0].incarnation.pane_id, "%1");
        assert_eq!(stream.recv().await, None);
    }

    #[tokio::test]
    async fn reader_delivers_protocol_error_then_closes() {
        let mut invalid = proto_pane("%1");
        invalid.pane_pid = 0;
        let mut stream = spawn_reader(FakeReader::new([Ok(Some(AgentStateSnapshot {
            through_event_version: 1,
            server_time_ms: 2,
            monitor_health: Vec::new(),
            panes: vec![invalid],
        }))]));

        assert_eq!(
            stream.recv().await,
            Some(Err(SourceError::Protocol(ProtocolError::ZeroPanePid)))
        );
        assert_eq!(stream.recv().await, None);
    }

    #[tokio::test]
    async fn reader_delivers_sanitized_bounded_stream_error_then_closes() {
        let status = tonic::Status::unknown(format!("\x1b[31m{}z", "e".repeat(600)));
        let mut stream = spawn_reader(FakeReader::new([Err(status)]));

        let error = stream.recv().await.expect("stream error item").unwrap_err();

        assert!(matches!(error, SourceError::Stream(_)));
        assert_eq!(error.detail().chars().count(), 512);
        assert!(!error.detail().chars().any(char::is_control));
        assert_eq!(stream.recv().await, None);
    }

    #[tokio::test]
    async fn reader_normal_eof_closes_without_an_item() {
        let mut stream = spawn_reader(FakeReader::new([Ok(None)]));

        assert_eq!(stream.recv().await, None);
    }

    #[tokio::test]
    async fn dropping_source_stream_aborts_reader_blocked_in_message() {
        let (entered_sender, entered_receiver) = oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let stream = spawn_reader(BlockingReader {
            entered: Some(entered_sender),
            dropped: Arc::clone(&dropped),
        });
        entered_receiver.await.expect("reader entered message");

        drop(stream);
        tokio::task::yield_now().await;

        assert!(dropped.load(Ordering::SeqCst));
    }

    struct FakeReader {
        items: VecDeque<Result<Option<AgentStateSnapshot>, tonic::Status>>,
    }

    impl FakeReader {
        fn new(
            items: impl IntoIterator<Item = Result<Option<AgentStateSnapshot>, tonic::Status>>,
        ) -> Self {
            Self {
                items: items.into_iter().collect(),
            }
        }
    }

    impl SnapshotReader for FakeReader {
        fn message(
            &mut self,
        ) -> Pin<
            Box<dyn Future<Output = Result<Option<AgentStateSnapshot>, tonic::Status>> + Send + '_>,
        > {
            Box::pin(async move { self.items.pop_front().unwrap_or(Ok(None)) })
        }
    }

    struct BlockingReader {
        entered: Option<oneshot::Sender<()>>,
        dropped: Arc<AtomicBool>,
    }

    impl SnapshotReader for BlockingReader {
        fn message(
            &mut self,
        ) -> Pin<
            Box<dyn Future<Output = Result<Option<AgentStateSnapshot>, tonic::Status>> + Send + '_>,
        > {
            let entered = self.entered.take().expect("message called once");
            let guard = DropFlag(Arc::clone(&self.dropped));
            Box::pin(async move {
                let _guard = guard;
                entered.send(()).expect("test receiver alive");
                std::future::pending().await
            })
        }
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn proto_pane(pane_id: &str) -> AgentPaneState {
        AgentPaneState {
            pane_id: pane_id.into(),
            tmux_target: "agents:1.2".into(),
            session_name: "agents".into(),
            window_index: 1,
            pane_index: 2,
            pane_pid: 100,
            agent_pid: 200,
            agent_started_at_ms: 300,
            provider_id: "codex".into(),
            provider_display_name: "Codex".into(),
            working_directory: "/work/project".into(),
            state: ProtoAgentState::Busy.into(),
            last_transition_at_ms: 400,
            work_summary: Some("implement API".into()),
        }
    }

    fn proto_health(
        state: i32,
        component: &str,
        reason_code: &str,
        observed_at_ms: i64,
    ) -> AgentMonitorHealth {
        AgentMonitorHealth {
            component: component.into(),
            state,
            reason_code: reason_code.into(),
            observed_at_ms,
        }
    }

    fn mapping_error(panes: Vec<AgentPaneState>) -> ProtocolError {
        map_snapshot(AgentStateSnapshot {
            through_event_version: 1,
            server_time_ms: 2,
            monitor_health: Vec::new(),
            panes,
        })
        .unwrap_err()
    }
}
