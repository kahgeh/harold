use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use harold_api::harold::{
    AgentMonitorHealth, AgentPaneState, AgentState as ProtoAgentState, AgentStateSnapshot,
    CallAction as ProtoCallAction, CallRequest, CallResponse, CallResult as ProtoCallResult,
    CallState as ProtoCallState, MonitorHealthState as ProtoMonitorHealthState,
    SetMessagingPausedRequest, SetTtsMutedRequest, WatchAgentStatesRequest,
    harold_client::HaroldClient,
};
use harold_api::tools::backlog::v1::{
    ListQueueRequest, ListQueueResponse, backlog_service_client::BacklogServiceClient,
};
use harold_api::tools::questions::v1::{
    AnswerKind as ProtoAnswerKind, AnswerQuestionRequest, Decision, DecisionOption,
    DeferQuestionRequest, ListQuestionsRequest, ListQuestionsResponse, ListedQuestion, Pick,
    SavePicksRequest, Step, TickStepRequest, question_service_client::QuestionServiceClient,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tonic::transport::{Channel, Endpoint};

use crate::app::{
    AgentIncarnation, AgentRow, AgentState, CallAction, CallState, CallView, MonitorHealth,
    MonitorHealthState, Snapshot,
};
use crate::questions::{
    AnswerKind, DecisionView, OptionView, Outgoing, PickView, QuestionView, Queue, SendResult,
    StepView,
};
use crate::text::sanitize_display;

const VOICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// A press of `c` or `t` waits longer than Harold's 5 s limit for closing the voice worker,
/// so a STOP that waits for the call's cleanup is not reported as timed out.
const CALL_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const PANE_FIELD_LIMIT: usize = 256;
const DIRECTORY_LIMIT: usize = 1_024;
const SUMMARY_LIMIT: usize = 160;
const MONITOR_FIELD_LIMIT: usize = 64;
pub const HAROLD_VERSION_LIMIT: usize = 32;
const TRANSCRIPT_LIMIT: usize = 1_024;
const ERROR_LIMIT: usize = 512;
/// Harold refuses a question field of 2,000 characters or more.
const QUESTION_FIELD_LIMIT: usize = 2_000;
/// The Now line is one terminal row, so its text is cut well before the field's own size.
const NOW_LINE_LIMIT: usize = 512;
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
    /// A request Harold did not answer within its timeout. It reads as a transport
    /// error; the runtime tells it apart to skip a second read in the same poll.
    TimedOut(String),
    Watch(String),
    Stream(String),
    Protocol(ProtocolError),
}

impl SourceError {
    pub fn detail(&self) -> &str {
        match self {
            Self::Transport(detail)
            | Self::TimedOut(detail)
            | Self::Watch(detail)
            | Self::Stream(detail) => detail,
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

    fn timed_out(detail: impl AsRef<str>) -> Self {
        Self::TimedOut(sanitize_display(detail.as_ref(), ERROR_LIMIT))
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
            Self::Transport(detail) | Self::TimedOut(detail) => {
                write!(formatter, "failed to connect to Harold: {detail}")
            }
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
    reader: Option<JoinHandle<()>>,
    voice: Option<HaroldClient<Channel>>,
    questions: Option<QuestionServiceClient<Channel>>,
    backlog: Option<BacklogServiceClient<Channel>>,
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

    SourceStream {
        receiver,
        reader: Some(reader),
        voice: None,
        questions: None,
        backlog: None,
    }
}

impl SourceStream {
    pub async fn recv(&mut self) -> Option<Result<Snapshot, SourceError>> {
        self.receiver.recv().await
    }

    pub async fn request_tts_muted(&self, muted: bool) -> Result<(), SourceError> {
        let Some(mut client) = self.voice.clone() else {
            return Err(SourceError::transport("voice control unavailable"));
        };
        tokio::time::timeout(
            VOICE_REQUEST_TIMEOUT,
            client.set_tts_muted(SetTtsMutedRequest { muted }),
        )
        .await
        .map_err(|_| SourceError::transport("voice request timed out"))?
        .map(|_| ())
        .map_err(|error| SourceError::transport(error.to_string()))
    }

    pub async fn request_messaging_paused(&self, paused: bool) -> Result<(), SourceError> {
        let Some(mut client) = self.voice.clone() else {
            return Err(SourceError::transport("messaging control unavailable"));
        };
        tokio::time::timeout(
            VOICE_REQUEST_TIMEOUT,
            client.set_messaging_paused(SetMessagingPausedRequest { paused }),
        )
        .await
        .map_err(|_| SourceError::transport("messaging request timed out"))?
        .map(|_| ())
        .map_err(|error| SourceError::transport(error.to_string()))
    }

    pub async fn request_call(&self, action: CallAction) -> Result<CallView, SourceError> {
        let Some(mut client) = self.voice.clone() else {
            return Err(SourceError::transport("call control unavailable"));
        };
        let proto_action = match action {
            CallAction::Start => ProtoCallAction::Start,
            CallAction::Stop => ProtoCallAction::Stop,
            CallAction::TalkNow => ProtoCallAction::TalkNow,
            CallAction::Status => ProtoCallAction::Status,
        };
        tokio::time::timeout(
            call_request_timeout(action),
            client.call(CallRequest {
                action: proto_action.into(),
            }),
        )
        .await
        .map_err(|_| SourceError::transport("call request timed out"))?
        .map(|response| map_call(response.into_inner()))
        .map_err(|error| SourceError::transport(error.to_string()))
    }

    /// The question queue, or an error when the tool cannot be reached: an older Harold
    /// answers `Unimplemented`.
    pub async fn list_questions(&self) -> Result<Queue, SourceError> {
        let Some(mut client) = self.questions.clone() else {
            return Err(SourceError::transport("question tool unavailable"));
        };
        tokio::time::timeout(
            VOICE_REQUEST_TIMEOUT,
            client.list_questions(ListQuestionsRequest {}),
        )
        .await
        .map_err(|_| SourceError::timed_out("question request timed out"))?
        .map(|response| map_queue(response.into_inner()))
        .map_err(|error| SourceError::transport(error.to_string()))
    }

    /// The first line of the chief's Now item, `None` when the queue has none, or an
    /// error when the backlog tool cannot be reached: an older Harold answers
    /// `Unimplemented`.
    pub async fn list_queue_now(&self) -> Result<Option<String>, SourceError> {
        let Some(mut client) = self.backlog.clone() else {
            return Err(SourceError::transport("backlog tool unavailable"));
        };
        tokio::time::timeout(VOICE_REQUEST_TIMEOUT, client.list_queue(now_request()))
            .await
            .map_err(|_| SourceError::transport("backlog request timed out"))?
            .map(|response| map_now(response.into_inner()))
            .map_err(|error| SourceError::transport(error.to_string()))
    }

    pub async fn answer_question(&self, out: &Outgoing) -> SendResult {
        let Some(mut client) = self.questions.clone() else {
            return SendResult::Unavailable;
        };
        let request = answer_request(out);
        match tokio::time::timeout(VOICE_REQUEST_TIMEOUT, client.answer_question(request)).await {
            Ok(result) => answer_send_result(result.map(|_| ())),
            Err(_) => SendResult::Unavailable,
        }
    }

    pub async fn tick_step(&self, id: &str, version: u32, step: u32, ticked: bool) -> SendResult {
        let Some(mut client) = self.questions.clone() else {
            return SendResult::Unavailable;
        };
        let request = tick_request(id, version, step, ticked);
        match tokio::time::timeout(VOICE_REQUEST_TIMEOUT, client.tick_step(request)).await {
            Ok(result) => tick_send_result(result.map(|_| ())),
            Err(_) => SendResult::Unavailable,
        }
    }

    pub async fn save_picks(&self, id: &str, version: u32, picks: &[PickView]) -> SendResult {
        let Some(mut client) = self.questions.clone() else {
            return SendResult::Unavailable;
        };
        let request = save_picks_request(id, version, picks);
        match tokio::time::timeout(VOICE_REQUEST_TIMEOUT, client.save_picks(request)).await {
            Ok(result) => answer_send_result(result.map(|_| ())),
            Err(_) => SendResult::Unavailable,
        }
    }

    pub async fn defer_question(&self, id: &str) -> Result<(), SourceError> {
        let Some(mut client) = self.questions.clone() else {
            return Err(SourceError::transport("question tool unavailable"));
        };
        tokio::time::timeout(
            VOICE_REQUEST_TIMEOUT,
            client.defer_question(DeferQuestionRequest { id: id.to_owned() }),
        )
        .await
        .map_err(|_| SourceError::transport("question request timed out"))?
        .map(|_| ())
        .map_err(|error| SourceError::transport(error.to_string()))
    }

    pub async fn close(mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
            let _ = reader.await;
        }
    }
}

impl Drop for SourceStream {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
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
            let channel = self
                .endpoint
                .connect()
                .await
                .map_err(|error| SourceError::transport(error.to_string()))?;
            let mut client = HaroldClient::new(channel.clone());
            let stream = client
                .watch_agent_states(WatchAgentStatesRequest {})
                .await
                .map_err(|error| SourceError::watch(error.to_string()))?
                .into_inner();

            let mut source = spawn_reader(stream);
            source.voice = Some(client);
            source.questions = Some(QuestionServiceClient::new(channel.clone()));
            source.backlog = Some(BacklogServiceClient::new(channel));
            Ok(source)
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
        tts_muted: snapshot.tts_muted,
        messaging_paused: snapshot.messaging_paused,
        harold_version: Some(sanitize_display(
            &snapshot.harold_version,
            HAROLD_VERSION_LIMIT,
        ))
        .filter(|version| !version.is_empty()),
    })
}

/// What `AnswerQuestion`'s answer means for the block. Only the two refusals the tool
/// names are told apart; anything else, an older Harold or a failed write included,
/// means the answer was not saved.
pub(crate) fn answer_send_result(result: Result<(), tonic::Status>) -> SendResult {
    let Err(status) = result else {
        return SendResult::Recorded;
    };
    match (status.code(), status.message()) {
        (tonic::Code::FailedPrecondition, "version_changed") => SendResult::VersionChanged,
        (tonic::Code::FailedPrecondition, "not_waiting") => SendResult::NotWaiting,
        _ => SendResult::Unavailable,
    }
}

/// What `TickStep`'s answer means. The dash only sends steps in range, so an
/// `invalid step` means the item is not the instructions it saw: it is no longer waiting.
pub(crate) fn tick_send_result(result: Result<(), tonic::Status>) -> SendResult {
    match result {
        Err(status) if status.code() == tonic::Code::InvalidArgument => SendResult::NotWaiting,
        other => answer_send_result(other),
    }
}

/// The tick request; `step` counts from 1, as the tool does.
pub(crate) fn tick_request(id: &str, version: u32, step: u32, ticked: bool) -> TickStepRequest {
    TickStepRequest {
        id: id.to_owned(),
        version,
        step,
        ticked,
    }
}

/// The save request; decisions count from 0, as the tool does.
pub(crate) fn save_picks_request(id: &str, version: u32, picks: &[PickView]) -> SavePicksRequest {
    SavePicksRequest {
        id: id.to_owned(),
        version,
        picks: picks.iter().map(proto_pick).collect(),
    }
}

fn proto_pick(pick: &PickView) -> Pick {
    Pick {
        decision: pick.decision,
        key: pick.key.clone(),
        text: pick.text.clone(),
    }
}

fn answer_request(out: &Outgoing) -> AnswerQuestionRequest {
    AnswerQuestionRequest {
        id: out.id.clone(),
        version: out.version,
        kind: proto_answer_kind(out.kind) as i32,
        picks: out.picks.iter().map(proto_pick).collect(),
        note: out.note.clone(),
        send_now: out.send_now,
    }
}

fn proto_answer_kind(kind: AnswerKind) -> ProtoAnswerKind {
    match kind {
        AnswerKind::Answer => ProtoAnswerKind::Answer,
        AnswerKind::Revise => ProtoAnswerKind::Revise,
        AnswerKind::Reject => ProtoAnswerKind::Reject,
        AnswerKind::FreeText => ProtoAnswerKind::FreeText,
        AnswerKind::Done => ProtoAnswerKind::Done,
        AnswerKind::Ignore => ProtoAnswerKind::Ignore,
    }
}

/// The question queue as the block shows it. Every text is cleaned for display. A
/// question with no decisions, or with a decision that has no options, is left out, so
/// the block never indexes into empty data; it is not counted as waiting either.
pub fn map_queue(response: ListQuestionsResponse) -> Queue {
    let listed = response.questions.len();
    let questions: Vec<QuestionView> = response
        .questions
        .into_iter()
        .filter_map(map_listed_question)
        .collect();
    let dropped = u32::try_from(listed - questions.len()).unwrap_or(u32::MAX);
    Queue {
        waiting: response.waiting.saturating_sub(dropped),
        questions,
        dismissed: response
            .dismissed
            .into_iter()
            .map(|dismissed| {
                (
                    question_text(&dismissed.id),
                    question_text(&dismissed.reason),
                )
            })
            .collect(),
    }
}

fn map_listed_question(listed: ListedQuestion) -> Option<QuestionView> {
    let question = listed.question?;
    let instructions = !question.steps.is_empty();
    if instructions == !question.decisions.is_empty()
        || question
            .decisions
            .iter()
            .any(|decision| decision.options.is_empty())
    {
        return None;
    }
    Some(QuestionView {
        id: question_text(&question.id),
        product: question_text(&question.product),
        spec: question_text(&question.spec),
        sha: question_text(&question.sha),
        link: question_text(&question.link),
        intro: question_text(&question.intro),
        version: listed.version,
        deferred: listed.deferred,
        decisions: question.decisions.into_iter().map(map_decision).collect(),
        title: question_text(&question.title),
        steps: question.steps.into_iter().map(map_step).collect(),
        ticked: listed.ticked,
        picks: listed.picks.into_iter().map(map_pick).collect(),
    })
}

fn map_pick(pick: Pick) -> PickView {
    PickView {
        decision: pick.decision,
        key: question_text(&pick.key),
        text: question_text(&pick.text),
    }
}

fn map_step(step: Step) -> StepView {
    StepView {
        text: question_text(&step.text),
        detail: question_text(&step.detail),
    }
}

fn map_decision(decision: Decision) -> DecisionView {
    DecisionView {
        ask: question_text(&decision.ask),
        options: decision.options.into_iter().map(map_option).collect(),
    }
}

fn map_option(option: DecisionOption) -> OptionView {
    OptionView {
        key: question_text(&option.key),
        label: question_text(&option.label),
        impact: question_text(&option.impact),
        suggested: option.suggested,
        free: option.free,
    }
}

/// The dash's queue read. `done_since_ms` 0 would return every done item, a history
/// that only grows, so the read asks for done items from the end of time: none.
pub(crate) fn now_request() -> ListQueueRequest {
    ListQueueRequest {
        done_since_ms: i64::MAX,
    }
}

/// The Now item's first line, cleaned and cut, or `None` when there is no Now item or
/// its first line is blank.
pub fn map_now(response: ListQueueResponse) -> Option<String> {
    let now = response.now?;
    let line = sanitize_display(now.text.lines().next()?, NOW_LINE_LIMIT);
    let line = line.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

fn question_text(text: &str) -> String {
    sanitize_display(text, QUESTION_FIELD_LIMIT)
}

/// How long the dashboard waits for Harold's answer to `action`. A Status poll is awaited
/// inline every 500 ms during a call and Harold answers it at once, so it keeps the short
/// voice timeout; a press waits `CALL_REQUEST_TIMEOUT`.
pub(crate) fn call_request_timeout(action: CallAction) -> Duration {
    match action {
        CallAction::Status => VOICE_REQUEST_TIMEOUT,
        CallAction::Start | CallAction::Stop | CallAction::TalkNow => CALL_REQUEST_TIMEOUT,
    }
}

/// Harold's call answer as the dashboard shows it. A state the dashboard does not
/// know counts as not set up, so the button never offers a call it cannot follow.
pub fn map_call(response: CallResponse) -> CallView {
    let state = match ProtoCallState::try_from(response.state) {
        Ok(ProtoCallState::Off) => CallState::Off,
        Ok(ProtoCallState::Listening) => CallState::Listening,
        Ok(ProtoCallState::Transcribing | ProtoCallState::Waiting) => CallState::Thinking,
        Ok(ProtoCallState::Speaking) => CallState::Speaking,
        Ok(ProtoCallState::NotSetUp | ProtoCallState::Unspecified) | Err(_) => CallState::NotSetUp,
    };
    let last_result = match ProtoCallResult::try_from(response.last_result) {
        Ok(ProtoCallResult::Typed) => Some("typed"),
        Ok(ProtoCallResult::NothingHeard) => Some("nothing heard"),
        Ok(ProtoCallResult::NoChief) => Some("no chief"),
        Ok(ProtoCallResult::ChiefBusy) => Some("chief busy"),
        Ok(ProtoCallResult::Failed) => Some("failed"),
        Ok(ProtoCallResult::VoiceMuted) => Some("voice muted"),
        Ok(ProtoCallResult::Unspecified) | Err(_) => None,
    };
    CallView {
        state,
        chief_pane_id: sanitize_display(&response.chief_pane_id, PANE_FIELD_LIMIT),
        last_transcript: sanitize_display(&response.last_transcript, TRANSCRIPT_LIMIT),
        last_result,
    }
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
    use std::time::Duration;

    use harold_api::harold::{
        AgentMonitorHealth, AgentPaneState, AgentState as ProtoAgentState, AgentStateSnapshot,
        CallResponse, CallResult as ProtoCallResult, CallState as ProtoCallState,
        MonitorHealthState as ProtoMonitorHealthState,
    };
    use tokio::sync::oneshot;

    use crate::app::{AgentState, CallAction, CallState, CallView, MonitorHealthState};

    use super::{
        ProtocolError, SnapshotReader, SourceError, call_request_timeout, map_call, map_now,
        map_snapshot, now_request, spawn_reader,
    };

    #[test]
    fn maps_the_harold_version_cleaned_and_cut_or_none_when_empty() {
        let map = |version: &str| {
            map_snapshot(AgentStateSnapshot {
                harold_version: version.into(),
                ..Default::default()
            })
            .unwrap()
            .harold_version
        };
        assert_eq!(map(""), None);
        assert_eq!(map("\x1b[31m"), None);
        assert_eq!(map("v0.9.0"), Some("v0.9.0".to_owned()));
        assert_eq!(map("v0.9.0\x1b[31m\u{7}"), Some("v0.9.0".to_owned()));
        assert_eq!(map(&"x".repeat(40)), Some("x".repeat(32)));
    }

    #[test]
    fn maps_tts_muted_from_the_snapshot() {
        for muted in [false, true] {
            let mapped = map_snapshot(AgentStateSnapshot {
                tts_muted: muted,
                ..Default::default()
            })
            .unwrap();
            assert_eq!(mapped.tts_muted, muted);
        }
    }

    #[test]
    fn maps_messaging_paused_from_the_snapshot() {
        for paused in [false, true] {
            let mapped = map_snapshot(AgentStateSnapshot {
                messaging_paused: paused,
                ..Default::default()
            })
            .unwrap();
            assert_eq!(mapped.messaging_paused, paused);
            assert!(!mapped.tts_muted);
        }
    }

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
            harold_version: String::new(),
            tts_muted: false,
            messaging_paused: false,
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
            harold_version: String::new(),
            tts_muted: false,
            messaging_paused: false,
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
            harold_version: String::new(),
            tts_muted: false,
            messaging_paused: false,
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
            harold_version: String::new(),
            tts_muted: false,
            messaging_paused: false,
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
            harold_version: String::new(),
            tts_muted: false,
            messaging_paused: false,
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
            harold_version: String::new(),
            tts_muted: false,
            messaging_paused: false,
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

    #[tokio::test]
    async fn closing_source_stream_aborts_and_awaits_reader() {
        let (entered_sender, entered_receiver) = oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let stream = spawn_reader(BlockingReader {
            entered: Some(entered_sender),
            dropped: Arc::clone(&dropped),
        });
        entered_receiver.await.expect("reader entered message");

        stream.close().await;

        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn request_tts_muted_without_a_client_reports_unavailable() {
        let stream = spawn_reader(FakeReader::new([]));
        let error = stream.request_tts_muted(true).await.unwrap_err();
        assert!(error.detail().contains("voice control unavailable"));
    }

    #[tokio::test]
    async fn list_queue_now_without_a_client_reports_unavailable() {
        let stream = spawn_reader(FakeReader::new([]));
        let error = stream.list_queue_now().await.unwrap_err();
        assert!(error.detail().contains("backlog tool unavailable"));
    }

    #[test]
    fn a_timeout_reads_as_a_transport_error() {
        let timed_out = SourceError::timed_out("question request timed out");
        let transport = SourceError::transport("question request timed out");
        assert_eq!(timed_out.detail(), transport.detail());
        assert_eq!(timed_out.to_string(), transport.to_string());
        assert_ne!(timed_out, transport);
    }

    #[test]
    fn the_now_read_asks_for_no_done_items() {
        assert_eq!(now_request().done_since_ms, i64::MAX);
    }

    #[test]
    fn map_now_takes_the_now_items_first_line_cleaned_or_none() {
        use harold_api::tools::backlog::v1::{ListQueueResponse, QueueItem};
        let response = |text: Option<&str>| ListQueueResponse {
            now: text.map(|text| QueueItem {
                id: "q1".into(),
                state: "now".into(),
                text: text.into(),
                ..QueueItem::default()
            }),
            parked: vec![QueueItem {
                text: "parked".into(),
                ..QueueItem::default()
            }],
            ..ListQueueResponse::default()
        };
        assert_eq!(
            map_now(response(Some("Ship \x1b[31mit\nthen the rest"))),
            Some("Ship it".into())
        );
        assert_eq!(map_now(response(None)), None, "a parked item is not Now");
        assert_eq!(map_now(response(Some(""))), None);
        assert_eq!(map_now(response(Some("  \nsecond"))), None);
        let long = "x".repeat(600);
        assert_eq!(
            map_now(response(Some(&long))).map(|line| line.chars().count()),
            Some(512)
        );
    }

    #[tokio::test]
    async fn request_messaging_paused_without_a_client_reports_unavailable() {
        let stream = spawn_reader(FakeReader::new([]));
        let error = stream.request_messaging_paused(true).await.unwrap_err();
        assert!(error.detail().contains("messaging control unavailable"));
    }

    #[test]
    fn maps_every_call_state_and_result() {
        for (state, expected) in [
            (ProtoCallState::Unspecified as i32, CallState::NotSetUp),
            (ProtoCallState::Off as i32, CallState::Off),
            (ProtoCallState::Listening as i32, CallState::Listening),
            (ProtoCallState::Transcribing as i32, CallState::Thinking),
            (ProtoCallState::Waiting as i32, CallState::Thinking),
            (ProtoCallState::Speaking as i32, CallState::Speaking),
            (ProtoCallState::NotSetUp as i32, CallState::NotSetUp),
            (99, CallState::NotSetUp),
        ] {
            let view = map_call(CallResponse {
                state,
                ..Default::default()
            });
            assert_eq!(view.state, expected, "{state}");
        }
        for (result, expected) in [
            (ProtoCallResult::Unspecified as i32, None),
            (ProtoCallResult::Typed as i32, Some("typed")),
            (ProtoCallResult::NothingHeard as i32, Some("nothing heard")),
            (ProtoCallResult::NoChief as i32, Some("no chief")),
            (ProtoCallResult::ChiefBusy as i32, Some("chief busy")),
            (ProtoCallResult::Failed as i32, Some("failed")),
            (ProtoCallResult::VoiceMuted as i32, Some("voice muted")),
            (99, None),
        ] {
            let view = map_call(CallResponse {
                last_result: result,
                ..Default::default()
            });
            assert_eq!(view.last_result, expected, "{result}");
        }

        let view = map_call(CallResponse {
            state: ProtoCallState::Listening.into(),
            chief_pane_id: "%3\x1b[31m".into(),
            last_transcript: "approve\x1b]0;x\x07 q19".into(),
            last_result: ProtoCallResult::Typed.into(),
        });
        assert_eq!(
            view,
            CallView {
                state: CallState::Listening,
                chief_pane_id: "%3".into(),
                last_transcript: "approve q19".into(),
                last_result: Some("typed"),
            }
        );
    }

    #[test]
    fn a_call_press_outwaits_the_workers_close_and_a_status_poll_stays_short() {
        // Harold closes the voice worker within 5 s when a call ends.
        let close_limit = Duration::from_secs(5);
        for action in [CallAction::Start, CallAction::Stop, CallAction::TalkNow] {
            assert_eq!(call_request_timeout(action), Duration::from_secs(8));
            assert!(call_request_timeout(action) > close_limit);
        }
        assert_eq!(
            call_request_timeout(CallAction::Status),
            Duration::from_secs(2)
        );
    }

    #[tokio::test]
    async fn request_call_without_a_client_reports_unavailable() {
        let stream = spawn_reader(FakeReader::new([]));
        let error = stream.request_call(CallAction::Status).await.unwrap_err();
        assert!(error.detail().contains("call control unavailable"));
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
            harold_version: String::new(),
            tts_muted: false,
            messaging_paused: false,
            through_event_version: 1,
            server_time_ms: 2,
            monitor_health: Vec::new(),
            panes,
        })
        .unwrap_err()
    }
}

#[cfg(test)]
mod question_tests {
    use harold_api::tools::questions::v1::{
        AnswerKind as ProtoAnswerKind, Decision, DecisionOption, Dismissed, ListQuestionsResponse,
        ListedQuestion, Question, Step,
    };

    use super::{
        answer_send_result, map_queue, proto_answer_kind, save_picks_request, tick_request,
    };
    use crate::questions::SendResult;

    fn option(key: &str, suggested: bool, free: bool) -> DecisionOption {
        DecisionOption {
            key: key.into(),
            label: format!("label {key}"),
            impact: format!("impact {key}"),
            suggested,
            free,
        }
    }

    fn listed(id: &str, version: u32, deferred: bool, decisions: Vec<Decision>) -> ListedQuestion {
        ListedQuestion {
            question: Some(Question {
                id: id.into(),
                product: "harold".into(),
                spec: "dash-question-queue".into(),
                sha: "d84bf386a2a5".into(),
                link: "https://example.com/spec".into(),
                intro: "We need a choice.".into(),
                decisions,
                title: String::new(),
                steps: vec![],
            }),
            version,
            deferred,
            ticked: vec![],
            picks: vec![],
        }
    }

    fn decision(ask: &str, options: Vec<DecisionOption>) -> Decision {
        Decision {
            ask: ask.into(),
            options,
        }
    }

    #[test]
    fn map_queue_keeps_order_versions_and_dismissals() {
        let mut hostile = listed(
            "Q3",
            1,
            false,
            vec![decision("\x1b[2Jwipe?", vec![option("a", false, false)])],
        );
        let question = hostile.question.as_mut().unwrap();
        question.intro = "line one\nline two\x1b]0;title\x07".into();
        question.product = "\x1b[31mred".into();
        let mut free_label = option("z", false, true);
        free_label.label = "other\r".into();

        let queue = map_queue(ListQuestionsResponse {
            questions: vec![
                listed(
                    "Q2",
                    4,
                    false,
                    vec![
                        decision(
                            "Protocol?",
                            vec![option("a", false, false), option("b", true, false)],
                        ),
                        decision("Build?", vec![option("y", true, false), free_label]),
                    ],
                ),
                listed(
                    "Q1",
                    1,
                    true,
                    vec![decision("Later?", vec![option("a", true, false)])],
                ),
                // No decisions, or a decision with no options: never shown.
                listed("Q8", 1, false, vec![]),
                listed("Q9", 1, false, vec![decision("Empty?", vec![])]),
                ListedQuestion {
                    question: None,
                    version: 1,
                    deferred: false,
                    ticked: vec![],
                    picks: vec![],
                },
                hostile,
            ],
            waiting: 6,
            dismissed: vec![Dismissed {
                id: "Q7".into(),
                reason: "no longer \x1b[1mneeded".into(),
            }],
        });

        let ids: Vec<&str> = queue.questions.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids, ["Q2", "Q1", "Q3"]);
        assert_eq!(queue.waiting, 3, "the dropped questions are not counted");
        let q2 = &queue.questions[0];
        assert_eq!((q2.version, q2.deferred), (4, false));
        assert_eq!(
            (queue.questions[1].version, queue.questions[1].deferred),
            (1, true)
        );
        assert_eq!(q2.product, "harold");
        assert_eq!(q2.spec, "dash-question-queue");
        assert_eq!(q2.sha, "d84bf386a2a5");
        assert_eq!(q2.link, "https://example.com/spec");
        assert_eq!(q2.decisions.len(), 2);
        assert_eq!(q2.decisions[0].ask, "Protocol?");
        let b = &q2.decisions[0].options[1];
        assert_eq!(
            (
                b.key.as_str(),
                b.label.as_str(),
                b.impact.as_str(),
                b.suggested,
                b.free
            ),
            ("b", "label b", "impact b", true, false)
        );
        let z = &q2.decisions[1].options[1];
        assert!(z.free);
        assert_eq!(z.label, "other");

        let q3 = &queue.questions[2];
        assert_eq!(q3.product, "red");
        assert_eq!(q3.intro, "line oneline two");
        assert_eq!(q3.decisions[0].ask, "wipe?");

        assert_eq!(
            queue.dismissed,
            vec![("Q7".to_string(), "no longer needed".to_string())]
        );
    }

    #[test]
    fn answer_refusals_map_to_send_results() {
        assert_eq!(answer_send_result(Ok(())), SendResult::Recorded);
        assert_eq!(
            answer_send_result(Err(tonic::Status::failed_precondition("version_changed"))),
            SendResult::VersionChanged
        );
        assert_eq!(
            answer_send_result(Err(tonic::Status::failed_precondition("not_waiting"))),
            SendResult::NotWaiting
        );
        for status in [
            tonic::Status::failed_precondition("something_else"),
            tonic::Status::unavailable("write failed"),
            tonic::Status::unimplemented("no such service"),
            tonic::Status::invalid_argument("invalid picks"),
            tonic::Status::unknown("transport error"),
            tonic::Status::deadline_exceeded("timed out"),
        ] {
            assert_eq!(
                answer_send_result(Err(status.clone())),
                SendResult::Unavailable,
                "{status:?}"
            );
        }
    }

    #[test]
    fn the_answer_request_carries_send_now() {
        for send_now in [false, true] {
            let out = crate::questions::Outgoing {
                id: "Q1".into(),
                version: 2,
                kind: crate::questions::AnswerKind::Answer,
                picks: vec![],
                note: String::new(),
                send_now,
            };
            assert_eq!(super::answer_request(&out).send_now, send_now);
        }
    }

    #[test]
    fn map_queue_maps_instructions_with_steps_and_ticks() {
        let queue = map_queue(ListQuestionsResponse {
            questions: vec![ListedQuestion {
                question: Some(Question {
                    id: "I1".into(),
                    product: "harold".into(),
                    spec: String::new(),
                    sha: String::new(),
                    link: String::new(),
                    intro: "do these".into(),
                    decisions: vec![],
                    title: "Ship it".into(),
                    steps: vec![
                        Step {
                            text: "one".into(),
                            detail: "first".into(),
                        },
                        Step {
                            text: "two".into(),
                            detail: String::new(),
                        },
                    ],
                }),
                version: 3,
                deferred: false,
                ticked: vec![2],
                picks: vec![],
            }],
            waiting: 1,
            dismissed: vec![],
        });
        assert_eq!(queue.questions.len(), 1);
        let q = &queue.questions[0];
        assert!(q.is_instructions());
        assert_eq!(q.title, "Ship it");
        assert_eq!(q.steps.len(), 2);
        assert_eq!(q.steps[0].text, "one");
        assert_eq!(q.steps[0].detail, "first");
        assert_eq!(q.ticked, vec![2]);
        assert_eq!(
            (q.spec.as_str(), q.sha.as_str(), q.link.as_str()),
            ("", "", "")
        );
        assert!(q.decisions.is_empty());
        assert_eq!(queue.waiting, 1);

        // Neither steps nor decisions is still dropped.
        let mut none = listed("Q8", 1, false, vec![]);
        none.question.as_mut().unwrap().title = "t".into();
        let queue = map_queue(ListQuestionsResponse {
            questions: vec![none],
            waiting: 1,
            dismissed: vec![],
        });
        assert!(queue.questions.is_empty());
    }

    #[test]
    fn map_queue_cleans_step_text() {
        let mut item = listed("I1", 1, false, vec![]);
        let question = item.question.as_mut().unwrap();
        question.title = "\x1b[31mTitle\x1b[0m".into();
        question.steps = vec![Step {
            text: "\x1b[2Jrun\r".into(),
            detail: "line\x1b]0;x\x07 one".into(),
        }];
        let queue = map_queue(ListQuestionsResponse {
            questions: vec![item],
            waiting: 1,
            dismissed: vec![],
        });
        let q = &queue.questions[0];
        assert_eq!(q.steps[0].text, "run");
        assert_eq!(q.steps[0].detail, "line one");
        assert!(!q.title.contains('\x1b'));
    }

    #[test]
    fn tick_request_has_one_based_step() {
        let request = tick_request("I1", 4, 1, true);
        assert_eq!(
            (
                request.id.as_str(),
                request.version,
                request.step,
                request.ticked
            ),
            ("I1", 4, 1, true)
        );
        assert!(!tick_request("I1", 4, 2, false).ticked);
    }

    #[test]
    fn save_picks_request_keeps_decisions_from_zero_and_free_text() {
        let picks = [
            crate::questions::PickView {
                decision: 0,
                key: "a".into(),
                text: String::new(),
            },
            crate::questions::PickView {
                decision: 2,
                key: "z".into(),
                text: "typed".into(),
            },
        ];
        let request = save_picks_request("Q43", 3, &picks);
        assert_eq!((request.id.as_str(), request.version), ("Q43", 3));
        let sent: Vec<(u32, &str, &str)> = request
            .picks
            .iter()
            .map(|p| (p.decision, p.key.as_str(), p.text.as_str()))
            .collect();
        assert_eq!(sent, [(0, "a", ""), (2, "z", "typed")]);
    }

    #[test]
    fn map_queue_maps_the_listed_picks() {
        let mut item = listed(
            "Q43",
            2,
            false,
            vec![decision("Which?", vec![option("a", false, false)])],
        );
        item.picks = vec![super::Pick {
            decision: 0,
            key: "a".into(),
            text: String::new(),
        }];
        let queue = map_queue(ListQuestionsResponse {
            questions: vec![item],
            waiting: 1,
            dismissed: vec![],
        });
        assert_eq!(
            queue.questions[0].picks,
            [crate::questions::PickView {
                decision: 0,
                key: "a".into(),
                text: String::new()
            }]
        );
    }

    #[test]
    fn a_save_that_breaks_the_pick_rule_reads_as_unavailable() {
        assert_eq!(
            answer_send_result(Err(tonic::Status::invalid_argument("invalid picks"))),
            SendResult::Unavailable
        );
    }

    #[test]
    fn done_and_ignore_map_to_proto_kinds() {
        use crate::questions::AnswerKind;
        assert_eq!(proto_answer_kind(AnswerKind::Done), ProtoAnswerKind::Done);
        assert_eq!(
            proto_answer_kind(AnswerKind::Ignore),
            ProtoAnswerKind::Ignore
        );
    }

    #[test]
    fn a_tick_refusal_of_invalid_step_means_not_waiting() {
        assert_eq!(
            super::tick_send_result(Err(tonic::Status::invalid_argument("invalid step"))),
            SendResult::NotWaiting
        );
        assert_eq!(
            super::tick_send_result(Err(tonic::Status::failed_precondition("version_changed"))),
            SendResult::VersionChanged
        );
        assert_eq!(super::tick_send_result(Ok(())), SendResult::Recorded);
    }
}
