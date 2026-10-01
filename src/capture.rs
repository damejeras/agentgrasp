//! Command output capture for Claude Code's Bash tool, as two hook events and one command.
//!
//! PreToolUse rewrites the command without grouping it, because Claude Code's permission
//! check asks about every brace group, subshell or `exec`:
//!
//! ```sh
//! unset TYPESAFE_API_KEY
//! <original command>
//!
//! /abs/agentgrasp finish '<capture dir>' "exit:$?"
//! ```
//!
//! `finish` records the exit status and ends the output with a footer line. It exits 0, so
//! Claude Code runs PostToolUse, the one event that can replace Bash output. PostToolUse saves
//! the output Claude Code captured in the capture directory and replaces long output with a
//! summary.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::state::{self, Kind};

/// Output and error output together at most this long are shown in full.
pub const SHOWN_BYTES: usize = 2048;
/// The longest the syntax check of a rewritten command may take.
pub const SYNTAX_CHECK: Duration = Duration::from_secs(2);
/// Claude Code cuts the `stdout` it gives hooks at this many characters and keeps the full
/// output in a file.
pub const CLAUDE_STDOUT_CHARS: usize = 30_000;
const FOOTER: &str = "agentgrasp: capture ";

/// The process facts a hook depends on.
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
/// names a bash or zsh, else `$SHELL` when that does. Any other shell is not supported.
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

/// An executable `name` on `PATH` or in the usual directories, as Claude Code looks for one.
fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(
            ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"]
                .iter()
                .map(PathBuf::from),
        )
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `text` in single quotes, for the shell.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The binary is written unquoted: Claude Code matches allow rules on the command name as
/// written, and a quoted name does not match. So only plain path characters are accepted.
fn plain_path(path: &Path) -> Option<&str> {
    let text = path.to_str()?;
    text.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b))
        .then_some(text)
}

fn rewrite(command: &str, binary: &str, dir: &str) -> String {
    format!(
        "unset TYPESAFE_API_KEY\n{command}\n\n{binary} finish {} \"exit:$?\"",
        quote(dir)
    )
}

/// Runs a syntax check without startup files, within `SYNTAX_CHECK`: its exit status and
/// what it wrote to stderr.
fn check(shell: &Path, kind: Shell, script: &str) -> Option<(bool, String)> {
    let mut command = Command::new(shell);
    match kind {
        Shell::Bash => command.args(["--norc", "--noprofile", "-n", "-c", script]),
        Shell::Zsh => command.args(["-f", "-n", "-c", script]),
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stderr = child.stderr.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some((status.success(), reader.join().unwrap_or_default())),
            Ok(None) if started.elapsed() < SYNTAX_CHECK => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// True when the rewritten command parses with the line of `finish` as a command of its own.
/// An unclosed heredoc in the original would take that line as text: bash warns about it
/// and zsh does not, so for zsh bash must parse the command cleanly too.
fn syntax_ok(shell: &Path, kind: Shell, script: &str) -> bool {
    match kind {
        Shell::Bash => {
            check(shell, kind, script).is_some_and(|(ok, stderr)| ok && stderr.is_empty())
        }
        // A command that bash cannot parse cleanly passes through: zsh syntax could hide an
        // unclosed heredoc from bash's check.
        Shell::Zsh => {
            let zsh_ok = check(shell, kind, script).is_some_and(|(ok, _)| ok);
            zsh_ok
                && find_program("bash").is_some_and(|bash| {
                    check(&bash, Shell::Bash, script)
                        .is_some_and(|(ok, stderr)| ok && stderr.is_empty())
                })
        }
    }
}

/// Handles one hook event. `None`: the call goes on unchanged.
pub fn hook(input: &[u8], env: &Env) -> Result<Option<Value>> {
    let input: Value = serde_json::from_slice(input).context("the hook input is not JSON")?;
    if input["tool_name"] != "Bash" {
        return Ok(None);
    }
    match input["hook_event_name"].as_str() {
        Some("PreToolUse") => pre_tool_use(&input, env),
        Some("PostToolUse") => post_tool_use(&input, env),
        _ => Ok(None),
    }
}

fn pre_tool_use(input: &Value, env: &Env) -> Result<Option<Value>> {
    let tool_input = &input["tool_input"];
    if tool_input["run_in_background"] == true {
        return Ok(None);
    }
    let Some(command) = tool_input["command"].as_str() else {
        return Ok(None);
    };
    let Some(tool_use_id) = input["tool_use_id"].as_str() else {
        bail!("the hook input has no tool_use_id")
    };
    let Some((shell_path, kind)) = shell(env) else {
        return Ok(None);
    };
    let Some(binary) = plain_path(&env.binary) else {
        bail!("the agentgrasp path has characters an allow rule cannot match unquoted");
    };
    if !syntax_ok(
        &shell_path,
        kind,
        &rewrite(command, binary, "/syntax/check"),
    ) {
        return Ok(None);
    }
    let dir = state::allocate(&env.state_root, Kind::Capture, tool_use_id)?;
    let metadata = json!({
        "command": command,
        "cwd": input["cwd"],
        "started_at": state::rfc3339(SystemTime::now()),
        "started_at_unix": unix_seconds(SystemTime::now()),
        "tool_use_id": tool_use_id,
        "shell": shell_path,
    });
    state::write_atomic(
        &dir.join("metadata.json"),
        &serde_json::to_vec_pretty(&metadata)?,
    )?;
    let dir_text = dir.to_str().context("the capture directory is not UTF-8")?;
    let mut updated = tool_input.clone();
    updated["command"] = json!(rewrite(command, binary, dir_text));
    Ok(Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": updated,
        }
    })))
}

fn unix_seconds(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// The last step of a rewritten command: records the exit status and ends the output with
/// the footer. It always exits 0, so that Claude Code hands the output to PostToolUse.
pub fn finish(dir: &Path, status: &str) -> String {
    let code: Option<i32> = status.strip_prefix("exit:").and_then(|c| c.parse().ok());
    if let Err(error) = record_finish(dir, code) {
        eprintln!("agentgrasp finish: {error:#}");
    }
    let shown = code.map_or("unknown".to_string(), |c| c.to_string());
    format!("\n{FOOTER}{} exit {shown}\n", dir.display())
}

/// The capture's metadata object.
fn read_metadata(path: &Path) -> Result<Value> {
    let metadata: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if !metadata.is_object() {
        bail!("{} is not a JSON object", path.display());
    }
    Ok(metadata)
}

fn record_finish(dir: &Path, code: Option<i32>) -> Result<()> {
    let path = dir.join("metadata.json");
    let mut metadata = read_metadata(&path)?;
    let now = SystemTime::now();
    let started = metadata["started_at_unix"]
        .as_f64()
        .unwrap_or_else(|| unix_seconds(now));
    metadata["exit_code"] = json!(code);
    metadata["ended_at"] = json!(state::rfc3339(now));
    metadata["duration_seconds"] = json!(((unix_seconds(now) - started) * 10.0).round() / 10.0);
    state::write_atomic(&path, &serde_json::to_vec_pretty(&metadata)?)
}

/// The footer at the end of `text`: where it starts, the capture directory and the status.
fn footer(text: &[u8]) -> Option<(usize, PathBuf, Option<i32>)> {
    let tail_start = text.len().saturating_sub(8192);
    let tail = &text[tail_start..];
    let at = tail
        .windows(FOOTER.len())
        .rposition(|w| w == FOOTER.as_bytes())?;
    let line = std::str::from_utf8(&tail[at + FOOTER.len()..])
        .ok()?
        .trim_end();
    if line.contains('\n') {
        return None;
    }
    let (dir, status) = line.rsplit_once(" exit ")?;
    let start = tail_start + at;
    // finish prints a newline before the footer; it is part of the footer, not the output.
    let start = if start > 0 && text[start - 1] == b'\n' {
        start - 1
    } else {
        start
    };
    Some((start, PathBuf::from(dir), status.parse().ok()))
}

/// The capture directory of a tool call, from the footer or from the call's id.
fn capture_dir(captures: &Path, tool_use_id: &str, found: Option<&PathBuf>) -> Option<PathBuf> {
    let suffix: String = format!(
        "-{}",
        tool_use_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            })
            .collect::<String>()
    );
    let belongs = |dir: &Path| {
        dir.parent() == Some(captures)
            && dir
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&suffix))
            && dir.join("metadata.json").is_file()
    };
    if let Some(dir) = found.filter(|d| belongs(d)) {
        return Some(dir.clone());
    }
    // The command ended before finish ran, as with `exit`: look the call up by its id.
    std::fs::read_dir(captures)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|dir| belongs(dir))
        .max()
}

/// The last bytes of the first `size` bytes of `file`, where the footer is.
fn read_tail(file: &std::fs::File, size: u64) -> Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    let length = size.min(8192);
    let mut file = file;
    file.seek(SeekFrom::Start(size - length))?;
    let mut tail = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut tail)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(tail)
}

fn post_tool_use(input: &Value, env: &Env) -> Result<Option<Value>> {
    let response = &input["tool_response"];
    let Some(tool_use_id) = input["tool_use_id"].as_str() else {
        return Ok(None);
    };
    if response["interrupted"] == true || response["isImage"] == true {
        return Ok(None);
    }
    let stdout = response["stdout"].as_str().unwrap_or_default();
    let stderr = response["stderr"].as_str().unwrap_or_default();
    // The full output is Claude Code's file when it kept one, else the stdout it passed. A
    // file is taken at the size it has now and copied in a stream, so neither a huge nor a
    // growing file is held in memory.
    let persisted = match response["persistedOutputPath"].as_str() {
        Some(path) => {
            let file = std::fs::File::open(path).with_context(|| format!("cannot read {path}"))?;
            let size = file.metadata()?.len();
            Some((file, size))
        }
        None => None,
    };
    let tail = match &persisted {
        Some((file, size)) => read_tail(file, *size)?,
        None => stdout.as_bytes().to_vec(),
    };
    let tail_offset = persisted
        .as_ref()
        .map_or(0, |(_, size)| *size - tail.len() as u64);
    let found = footer(&tail);
    let captures = env.state_root.join("captures");
    let Some(dir) = capture_dir(&captures, tool_use_id, found.as_ref().map(|f| &f.1)) else {
        return Ok(None);
    };
    let metadata_path = dir.join("metadata.json");
    let mut metadata = read_metadata(&metadata_path)?;
    let full = persisted
        .as_ref()
        .map_or(stdout.len() as u64, |(_, size)| *size);
    let (output_len, code) = match &found {
        Some((start, footer_dir, code)) if *footer_dir == dir => {
            (tail_offset + *start as u64, *code)
        }
        _ => (full, None),
    };
    let output_path = dir.join("output.log");
    match persisted {
        Some((file, _)) => {
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output_path)?;
            std::io::copy(&mut file.take(output_len), &mut out)?;
        }
        None => state::write_new(&output_path, &stdout.as_bytes()[..output_len as usize])?,
    }
    let complete =
        response["persistedOutputPath"].is_string() || stdout.chars().count() < CLAUDE_STDOUT_CHARS;
    if !stderr.is_empty() {
        state::write_new(&dir.join("stderr.log"), stderr.as_bytes())?;
    }
    metadata["output_bytes"] = json!(output_len);
    metadata["stderr_bytes"] = json!(stderr.len());
    metadata["output_complete"] = json!(complete);
    state::write_atomic(&metadata_path, &serde_json::to_vec_pretty(&metadata)?)?;
    let shown_bytes = output_len as usize + stderr.len();
    let output = if shown_bytes <= SHOWN_BYTES {
        std::fs::read(&output_path)?
    } else {
        Vec::new()
    };

    let mut summary = format!(
        "exit_code: {}\noutput: {} bytes {}\n",
        code.map_or("unknown".to_string(), |c| c.to_string()),
        output_len,
        output_path.display()
    );
    if !stderr.is_empty() {
        summary.push_str(&format!(
            "stderr: {} bytes {}\n",
            stderr.len(),
            dir.join("stderr.log").display()
        ));
    }
    if !complete {
        summary
            .push_str("output_complete: false (Claude Code passed only the start of the output)\n");
    }
    if let Some(duration) = metadata["duration_seconds"].as_f64() {
        summary.push_str(&format!("duration_seconds: {duration}\n"));
    }
    let text = if shown_bytes <= SHOWN_BYTES {
        let mut text = String::from_utf8_lossy(&output).into_owned();
        if !stderr.is_empty() {
            text.push_str("\n--- stderr\n");
            text.push_str(stderr);
        }
        let separator = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        format!("{text}{separator}{summary}")
    } else {
        format!(
            "{summary}output not shown; ask yes/no questions with the agentgrasp ask tool, or read the file\n"
        )
    };
    Ok(Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "updatedToolOutput": {
                "stdout": text,
                "stderr": "",
                "interrupted": false,
                "isImage": false,
            }
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(state_root: &Path) -> Env {
        Env {
            state_root: state_root.to_path_buf(),
            binary: PathBuf::from("/opt/bin/agentgrasp"),
            claude_code_shell: None,
            shell: Some("/run/current-system/sw/bin/bash".into()),
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

    fn bash() -> Option<String> {
        find_program("bash").map(|p| p.to_string_lossy().into_owned())
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("/a b/it's"), r"'/a b/it'\''s'");
        assert!(plain_path(Path::new("/home/me/.cargo/bin/agentgrasp")).is_some());
        assert!(plain_path(Path::new("/home/my dir/agentgrasp")).is_none());
    }

    #[test]
    fn pre_tool_use_rewrites_without_grouping() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = env(temp.path());
        env.shell = bash();
        let output = hook(pre("ls -la").to_string().as_bytes(), &env)
            .unwrap()
            .unwrap();
        let specific = &output["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PreToolUse");
        assert!(specific.get("permissionDecision").is_none());
        let updated = &specific["updatedInput"];
        assert_eq!(updated["description"], "List", "every other field is kept");
        assert_eq!(updated["timeout"], 5000);
        let command = updated["command"].as_str().unwrap();
        let dir = std::fs::read_dir(temp.path().join("captures"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            dir.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("-toolu_01ABC")
        );
        assert_eq!(
            command,
            format!(
                "unset TYPESAFE_API_KEY\nls -la\n\n/opt/bin/agentgrasp finish '{}' \"exit:$?\"",
                dir.display()
            )
        );
        let metadata: Value =
            serde_json::from_slice(&std::fs::read(dir.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["command"], "ls -la");
        assert_eq!(metadata["cwd"], "/work");
    }

    #[test]
    fn calls_that_pass_through() {
        let temp = tempfile::tempdir().unwrap();
        let mut env = env(temp.path());
        env.shell = bash();
        let mut background = pre("sleep 9");
        background["tool_input"]["run_in_background"] = json!(true);
        let mut other_tool = pre("x");
        other_tool["tool_name"] = json!("Read");
        for input in [background, other_tool, pre("cat <<EOF\nno end")] {
            assert!(
                hook(input.to_string().as_bytes(), &env).unwrap().is_none(),
                "{input}"
            );
        }
        assert!(
            !temp.path().join("captures").exists()
                || std::fs::read_dir(temp.path().join("captures"))
                    .unwrap()
                    .count()
                    == 0,
            "a call that passes through leaves no capture"
        );
        if let Some(zsh) = find_program("zsh") {
            env.shell = Some(zsh.to_string_lossy().into_owned());
            assert!(
                hook(pre("cat <<EOF\nno end").to_string().as_bytes(), &env)
                    .unwrap()
                    .is_none(),
                "zsh"
            );
            let hidden = "for x (a b) print $x\ncat <<EOF\nno end";
            assert!(
                hook(pre(hidden).to_string().as_bytes(), &env)
                    .unwrap()
                    .is_none(),
                "zsh syntax that bash cannot parse passes through"
            );
            assert!(
                hook(pre("cat <<EOF\nend\nEOF").to_string().as_bytes(), &env)
                    .unwrap()
                    .is_some(),
                "a closed heredoc is fine"
            );
        }
        env.shell = Some("/usr/bin/fish".into());
        assert!(
            hook(pre("ls").to_string().as_bytes(), &env)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn finish_records_the_status_and_prints_the_footer() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("c");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            json!({"started_at_unix": unix_seconds(SystemTime::now()) - 2.0}).to_string(),
        )
        .unwrap();
        let printed = finish(&dir, "exit:3");
        assert_eq!(printed, format!("\n{FOOTER}{} exit 3\n", dir.display()));
        let metadata: Value =
            serde_json::from_slice(&std::fs::read(dir.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["exit_code"], 3);
        assert!(metadata["duration_seconds"].as_f64().unwrap() >= 2.0);
        // A missing record does not change what finish prints.
        assert!(finish(&temp.path().join("gone"), "exit:1").ends_with(" exit 1\n"));
    }

    fn post(stdout: &str, extra: Value) -> Value {
        let mut response =
            json!({"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false});
        response
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "toolu_01ABC", "tool_response": response})
    }

    fn capture(root: &Path) -> PathBuf {
        let dir = state::allocate(root, Kind::Capture, "toolu_01ABC").unwrap();
        std::fs::write(
            dir.join("metadata.json"),
            json!({"cwd": "/w", "duration_seconds": 1.5}).to_string(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn short_output_is_shown_with_the_summary() {
        let temp = tempfile::tempdir().unwrap();
        let dir = capture(temp.path());
        let stdout = format!("hello\n{FOOTER}{} exit 0", dir.display());
        let output = hook(
            post(&stdout, json!({})).to_string().as_bytes(),
            &env(temp.path()),
        )
        .unwrap()
        .unwrap();
        let shown = output["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .unwrap();
        assert!(
            shown.starts_with("hello\nexit_code: 0\noutput: 5 bytes "),
            "{shown}"
        );
        assert_eq!(std::fs::read(dir.join("output.log")).unwrap(), b"hello");
    }

    #[test]
    fn long_output_is_saved_and_summarized() {
        let temp = tempfile::tempdir().unwrap();
        let dir = capture(temp.path());
        let body = "\x1b[31mred\x1b[0m line\n".repeat(500);
        let persisted = temp.path().join("persisted.txt");
        std::fs::write(
            &persisted,
            format!("{body}\n{FOOTER}{} exit 2\n", dir.display()),
        )
        .unwrap();
        let input = post("cut", json!({"persistedOutputPath": persisted}));
        let output = hook(input.to_string().as_bytes(), &env(temp.path()))
            .unwrap()
            .unwrap();
        let shown = output["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .unwrap();
        assert!(shown.starts_with("exit_code: 2\noutput: "), "{shown}");
        assert!(shown.contains("output not shown"));
        assert!(!shown.contains("red"));
        assert_eq!(
            std::fs::read(dir.join("output.log")).unwrap(),
            body.as_bytes(),
            "exact bytes, ANSI kept"
        );
        let metadata: Value =
            serde_json::from_slice(&std::fs::read(dir.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["output_bytes"], body.len());
        assert_eq!(metadata["output_complete"], true);
    }

    #[test]
    fn a_persisted_file_is_taken_at_its_size_when_read() {
        let temp = tempfile::tempdir().unwrap();
        let dir = capture(temp.path());
        let body = "x".repeat(3 * 1024 * 1024);
        let persisted = temp.path().join("persisted.txt");
        std::fs::write(
            &persisted,
            format!("{body}\n{FOOTER}{} exit 0\n", dir.display()),
        )
        .unwrap();
        let input = post("cut", json!({"persistedOutputPath": persisted}));
        let output = hook(input.to_string().as_bytes(), &env(temp.path()))
            .unwrap()
            .unwrap();
        let shown = output["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .unwrap();
        assert!(
            shown.contains(&format!("output: {} bytes", body.len())),
            "{shown}"
        );
        assert_eq!(
            std::fs::metadata(dir.join("output.log")).unwrap().len(),
            body.len() as u64
        );
    }

    #[test]
    fn bytes_added_after_the_size_is_taken_are_not_copied() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("growing.txt");
        std::fs::write(&path, b"0123456789").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let size = file.metadata().unwrap().len();
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"more")
            .unwrap();
        assert_eq!(read_tail(&file, size).unwrap(), b"0123456789");
        let mut copied = Vec::new();
        file.take(size).read_to_end(&mut copied).unwrap();
        assert_eq!(copied, b"0123456789");
    }

    #[test]
    fn metadata_that_is_not_an_object_is_an_error_not_a_crash() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("c");
        std::fs::create_dir(&dir).unwrap();
        for text in ["[]", "42", "\"corrupt\"", "not json"] {
            std::fs::write(dir.join("metadata.json"), text).unwrap();
            assert!(finish(&dir, "exit:0").ends_with(" exit 0\n"), "{text}");
            assert!(read_metadata(&dir.join("metadata.json")).is_err());
        }
    }

    #[test]
    fn an_early_exit_is_found_by_its_call_id() {
        let temp = tempfile::tempdir().unwrap();
        let dir = capture(temp.path());
        let output = hook(
            post("partial output", json!({})).to_string().as_bytes(),
            &env(temp.path()),
        )
        .unwrap()
        .unwrap();
        let shown = output["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .unwrap();
        assert!(shown.contains("exit_code: unknown"));
        assert_eq!(
            std::fs::read(dir.join("output.log")).unwrap(),
            b"partial output"
        );
    }

    #[test]
    fn calls_without_a_capture_are_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            hook(
                post("x", json!({})).to_string().as_bytes(),
                &env(temp.path())
            )
            .unwrap()
            .is_none()
        );
        let _ = capture(temp.path());
        let interrupted = post("x", json!({"interrupted": true}));
        assert!(
            hook(interrupted.to_string().as_bytes(), &env(temp.path()))
                .unwrap()
                .is_none()
        );
    }
}
