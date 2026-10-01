use std::process::ExitCode;

const USAGE: &str = "usage: agentgrasp hook | agentgrasp finish <dir> <status> | agentgrasp mcp";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("hook") | Some("finish") | Some("mcp") => {
            eprintln!("agentgrasp: {} is not built yet", args[0]);
            ExitCode::FAILURE
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}
