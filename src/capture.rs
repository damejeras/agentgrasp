//! Command output capture for Claude Code's Bash tool: the PreToolUse hook rewrites each
//! command so that its output goes to files, and `finish` shows the output or a summary.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::state::{self, Kind};

/// Output and error output together at most this long are shown in full.
pub const SHOWN_BYTES: u64 = 2048;
/// The longest the syntax check of a rewritten command may take.
pub const SYNTAX_CHECK: Duration = Duration::from_secs(2);

/// The process facts the hook depends on.
pub struct Env {
    pub state_root: PathBuf,
    /// The absolute path of the running binary.
    pub binary: PathBuf,
    pub claude_code_shell: Option<String>,
    pub shell: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shell {
    Bash,
    Zsh,
}

/// The shell Claude Code runs commands in, as it picks it: `CLAUDE_CODE_SHELL` when that
/// names an executable bash or zsh, else `$SHELL` when that does. Any other shell is not
/// supported.
fn shell(env: &Env) -> Option<(PathBuf, Shell)> {
    let named = |value: &Option<String>| {
        let path = PathBuf::from(value.as_ref()?);
        let name = path.file_name()?.to_string_lossy().into_owned();
        let kind = if name.contains("zsh") {
            Shell::Zsh
        } else if name.contains("bash") {
            Shell::Bash
        } else {
            return None;
        };
        is_executable(&path).then_some((path, kind))
    };
    named(&env.claude_code_shell).or_else(|| named(&env.shell))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `text` in single quotes, for the shell.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The original command stays on its own lines, as visible text, so Claude Code checks it
/// against the permission rules. The blank line after it stops a trailing backslash from
/// joining the closing `}`. The first `echo` names the capture directory even when the
/// command ends the shell before `finish` runs.
fn rewrite(command: &str, binary: &str, dir: &str) -> String {
    let d = quote(dir);
    format!(
        "echo {}; {{ unset TYPESAFE_API_KEY\n{command}\n\n}} > {} 2> {} < /dev/null; {} finish {d} $?",
        quote(&format!("agentgrasp: {dir}")),
        quote(&format!("{dir}/stdout.log")),
        quote(&format!("{dir}/stderr.log")),
        quote(binary),
    )
}

/// The shell's syntax check without startup files, within `SYNTAX_CHECK`.
fn syntax_ok(shell: &Path, kind: Shell, script: &str) -> bool {
    let mut command = Command::new(shell);
    match kind {
        Shell::Bash => command.args(["--norc", "--noprofile", "-n", "-c", script]),
        Shell::Zsh => command.args(["-f", "-n", "-c", script]),
    };
    let spawned = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else { return false };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() < SYNTAX_CHECK => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn unix_seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Handles the PreToolUse hook. `None`: the call runs unchanged. An error also lets the
/// call run unchanged; the caller writes the reason to stderr.
pub fn hook(input: &[u8], env: &Env) -> Result<Option<Value>> {
    let input: Value = serde_json::from_slice(input).context("the hook input is not JSON")?;
    let tool_input = &input["tool_input"];
    if input["tool_name"] != "Bash" || tool_input["run_in_background"] == true {
        return Ok(None);
    }
    let Some(command) = tool_input["command"].as_str() else {
        return Ok(None);
    };
    let Some((shell_path, kind)) = shell(env) else {
        return Ok(None);
    };
    let binary = env
        .binary
        .to_str()
        .context("the agentgrasp path is not UTF-8")?;
    // The check comes before the capture directory, so a call that runs unchanged leaves no
    // capture behind. The directory's path does not change the syntax: it is quoted.
    if !syntax_ok(
        &shell_path,
        kind,
        &rewrite(command, binary, "/agentgrasp/check"),
    ) {
        bail!(
            "the rewritten command fails the syntax check of {}; the call runs unchanged",
            shell_path.display()
        );
    }
    let label = input["tool_use_id"].as_str().unwrap_or("call");
    let dir = state::allocate(&env.state_root, Kind::Capture, label)?;
    let now = SystemTime::now();
    let metadata = json!({
        "command": command,
        "cwd": input["cwd"],
        "started_at": state::rfc3339(now),
        "started_at_unix": unix_seconds(now),
    });
    state::write_atomic(
        &dir.join("metadata.json"),
        &serde_json::to_vec_pretty(&metadata)?,
    )?;
    let dir_text = dir.to_str().context("the capture directory is not UTF-8")?;
    let rewritten = rewrite(command, binary, dir_text);
    let mut updated = tool_input.clone();
    updated["command"] = json!(rewritten);
    Ok(Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": updated,
        }
    })))
}

/// The last step of a rewritten command: records the result and returns what the agent sees
/// and the status to exit with, which is always `status`, also when the recording fails.
pub fn finish(dir: &Path, status: &str) -> (Vec<u8>, i32) {
    let code: i32 = status.parse().unwrap_or(1);
    let size = |name: &str| std::fs::metadata(dir.join(name)).map_or(0, |m| m.len());
    // The sizes now bound every read, so a log that still grows cannot make finish read
    // without limit.
    let (stdout_bytes, stderr_bytes) = (size("stdout.log"), size("stderr.log"));
    let duration = record_finish(dir, code, stdout_bytes, stderr_bytes)
        .map_err(|error| eprintln!("agentgrasp finish: {error:#}"))
        .ok()
        .flatten();
    let mut shown = Vec::new();
    let in_full = stdout_bytes + stderr_bytes <= SHOWN_BYTES;
    if in_full {
        shown.extend(read_start(&dir.join("stdout.log"), stdout_bytes));
        if stderr_bytes > 0 {
            if !shown.is_empty() && !shown.ends_with(b"\n") {
                shown.push(b'\n');
            }
            shown.extend_from_slice(b"--- stderr\n");
            shown.extend(read_start(&dir.join("stderr.log"), stderr_bytes));
        }
        if !shown.is_empty() && !shown.ends_with(b"\n") {
            shown.push(b'\n');
        }
    }
    let _ = writeln!(shown, "exit_code: {code}");
    let _ = writeln!(
        shown,
        "stdout: {stdout_bytes} bytes {}",
        dir.join("stdout.log").display()
    );
    let _ = writeln!(
        shown,
        "stderr: {stderr_bytes} bytes {}",
        dir.join("stderr.log").display()
    );
    if let Some(duration) = duration {
        let _ = writeln!(shown, "duration_seconds: {duration}");
    }
    if !in_full {
        shown.extend_from_slice(b"output not shown; ask yes/no questions with the agentgrasp ask tool, or read the files\n");
    }
    (shown, code)
}

fn read_start(path: &Path, size: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(size).read_to_end(&mut bytes);
    }
    bytes
}

/// Adds the result to `metadata.json`; returns the duration in seconds when the start is
/// known.
fn record_finish(
    dir: &Path,
    code: i32,
    stdout_bytes: u64,
    stderr_bytes: u64,
) -> Result<Option<f64>> {
    let path = dir.join("metadata.json");
    let mut metadata: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    if !metadata.is_object() {
        bail!("{} is not a JSON object", path.display());
    }
    let now = SystemTime::now();
    let duration = metadata["started_at_unix"]
        .as_f64()
        .map(|started| ((unix_seconds(now) - started) * 10.0).round() / 10.0);
    metadata["exit_code"] = json!(code);
    metadata["ended_at"] = json!(state::rfc3339(now));
    metadata["duration_seconds"] = json!(duration);
    metadata["stdout_bytes"] = json!(stdout_bytes);
    metadata["stderr_bytes"] = json!(stderr_bytes);
    state::write_atomic(&path, &serde_json::to_vec_pretty(&metadata)?)?;
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_program(name: &str) -> Option<String> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| is_executable(p))
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn env(state_root: &Path, shell: Option<String>) -> Env {
        Env {
            state_root: state_root.to_path_buf(),
            binary: PathBuf::from("/opt/my bin/agentgrasp"),
            claude_code_shell: None,
            shell,
        }
    }

    fn pre(command: &str) -> Value {
        json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_use_id": "toolu_01ABC",
            "cwd": "/work",
            "tool_input": {"command": command, "description": "List", "timeout": 5000},
        })
    }

    fn captures(root: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(root.join("captures"))
            .map(|dirs| dirs.map(|d| d.unwrap().path()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("/a b/it's"), r"'/a b/it'\''s'");
    }

    #[test]
    fn the_rewrite_keeps_the_command_as_text_and_every_other_field() {
        let temp = tempfile::tempdir().unwrap();
        let env = env(temp.path(), find_program("bash"));
        let output = hook(pre("ls -la").to_string().as_bytes(), &env)
            .unwrap()
            .unwrap();
        let specific = &output["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PreToolUse");
        assert!(specific.get("permissionDecision").is_none());
        let updated = &specific["updatedInput"];
        assert_eq!(updated["description"], "List");
        assert_eq!(updated["timeout"], 5000);
        let dir = captures(temp.path()).pop().unwrap();
        let d = dir.display();
        assert_eq!(
            updated["command"],
            format!(
                "echo 'agentgrasp: {d}'; {{ unset TYPESAFE_API_KEY\nls -la\n\n}} > '{d}/stdout.log' 2> '{d}/stderr.log' < /dev/null; '/opt/my bin/agentgrasp' finish '{d}' $?"
            )
        );
        let metadata: Value =
            serde_json::from_slice(&std::fs::read(dir.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["command"], "ls -la");
        assert_eq!(metadata["cwd"], "/work");
        assert!(metadata["started_at"].is_string());
    }

    #[test]
    fn calls_that_pass_through() {
        let temp = tempfile::tempdir().unwrap();
        let mut background = pre("sleep 9");
        background["tool_input"]["run_in_background"] = json!(true);
        let mut other_tool = pre("x");
        other_tool["tool_name"] = json!("Read");
        let bash = env(temp.path(), find_program("bash"));
        for input in [background, other_tool] {
            assert!(
                hook(input.to_string().as_bytes(), &bash).unwrap().is_none(),
                "{input}"
            );
        }
        let fish = env(temp.path(), Some("/usr/bin/fish".into()));
        assert!(
            hook(pre("ls").to_string().as_bytes(), &fish)
                .unwrap()
                .is_none()
        );
        assert!(captures(temp.path()).is_empty());
        for shell in [find_program("bash"), find_program("zsh")]
            .into_iter()
            .flatten()
        {
            let env = env(temp.path(), Some(shell.clone()));
            // A failed check is an error: the reason goes to stderr, the call runs unchanged.
            assert!(
                hook(pre("cat <<EOF\nno end").to_string().as_bytes(), &env).is_err(),
                "{shell}"
            );
            assert!(
                hook(pre("cat <<EOF\nend\nEOF").to_string().as_bytes(), &env)
                    .unwrap()
                    .is_some(),
                "{shell}"
            );
        }
    }

    fn capture_with(stdout: &[u8], stderr: &[u8]) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("stdout.log"), stdout).unwrap();
        std::fs::write(temp.path().join("stderr.log"), stderr).unwrap();
        let started = unix_seconds(SystemTime::now()) - 6.4;
        std::fs::write(
            temp.path().join("metadata.json"),
            json!({"started_at_unix": started}).to_string(),
        )
        .unwrap();
        temp
    }

    #[test]
    fn short_output_is_shown_with_the_summary() {
        let temp = capture_with(b"out\x1b[31m\n", b"err\n");
        let (shown, code) = finish(temp.path(), "3");
        assert_eq!(code, 3);
        let shown = String::from_utf8(shown).unwrap();
        let d = temp.path().display();
        assert!(
            shown.starts_with(&format!(
                "out\x1b[31m\n--- stderr\nerr\nexit_code: 3\nstdout: 9 bytes {d}/stdout.log\nstderr: 4 bytes {d}/stderr.log\nduration_seconds: "
            )),
            "{shown}"
        );
        let metadata: Value =
            serde_json::from_slice(&std::fs::read(temp.path().join("metadata.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["exit_code"], 3);
        assert_eq!(metadata["stdout_bytes"], 9);
        assert!(metadata["duration_seconds"].as_f64().unwrap() >= 6.4);
    }

    #[test]
    fn output_of_2048_bytes_is_shown_and_2049_is_not() {
        for (stdout, stderr, in_full) in [
            (2048, 0, true),
            (2040, 8, true),
            (2049, 0, false),
            (1, 2048, false),
        ] {
            let temp = capture_with(&vec![b'#'; stdout], &vec![b'e'; stderr]);
            let shown = String::from_utf8(finish(temp.path(), "0").0).unwrap();
            assert_eq!(
                shown.contains("output not shown"),
                !in_full,
                "{stdout}+{stderr}"
            );
            assert_eq!(shown.contains('#'), in_full);
        }
    }

    #[test]
    fn a_log_that_grows_is_read_only_to_its_size() {
        let temp = capture_with(b"12345", b"");
        // finish reads the sizes before it reads the logs; a writer could add bytes between.
        let (shown, _) = finish(temp.path(), "0");
        assert!(
            String::from_utf8(shown)
                .unwrap()
                .starts_with("12345\nexit_code")
        );
        assert_eq!(read_start(&temp.path().join("stdout.log"), 3), b"123");
    }

    #[test]
    fn finish_keeps_the_status_when_its_work_fails() {
        let temp = tempfile::tempdir().unwrap();
        for metadata in [None, Some("[]"), Some("not json")] {
            if let Some(text) = metadata {
                std::fs::write(temp.path().join("metadata.json"), text).unwrap();
            }
            let (shown, code) = finish(&temp.path().join("gone"), "7");
            assert_eq!(code, 7);
            assert!(String::from_utf8(shown).unwrap().contains("exit_code: 7"));
            let (_, code) = finish(temp.path(), "7");
            assert_eq!(code, 7);
        }
    }
}
