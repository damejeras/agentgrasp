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

/// The allow rules the README gives, plus `extra`.
fn settings(extra: Value) -> Value {
    let mut settings = json!({
        "permissions": {
            "allow": [
                "Bash(seq:*)",
                "Bash(unset TYPESAFE_API_KEY)",
                format!("Bash({} finish:*)", binary().display()),
            ],
        },
    });
    let object = settings.as_object_mut().unwrap();
    for (key, value) in extra.as_object().unwrap() {
        match (object.get_mut(key), value) {
            (Some(Value::Object(old)), Value::Object(new)) => {
                for (k, v) in new {
                    match (old.get_mut(k), v) {
                        (Some(Value::Array(a)), Value::Array(b)) => a.extend(b.clone()),
                        _ => {
                            old.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            _ => {
                object.insert(key.clone(), value.clone());
            }
        }
    }
    settings
}

fn claude(project: &Project, settings: &Value, command: &str) -> Run {
    let settings_path = project.dir.parent().unwrap().join("settings.json");
    std::fs::write(&settings_path, settings.to_string()).unwrap();
    let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugin");
    let path = format!(
        "{}:{}",
        binary().parent().unwrap().display(),
        std::env::var("PATH").unwrap()
    );
    let prompt = format!(
        "Call the Bash tool exactly once with this exact command and nothing else, then reply DONE: {command}"
    );
    let output = Command::new("claude")
        .args(["-p", "--setting-sources", "", "--settings"])
        .arg(&settings_path)
        .arg("--plugin-dir")
        .arg(&plugin)
        .args([
            "--strict-mcp-config",
            "--permission-mode",
            "default",
            "--model",
            "haiku",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
            &prompt,
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

#[test]
#[ignore = "runs the real Claude Code"]
fn an_allowed_command_runs_without_a_prompt_and_long_output_is_summarized() {
    let project = project();
    let run = claude(&project, &settings(json!({})), "seq 1 2000");
    assert!(run.denials.is_empty(), "{:?}", run.denials);
    let (text, is_error) = &run.results[0];
    assert!(!is_error);
    assert!(text.starts_with("exit_code: 0\noutput: "), "{text}");
    assert!(text.contains("output not shown"));
    assert!(!text.contains("1999"));
}

#[test]
#[ignore = "runs the real Claude Code"]
fn short_output_is_shown_with_the_summary() {
    let project = project();
    let run = claude(&project, &settings(json!({})), "seq 1 3");
    assert!(run.denials.is_empty());
    assert!(
        run.results[0].0.starts_with("1\n2\n3\nexit_code: 0\n"),
        "{}",
        run.results[0].0
    );
}

#[test]
#[ignore = "runs the real Claude Code"]
fn a_deny_rule_still_blocks_the_rewritten_command() {
    let project = project();
    let settings = settings(json!({"permissions": {"deny": ["Bash(touch:*)"]}}));
    let run = claude(&project, &settings, "touch marker.txt");
    assert!(!run.denials.is_empty(), "the call is denied");
    assert!(
        run.results[0].0.contains("has been denied"),
        "{}",
        run.results[0].0
    );
    assert!(!project.dir.join("marker.txt").exists());
}

#[test]
#[ignore = "runs the real Claude Code"]
fn the_hook_does_not_approve_a_command_the_rules_do_not_allow() {
    let project = project();
    let run = claude(&project, &settings(json!({})), "touch marker.txt");
    assert!(!run.denials.is_empty(), "the call still needs approval");
    assert!(!project.dir.join("marker.txt").exists());
}

#[test]
#[ignore = "runs the real Claude Code"]
fn the_sandbox_with_the_readme_setting_captures_a_failing_command() {
    let project = project();
    let sandbox =
        json!({"sandbox": {"enabled": true, "filesystem": {"allowWrite": [project.state]}}});
    let run = claude(&project, &settings(sandbox), "seq 1 2000; false");
    assert!(run.denials.is_empty(), "{:?}", run.denials);
    let (text, is_error) = &run.results[0];
    assert!(
        !is_error,
        "the call ends 0, so PostToolUse replaces the output"
    );
    assert!(text.starts_with("exit_code: 1\n"), "{text}");
    let captures = project.state.join("agentgrasp/captures");
    let dir = std::fs::read_dir(captures)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let metadata: Value =
        serde_json::from_slice(&std::fs::read(dir.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(metadata["exit_code"], 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("output.log"))
            .unwrap()
            .lines()
            .count(),
        2000
    );
}
