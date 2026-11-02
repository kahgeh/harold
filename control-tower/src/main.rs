use std::process::ExitCode;

use control_tower::api::AgentStateSource;
use control_tower::cli;
use control_tower::navigation::TmuxNavigator;
use control_tower::remote;
use control_tower::runtime;
use tonic::transport::Endpoint;

fn main() -> ExitCode {
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("control-tower: {error}");
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<(), String> {
    let options = cli::parse_args(std::env::args_os()).map_err(|error| error.to_string())?;
    let endpoint = Endpoint::from_shared(options.endpoint.clone())
        .map_err(|error| format!("invalid Harold endpoint: {error}"))?;
    let source = AgentStateSource::new(endpoint);
    let navigator = TmuxNavigator::new();
    if options.remote {
        return remote::run(options, source, navigator).map_err(|error| error.to_string());
    }
    runtime::run(options, source, navigator).map_err(|error| error.to_string())
}
