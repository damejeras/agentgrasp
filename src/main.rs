use std::process::ExitCode;

use agentgrasp::{jev, mcp, state};

const USAGE: &str = "usage: agentgrasp hook | agentgrasp finish <dir> <status> | agentgrasp mcp";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp") if args.len() == 1 => run_mcp(),
        Some("hook") | Some("finish") => {
            eprintln!("agentgrasp: {} is not built yet", args[0]);
            ExitCode::FAILURE
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run_mcp() -> ExitCode {
    let state_root = match state::root() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("agentgrasp mcp: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    let config = mcp::Config {
        endpoint: jev::ENDPOINT.to_string(),
        key: std::env::var("TYPESAFE_API_KEY")
            .ok()
            .filter(|key| !key.is_empty()),
        state_root,
    };
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime starts");
    match runtime.block_on(mcp::serve(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agentgrasp mcp: {error:#}");
            ExitCode::FAILURE
        }
    }
}
