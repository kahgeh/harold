mod settings;
mod telemetry;

use settings::{get_settings, init_settings};
use telemetry::init_telemetry;
use tonic::{Request, Response, Status, transport::Server};
use tracing::info;

pub mod harold {
    tonic::include_proto!("harold");
}

use harold::harold_server::{Harold, HaroldServer};
use harold::{TurnCompleteRequest, TurnCompleteResponse};

struct HaroldService;

#[tonic::async_trait]
impl Harold for HaroldService {
    async fn turn_complete(
        &self,
        request: Request<TurnCompleteRequest>,
    ) -> Result<Response<TurnCompleteResponse>, Status> {
        let req = request.into_inner();
        // transcript omitted from log — can be large
        info!(
            pane_id = %req.pane_id,
            pane_label = %req.pane_label,
            git_context = %req.git_context,
            "turn complete received"
        );
        Ok(Response::new(TurnCompleteResponse { accepted: true }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = settings::Settings::load()?;
    init_settings(settings);

    let cfg = get_settings();
    init_telemetry(&cfg.log.level);

    let addr = cfg.grpc.addr()?;
    info!(address = %addr, "Harold listening");

    Server::builder()
        .add_service(HaroldServer::new(HaroldService))
        .serve(addr)
        .await?;

    Ok(())
}
