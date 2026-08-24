use std::process::ExitCode;

use tmx_agent_dash::terminal::{FaultScenario, run_terminal_fault_harness};

fn main() -> ExitCode {
    let Some(argument) = std::env::args().nth(1) else {
        eprintln!(
            "usage: cargo run --offline --example terminal_fault_harness -- \
             <partial-init|render-failure|panic-cleanup|restoration-failure>"
        );
        return ExitCode::from(2);
    };
    let scenario = match argument.as_str() {
        "partial-init" => FaultScenario::PartialInitialization,
        "render-failure" => FaultScenario::RenderFailure,
        "panic-cleanup" => FaultScenario::PanicCleanup,
        "restoration-failure" => FaultScenario::RestorationFailure,
        _ => {
            eprintln!("unknown terminal fault scenario: {argument}");
            return ExitCode::from(2);
        }
    };

    match run_terminal_fault_harness(scenario) {
        Ok(report) => {
            println!(
                "TMX_FAULT scenario={} outcome=expected cleanup=exactly-once",
                report.scenario().name()
            );
            println!("TMX_FAULT calls={}", report.calls().join(","));
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!(
                "TMX_FAULT scenario={} outcome=unexpected error={error}",
                scenario.name()
            );
            ExitCode::FAILURE
        }
    }
}
