use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

use agentgrasp::{capture, jev, mcp, state};

const USAGE: &str = "usage: agentgrasp hook | agentgrasp finish <dir> <status> | agentgrasp mcp";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("mcp") if args.len() == 1 => run_mcp(),
        Some("hook") if args.len() == 1 => run_hook(),
        Some("finish") if args.len() == 3 => {
            print!("{}", capture::finish(Path::new(&args[1]), &args[2]));
            ExitCode::SUCCESS
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

/// A hook never fails the tool call: on any problem it writes the reason to stderr, prints
/// nothing and exits 0, and the call goes on unchanged.
fn run_hook() -> ExitCode {
    let result = (|| {
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input)?;
        let env = capture::Env {
            state_root: state::root()?,
            binary: std::fs::canonicalize(std::env::current_exe()?)?,
            claude_code_shell: std::env::var("CLAUDE_CODE_SHELL").ok(),
            shell: std::env::var("SHELL").ok(),
        };
        capture::hook(&input, &env)
    })();
    match result {
        Ok(Some(output)) => println!("{output}"),
        Ok(None) => {}
        Err(error) => eprintln!("agentgrasp hook: {error:#}"),
    }
    ExitCode::SUCCESS
}
