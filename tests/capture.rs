//! The capture chain through real shells: the hook rewrites the command, and the shell runs
//! it the way Claude Code runs Bash calls.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_agentgrasp");

fn find(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// A shell session as Claude Code keeps one: each call runs in a fresh process, in the working
/// directory the previous call left.
struct Session {
    shell: PathBuf,
    _temp: tempfile::TempDir,
    state: PathBuf,
    cwd: PathBuf,
    cwd_file: PathBuf,
    calls: usize,
}

struct Call {
    /// What the agent sees: stdout and stderr of the call, merged as Claude Code shows them.
    shown: String,
    status: i32,
    capture: Option<PathBuf>,
    /// The hook's reason when it did not rewrite the call.
    hook_stderr: String,
}

impl Session {
    fn new(shell: PathBuf) -> Session {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(base.join("work/sub")).unwrap();
        Session {
            shell,
            state: base.join("state"),
            cwd: base.join("work"),
            cwd_file: base.join("cwd"),
            _temp: temp,
            calls: 0,
        }
    }

    fn rewrite(&self, command: &str, id: &str) -> (Option<String>, String) {
        let input = json!({
            "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": id,
            "cwd": self.cwd, "tool_input": {"command": command, "description": "test"},
        });
        let mut child = Command::new(BINARY)
            .arg("hook")
            .env("XDG_STATE_HOME", &self.state)
            .env("SHELL", &self.shell)
            .env_remove("CLAUDE_CODE_SHELL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "the hook always exits 0");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if stdout.trim().is_empty() {
            return (None, stderr);
        }
        let value: Value = serde_json::from_str(&stdout).unwrap();
        let command = value["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        (Some(command.to_string()), stderr)
    }

    fn call(&mut self, command: &str) -> Call {
        self.calls += 1;
        let id = format!("toolu_{}", self.calls);
        let (rewritten, hook_stderr) = self.rewrite(command, &id);
        let rewritten = rewritten.unwrap_or_else(|| command.to_string());
        let quoted = format!("'{}'", rewritten.replace('\'', r"'\''"));
        let script = format!(
            "eval {quoted} < /dev/null && pwd -P >| '{}'",
            self.cwd_file.display()
        );
        let output = Command::new(&self.shell)
            .args(["-c", &format!("{{ {script}; }} 2>&1")])
            .current_dir(&self.cwd)
            .env("TYPESAFE_API_KEY", "secret-key")
            .output()
            .unwrap();
        if let Ok(cwd) = std::fs::read_to_string(&self.cwd_file) {
            self.cwd = PathBuf::from(cwd.trim());
        }
        let capture = std::fs::read_dir(self.state.join("agentgrasp/captures"))
            .ok()
            .and_then(|dirs| {
                dirs.filter_map(|d| d.ok())
                    .map(|d| d.path())
                    .find(|p| p.to_string_lossy().ends_with(&format!("-{id}")))
            });
        Call {
            shown: String::from_utf8_lossy(&output.stdout).into_owned(),
            status: output.status.code().unwrap_or(-1),
            capture,
            hook_stderr,
        }
    }
}

fn metadata(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(dir.join("metadata.json")).unwrap()).unwrap()
}

fn shells() -> Vec<PathBuf> {
    let found: Vec<PathBuf> = ["bash", "zsh"].iter().filter_map(|s| find(s)).collect();
    assert!(!found.is_empty(), "no bash or zsh on PATH");
    found
}

/// The capture directory of a call that must have been rewritten.
fn dir(call: &Call) -> PathBuf {
    call.capture.clone().unwrap_or_else(|| {
        panic!(
            "the call was not rewritten; the hook said: {}",
            call.hook_stderr
        )
    })
}

/// The capture line, then what finish printed.
fn after_capture_line(call: &Call) -> &str {
    let dir = call.capture.as_ref().unwrap_or_else(|| {
        panic!(
            "the call was not rewritten; the hook said: {}",
            call.hook_stderr
        )
    });
    let line = format!("agentgrasp: {}\n", dir.display());
    call.shown
        .strip_prefix(&line)
        .unwrap_or_else(|| panic!("no capture line: {}", call.shown))
}

#[test]
fn logs_hold_the_exact_bytes_of_each_stream() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let call = session.call("printf '\\033[31mred\\033[0m\\n'; printf 'err' >&2");
        assert_eq!(call.status, 0, "{shell:?}");
        let dir = dir(&call);
        assert_eq!(
            std::fs::read(dir.join("stdout.log")).unwrap(),
            b"\x1b[31mred\x1b[0m\n"
        );
        assert_eq!(std::fs::read(dir.join("stderr.log")).unwrap(), b"err");
        let shown = after_capture_line(&call);
        assert!(
            shown.starts_with(
                "\u{1b}[31mred\u{1b}[0m\n--- stderr\nerr\nexit_code: 0\nstdout: 13 bytes "
            ),
            "{shell:?}: {shown:?}"
        );
        assert_eq!(metadata(&dir)["exit_code"], 0);
        assert_eq!(metadata(&dir)["stderr_bytes"], 3);
    }
}

#[test]
fn long_output_is_summarized() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let call = session
            .call("i=0; while [ $i -lt 300 ]; do echo \"line $i of output\"; i=$((i+1)); done");
        let shown = after_capture_line(&call);
        assert!(
            shown.starts_with("exit_code: 0\nstdout: "),
            "{shell:?}: {shown}"
        );
        assert!(shown.ends_with("output not shown; ask yes/no questions with the agentgrasp ask tool, or read the files\n"));
        assert!(!shown.contains("line 299"));
        let saved = std::fs::read_to_string(dir(&call).join("stdout.log")).unwrap();
        assert!(saved.starts_with("line 0 of output\n") && saved.ends_with("line 299 of output\n"));
    }
}

#[test]
fn a_failing_command_fails_with_its_status() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let call = session.call("echo before; (exit 3)");
        assert_eq!(call.status, 3, "{shell:?}: Claude Code sees the failure");
        assert!(after_capture_line(&call).starts_with("before\nexit_code: 3\n"));
        assert_eq!(metadata(&dir(&call))["exit_code"], 3);
    }
}

#[test]
fn the_command_runs_as_without_the_hook() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let key = session.call("echo \"key=${TYPESAFE_API_KEY:-unset}\"");
        assert!(
            after_capture_line(&key).starts_with("key=unset\n"),
            "{shell:?}"
        );
        let stdin = session.call("cat; echo after-cat");
        assert!(
            after_capture_line(&stdin).starts_with("after-cat\n"),
            "stdin is at EOF"
        );
        let comment = session.call("echo hi # a trailing comment");
        assert!(after_capture_line(&comment).starts_with("hi\n"));
        let backslash = session.call("echo joined \\");
        assert!(
            after_capture_line(&backslash).starts_with("joined\n"),
            "{shell:?}"
        );
        let heredoc = session.call("cat <<EOF\nfrom heredoc\nEOF");
        assert!(after_capture_line(&heredoc).starts_with("from heredoc\n"));
        let function = session.call("agentgrasp() { echo fake; }; PATH=/nonexistent; echo still");
        assert!(
            after_capture_line(&function).starts_with("still\nexit_code: 0"),
            "finish runs by its absolute path"
        );
    }
}

#[test]
fn an_unclosed_heredoc_passes_through() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let call = session.call("cat <<EOF\nno end");
        assert!(
            call.capture.is_none(),
            "{shell:?}: the call is not rewritten"
        );
        assert!(
            !call.shown.contains("agentgrasp"),
            "{shell:?}: {}",
            call.shown
        );
    }
}

#[test]
fn cd_carries_over_to_the_next_call() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        session.call("cd sub");
        let call = session.call("pwd");
        assert!(after_capture_line(&call).starts_with(&format!("{}\n", session.cwd.display())));
        assert!(session.cwd.ends_with("work/sub"), "{shell:?}");
    }
}

// `set -e` is not here: Claude Code runs a call as `eval '...' && pwd`, and a command in an
// `&&` list ignores errexit, with or without the hook.
#[test]
fn exit_and_exec_skip_finish_but_name_the_capture() {
    for shell in shells() {
        for command in ["echo partial; exit 3", "exec false"] {
            let mut session = Session::new(shell.clone());
            let call = session.call(command);
            assert_ne!(call.status, 0, "{shell:?} {command}");
            let dir = dir(&call);
            assert_eq!(
                call.shown,
                format!("agentgrasp: {}\n", dir.display()),
                "{shell:?} {command}"
            );
            assert!(
                metadata(&dir).get("exit_code").is_none(),
                "finish did not run"
            );
        }
        let mut session = Session::new(shell.clone());
        let call = session.call("echo partial; exit 3");
        assert_eq!(
            std::fs::read(dir(&call).join("stdout.log")).unwrap(),
            b"partial\n"
        );
    }
}

#[test]
fn finish_exits_with_the_status_when_its_record_is_corrupt() {
    let temp = tempfile::tempdir().unwrap();
    for text in ["[]", "42", "not json"] {
        std::fs::write(temp.path().join("metadata.json"), text).unwrap();
        let output = Command::new(BINARY)
            .args(["finish"])
            .arg(temp.path())
            .arg("5")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(5), "{text}");
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("exit_code: 5")
        );
    }
}
