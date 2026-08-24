use std::process::ExitCode;

use tmx_agent_dash::api::AgentStateSource;
use tmx_agent_dash::cli;
use tmx_agent_dash::navigation::TmuxNavigator;
use tmx_agent_dash::runtime;
use tonic::transport::Endpoint;

fn main() -> ExitCode {
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tmx-agent-dash: {error}");
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<(), String> {
    let options = cli::parse_args(std::env::args_os()).map_err(|error| error.to_string())?;
    let endpoint = Endpoint::from_shared(options.endpoint.clone())
        .map_err(|error| format!("invalid Harold endpoint: {error}"))?;
    runtime::run(
        options,
        AgentStateSource::new(endpoint),
        TmuxNavigator::new(),
    )
    .map_err(|error| error.to_string())
}
