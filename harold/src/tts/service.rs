//! `harold.tts.v1.TtsService`: the gRPC face of the TTS server. It trusts local callers, as
//! Harold's other services do (ADR 0002), and refuses a caller that is not on a loopback
//! address with `PermissionDenied` `not_local`.

use super::models::{AddRefusal, InfoState, ModelInfo, Phase, RemoveRefusal, SetDefaultRefusal};
use super::{Caller, SpeakFailure, TtsServer};
use harold_api::tts::v1::tts_service_server::TtsService;
use harold_api::tts::v1::{
    AddModelRequest, AddModelResponse, AudioChunk, ListModelsRequest, ListModelsResponse, Model,
    ModelState, RemoveModelRequest, RemoveModelResponse, SetDefaultRequest, SetDefaultResponse,
    SpeakRequest,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

/// How many pieces of audio wait for the caller to read them.
const PIECES_QUEUED: usize = 16;

pub(crate) struct TtsApi {
    server: Arc<TtsServer>,
}

impl TtsApi {
    pub(crate) fn new(server: Arc<TtsServer>) -> Self {
        Self { server }
    }
}

/// Refuses an address that is not a loopback address, and a call with no address.
pub(crate) fn require_local(remote: Option<SocketAddr>) -> Result<(), Status> {
    match remote {
        Some(address) if address.ip().is_loopback() => Ok(()),
        _ => Err(Status::permission_denied("not_local")),
    }
}

fn speak_status(failure: SpeakFailure) -> Status {
    let word = failure.as_str();
    match failure {
        SpeakFailure::InvalidText | SpeakFailure::UnknownModel | SpeakFailure::UnknownVoice => {
            Status::invalid_argument(word)
        }
        SpeakFailure::ModelDownloading
        | SpeakFailure::ModelMissing
        | SpeakFailure::WorkerFailed
        | SpeakFailure::TimedOut => Status::unavailable(word),
    }
}

fn model_state(state: InfoState) -> ModelState {
    match state {
        InfoState::Ready => ModelState::Ready,
        InfoState::Downloading => ModelState::Downloading,
        InfoState::Failed => ModelState::Failed,
        InfoState::WaitingForApproval => ModelState::WaitingForApproval,
    }
}

fn phase_state(phase: Phase) -> ModelState {
    match phase {
        Phase::Ready => ModelState::Ready,
        Phase::Failed(_) => ModelState::Failed,
        Phase::WaitingForApproval => ModelState::WaitingForApproval,
        Phase::Checking | Phase::Waiting | Phase::Downloading { .. } => ModelState::Downloading,
    }
}

fn model_of(info: ModelInfo) -> Model {
    Model {
        id: info.id,
        source: info.source.to_string(),
        revision: info.revision,
        worker: info.worker.as_str().to_string(),
        voices: info.voices,
        state: model_state(info.state).into(),
        reason: info.reason,
        size_bytes: info.size_bytes,
        is_default: info.is_default,
        default_voice: info.default_voice,
    }
}

#[tonic::async_trait]
impl TtsService for TtsApi {
    type SpeakStream = ReceiverStream<Result<AudioChunk, Status>>;

    async fn speak(
        &self,
        request: Request<SpeakRequest>,
    ) -> Result<Response<Self::SpeakStream>, Status> {
        require_local(request.remote_addr())?;
        let request = request.into_inner();
        // A caller of the API is the desk's: it plays the line, or hears it from the CLI.
        let mut speech = self
            .server
            .speak(Caller::Desk, &request.text, &request.model, &request.voice)
            .await
            .map_err(speak_status)?;
        let (sender, receiver) = mpsc::channel(PIECES_QUEUED);
        tokio::spawn(async move {
            let mut rate = speech.sample_rate;
            loop {
                let piece = match speech.next().await {
                    Ok(Some(piece)) => piece,
                    Ok(None) => return,
                    Err(failure) => {
                        let _ = sender.send(Err(speak_status(failure))).await;
                        return;
                    }
                };
                let chunk = AudioChunk {
                    sample_rate: std::mem::take(&mut rate),
                    pcm: piece,
                };
                if sender.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn list_models(
        &self,
        request: Request<ListModelsRequest>,
    ) -> Result<Response<ListModelsResponse>, Status> {
        require_local(request.remote_addr())?;
        let models = self
            .server
            .registry()
            .list()
            .into_iter()
            .map(model_of)
            .collect();
        Ok(Response::new(ListModelsResponse { models }))
    }

    async fn add_model(
        &self,
        request: Request<AddModelRequest>,
    ) -> Result<Response<AddModelResponse>, Status> {
        require_local(request.remote_addr())?;
        let request = request.into_inner();
        let added = self
            .server
            .registry()
            .add_model(&request.source, &request.repo, &request.revision)
            .await;
        match added {
            Ok((id, phase)) => Ok(Response::new(AddModelResponse {
                id,
                state: phase_state(phase).into(),
            })),
            Err(AddRefusal::UnsupportedSource) => {
                Err(Status::invalid_argument("unsupported_source"))
            }
            Err(AddRefusal::InvalidRepo) => Err(Status::invalid_argument("invalid_repo")),
            Err(AddRefusal::InvalidRevision) => Err(Status::invalid_argument("invalid_revision")),
            Err(AddRefusal::NotApproved) => Err(Status::permission_denied("not_approved")),
            Err(AddRefusal::UnsupportedWorker) => {
                Err(Status::failed_precondition("unsupported_worker"))
            }
            Err(AddRefusal::UnsafeFormat) => Err(Status::failed_precondition("unsafe_format")),
            Err(AddRefusal::Unavailable(reason)) => Err(Status::unavailable(reason)),
        }
    }

    async fn remove_model(
        &self,
        request: Request<RemoveModelRequest>,
    ) -> Result<Response<RemoveModelResponse>, Status> {
        require_local(request.remote_addr())?;
        let id = request.into_inner().id;
        match self.server.registry().remove_model(&id) {
            Ok(()) => {
                // A worker that holds the removed model is stopped; the next request loads
                // the one it asks for.
                self.server.supervisor().stop().await;
                Ok(Response::new(RemoveModelResponse {}))
            }
            Err(RemoveRefusal::DefaultModel) => Err(Status::failed_precondition("default_model")),
            Err(RemoveRefusal::UnknownModel) => Err(Status::not_found("unknown_model")),
        }
    }

    async fn set_default(
        &self,
        request: Request<SetDefaultRequest>,
    ) -> Result<Response<SetDefaultResponse>, Status> {
        require_local(request.remote_addr())?;
        let request = request.into_inner();
        match self
            .server
            .registry()
            .set_default(&request.model, &request.voice)
            .await
        {
            Ok(()) => Ok(Response::new(SetDefaultResponse {})),
            Err(SetDefaultRefusal::UnknownModel) => Err(Status::not_found("unknown_model")),
            Err(SetDefaultRefusal::UnknownVoice) => Err(Status::invalid_argument("unknown_voice")),
            Err(SetDefaultRefusal::NotReady) => Err(Status::failed_precondition("model_not_ready")),
            Err(SetDefaultRefusal::Unavailable) => Err(Status::unavailable("store_unavailable")),
        }
    }
}
