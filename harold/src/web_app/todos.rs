//! The todo page: the question tool's todos, read and answered through its gRPC service.

use harold_api::tools::questions::v1::question_service_client::QuestionServiceClient;
use tonic::transport::{Channel, Endpoint};

use crate::settings::GrpcSettings;

/// The question tool's service on Harold's own gRPC address.
#[derive(Clone)]
pub(crate) struct Questions {
    client: QuestionServiceClient<Channel>,
}

impl Questions {
    /// A client that connects at its first call, so a tool that did not start fails that
    /// call and not the web app's start.
    pub(crate) fn lazy(grpc: &GrpcSettings) -> Self {
        let channel = Endpoint::from_shared(format!("http://{}:{}", grpc.host, grpc.port))
            .expect("host and port make a URI")
            .connect_lazy();
        Self {
            client: QuestionServiceClient::new(channel),
        }
    }
}
