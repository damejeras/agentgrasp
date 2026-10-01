//! Checks against the real Claude Code, with the plugin in `plugin/` and this build of
//! agentgrasp on PATH. They need a signed-in `claude` and use its plan, so they are ignored by
//! default. Run them before a release:
//!
//! ```sh
//! cargo build && cargo test --test claude -- --ignored --test-threads 1
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

struct Run {
    results: Vec<(String, bool)>,
    denials: Vec<Value>,
}

struct Project {
    _temp: tempfile::TempDir,
    dir: PathBuf,
    state: PathBuf,
}

fn binary() -> PathBuf {
    std::fs::canonicalize(env!("CARGO_BIN_EXE_agentgrasp")).unwrap()
}

fn project() -> Project {
    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::create_dir_all(base.join("project")).unwrap();
    Project {
        dir: base.join("project"),
        state: base.join("state"),
        _temp: temp,
    }
}

const DEFAULT: &str = "default";
const BYPASS: &str = "bypassPermissions";

fn bash_prompt(command: &str) -> String {
    format!(
        "Call the Bash tool exactly once with this exact command and nothing else, then reply DONE: {command}"
    )
}

/// Runs Claude Code on `prompt` in permission `mode`, with extra arguments and environment.
fn claude(
    project: &Project,
    settings: &Value,
    mode: &str,
    prompt: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Run {
    let settings_path = project.dir.parent().unwrap().join("settings.json");
    std::fs::write(&settings_path, settings.to_string()).unwrap();
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin");
    let path = format!(
        "{}:{}",
        binary().parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let output = Command::new("claude")
        .args(args)
        .envs(envs.iter().copied())
        .args(["-p", "--setting-sources", "", "--settings"])
        .arg(&settings_path)
        .arg("--plugin-dir")
        .arg(&plugin)
        .args([
            "--strict-mcp-config",
            "--permission-mode",
            mode,
            "--model",
            "haiku",
        ])
        .args([
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            prompt,
        ])
        .current_dir(&project.dir)
        .env("PATH", path)
        .env("XDG_STATE_HOME", &project.state)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("claude runs");
    let mut run = Run {
        results: Vec::new(),
        denials: Vec::new(),
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event["type"] == "user" {
            for content in event["message"]["content"].as_array().into_iter().flatten() {
                if content["type"] == "tool_result" {
                    let text = match &content["content"] {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    run.results.push((text, content["is_error"] == true));
                }
            }
        }
        if event["type"] == "result" {
            run.denials = event["permission_denials"]
                .as_array()
                .cloned()
                .unwrap_or_default();
        }
    }
    run
}

fn bash(project: &Project, settings: &Value, mode: &str, command: &str) -> Run {
    claude(project, settings, mode, &bash_prompt(command), &[], &[])
}

fn only_capture(project: &Project) -> PathBuf {
    let captures = project.state.join("agentgrasp/captures");
    std::fs::read_dir(captures)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

#[test]
#[ignore = "runs the real Claude Code"]
fn in_default_mode_a_wrapped_command_asks_for_approval_even_when_allowed() {
    let project = project();
    let settings = json!({"permissions": {"allow": ["Bash(seq:*)"]}});
    let run = bash(&project, &settings, DEFAULT, "seq 1 3");
    assert!(
        !run.denials.is_empty(),
        "Claude Code asks about every brace group"
    );
    assert!(
        run.results[0].0.contains("compound_statement"),
        "{}",
        run.results[0].0
    );
}

#[test]
#[ignore = "runs the real Claude Code"]
fn a_deny_rule_still_blocks_the_rewritten_command() {
    for mode in [DEFAULT, BYPASS] {
        let project = project();
        let settings = json!({"permissions": {"deny": ["Bash(touch:*)"]}});
        let run = bash(&project, &settings, mode, "touch marker.txt");
        assert!(!run.denials.is_empty(), "{mode}: the call is denied");
        assert!(
            run.results[0].0.contains("has been denied"),
            "{mode}: {}",
            run.results[0].0
        );
        assert!(!project.dir.join("marker.txt").exists());
    }
}

#[test]
#[ignore = "runs the real Claude Code"]
fn in_bypass_mode_output_is_captured_and_summarized() {
    let project = project();
    let run = bash(&project, &json!({}), BYPASS, "seq 1 3");
    assert!(run.denials.is_empty(), "{:?}", run.denials);
    let (text, is_error) = &run.results[0];
    assert!(!is_error);
    let dir = only_capture(&project);
    let expected = format!("agentgrasp: {}\n1\n2\n3\nexit_code: 0\n", dir.display());
    assert!(text.starts_with(&expected), "{text}");

    let project = self::project();
    let run = bash(&project, &json!({}), BYPASS, "seq 1 2000");
    let (text, _) = &run.results[0];
    assert!(text.contains("output not shown"), "{text}");
    assert!(!text.contains("1999"));
}

#[test]
#[ignore = "runs the real Claude Code"]
fn the_sandbox_with_the_readme_setting_captures_a_failing_command() {
    let project = project();
    let settings =
        json!({"sandbox": {"enabled": true, "filesystem": {"allowWrite": [project.state]}}});
    let run = bash(&project, &settings, BYPASS, "seq 1 2000; false");
    assert!(run.denials.is_empty(), "{:?}", run.denials);
    let (text, is_error) = &run.results[0];
    assert!(is_error, "Claude Code reports the failure");
    assert!(text.contains("exit_code: 1\n"), "{text}");
    let dir = only_capture(&project);
    let metadata: Value =
        serde_json::from_slice(&std::fs::read(dir.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(metadata["exit_code"], 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("stdout.log"))
            .unwrap()
            .lines()
            .count(),
        2000
    );
}

#[test]
#[ignore = "runs the real Claude Code"]
fn claude_code_shell_picks_the_shell_that_is_checked() {
    let project = project();
    let bash_path = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join("bash"))
        .find(|p| p.is_file())
        .expect("bash on PATH")
        .to_string_lossy()
        .into_owned();
    // /bin/sh is not a shell the hook supports, so without the override the call is not
    // rewritten; with CLAUDE_CODE_SHELL naming bash, it is.
    let plain = claude(
        &project,
        &json!({}),
        BYPASS,
        &bash_prompt("seq 1 3"),
        &[],
        &[("SHELL", "/bin/sh")],
    );
    assert!(
        !plain.results[0].0.starts_with("agentgrasp: "),
        "{}",
        plain.results[0].0
    );
    let envs = [
        ("CLAUDE_CODE_SHELL", bash_path.as_str()),
        ("SHELL", "/bin/sh"),
    ];
    let run = claude(
        &project,
        &json!({}),
        BYPASS,
        &bash_prompt("seq 1 3"),
        &[],
        &envs,
    );
    assert!(run.denials.is_empty(), "{:?}", run.denials);
    assert!(
        run.results[0].0.starts_with("agentgrasp: "),
        "{}",
        run.results[0].0
    );
}

#[test]
#[ignore = "runs the real Claude Code"]
fn an_added_directory_is_an_mcp_root() {
    let project = project();
    let added = project.dir.parent().unwrap().join("added");
    std::fs::create_dir_all(&added).unwrap();
    std::fs::write(added.join("notes.txt"), "notes").unwrap();
    let outside = project.dir.parent().unwrap().join("outside.txt");
    std::fs::write(&outside, "outside").unwrap();
    // --strict-mcp-config leaves out plugin servers, so the server is given here.
    let servers =
        json!({"mcpServers": {"agentgrasp": {"command": binary(), "args": ["mcp"]}}}).to_string();
    let settings = json!({"permissions": {"allow": ["mcp__agentgrasp__ask"]}});
    let prompt = format!(
        "Call the agentgrasp ask MCP tool twice, then reply DONE. First with paths [\"{}\"] and questions [\"Is it text?\"]. Then with paths [\"{}\"] and questions [\"Is it text?\"].",
        added.join("notes.txt").display(),
        outside.display()
    );
    let added_arg = added.to_string_lossy().into_owned();
    // Without a key, an allowed path gives provider_unavailable and a refused one invalid_input.
    let args = [
        "--add-dir",
        added_arg.as_str(),
        "--mcp-config",
        servers.as_str(),
    ];
    let run = claude(
        &project,
        &settings,
        DEFAULT,
        &prompt,
        &args,
        &[("TYPESAFE_API_KEY", "")],
    );
    let texts: Vec<&String> = run
        .results
        .iter()
        .map(|(text, _)| text)
        .filter(|t| t.contains("evaluation_status"))
        .collect();
    assert_eq!(texts.len(), 2, "{:?}", run.results);
    assert!(texts[0].contains("provider_unavailable"), "{}", texts[0]);
    assert!(texts[1].contains("invalid_input"), "{}", texts[1]);
}
