//! The capture chain through real shells: PreToolUse rewrites the command, the shell runs it
//! the way Claude Code runs Bash calls, and PostToolUse saves and summarizes the output.

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
    /// What the PostToolUse hook returned, if anything.
    shown: Option<String>,
    /// The merged output the shell printed, as Claude Code captures it.
    raw: String,
    status: i32,
    capture: Option<PathBuf>,
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

    fn hook(&self, input: &Value) -> Option<Value> {
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
        assert!(output.status.success(), "a hook always exits 0");
        let stdout = String::from_utf8(output.stdout).unwrap();
        (!stdout.trim().is_empty()).then(|| serde_json::from_str(&stdout).unwrap())
    }

    fn call(&mut self, command: &str) -> Call {
        self.calls += 1;
        let id = format!("toolu_{}", self.calls);
        let pre = json!({
            "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": id,
            "cwd": self.cwd, "tool_input": {"command": command, "description": "test"},
        });
        let rewritten = self
            .hook(&pre)
            .map(|o| {
                o["hookSpecificOutput"]["updatedInput"]["command"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .unwrap_or_else(|| command.to_string());
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
        let raw = String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string();
        let status = output.status.code().unwrap_or(-1);
        // Claude Code runs PostToolUse for a call that exits 0, PostToolUseFailure otherwise.
        let shown = (status == 0)
            .then(|| {
                let post = json!({
                    "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": id,
                    "tool_response": {"stdout": raw, "stderr": "", "interrupted": false, "isImage": false},
                });
                self.hook(&post)
            })
            .flatten()
            .map(|o| o["hookSpecificOutput"]["updatedToolOutput"]["stdout"].as_str().unwrap().to_string());
        let capture = std::fs::read_dir(self.state.join("agentgrasp/captures"))
            .ok()
            .and_then(|dirs| {
                dirs.filter_map(|d| d.ok())
                    .map(|d| d.path())
                    .find(|p| p.to_string_lossy().ends_with(&format!("-{id}")))
            });
        Call {
            shown,
            raw,
            status,
            capture,
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

#[test]
fn short_output_is_shown_and_saved_exactly() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let call = session.call("printf '\\033[31mred\\033[0m\\n'; echo err >&2");
        assert_eq!(call.status, 0, "{shell:?}");
        let shown = call.shown.expect("output is replaced");
        assert!(
            shown.starts_with("\u{1b}[31mred\u{1b}[0m\nerr\nexit_code: 0\n"),
            "{shell:?}: {shown:?}"
        );
        let dir = call.capture.unwrap();
        assert_eq!(
            std::fs::read(dir.join("output.log")).unwrap(),
            b"\x1b[31mred\x1b[0m\nerr\n"
        );
        assert_eq!(metadata(&dir)["exit_code"], 0);
    }
}

#[test]
fn long_output_is_summarized() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let call = session.call("i=0; while [ $i -lt 300 ]; do echo \"line $i of output\"; i=$((i+1)); done; exit_code_marker=1");
        let shown = call.shown.unwrap();
        assert!(
            shown.starts_with("exit_code: 0\noutput: "),
            "{shell:?}: {shown}"
        );
        assert!(shown.contains("output not shown"));
        assert!(!shown.contains("line 299"));
        let saved = std::fs::read_to_string(call.capture.unwrap().join("output.log")).unwrap();
        assert!(saved.starts_with("line 0 of output\n") && saved.ends_with("line 299 of output\n"));
    }
}

#[test]
fn a_failing_command_reports_its_status_in_the_summary() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let call = session.call("echo before; false");
        assert_eq!(call.status, 0, "the call exits 0 so PostToolUse runs");
        let shown = call.shown.unwrap();
        assert!(shown.contains("exit_code: 1"), "{shell:?}: {shown}");
        assert_eq!(metadata(&call.capture.unwrap())["exit_code"], 1);
    }
}

#[test]
fn the_command_runs_as_without_the_hook() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let key = session.call("echo \"key=${TYPESAFE_API_KEY:-unset}\"");
        assert!(key.shown.unwrap().starts_with("key=unset\n"), "{shell:?}");
        let stdin = session.call("cat; echo after-cat");
        assert!(
            stdin.shown.unwrap().starts_with("after-cat\n"),
            "stdin is at EOF"
        );
        let comment = session.call("echo hi # a trailing comment");
        assert!(comment.shown.unwrap().starts_with("hi\n"));
        let backslash = session.call("echo joined \\");
        assert!(
            backslash.shown.unwrap().starts_with("joined\n"),
            "{shell:?}"
        );
        let heredoc = session.call("cat <<EOF\nfrom heredoc\nEOF");
        assert!(heredoc.shown.unwrap().starts_with("from heredoc\n"));
        let function = session.call("agentgrasp() { echo fake; }; PATH=/nonexistent; echo still");
        assert!(
            function.shown.unwrap().starts_with("still\nexit_code: 0"),
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
        assert!(call.shown.is_none());
        assert!(!call.raw.contains("agentgrasp"), "{shell:?}: {}", call.raw);
    }
}

#[test]
fn cd_carries_over_to_the_next_call() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        session.call("cd sub");
        let call = session.call("pwd");
        assert!(
            call.shown
                .unwrap()
                .starts_with(&format!("{}\n", session.cwd.display()))
        );
        assert!(session.cwd.ends_with("work/sub"), "{shell:?}");
    }
}

// `set -e` is not here: Claude Code runs a call as `eval '...' && pwd`, and a command in an
// `&&` list ignores errexit, with or without the hook.
#[test]
fn exit_and_exec_skip_finish() {
    for shell in shells() {
        for command in ["echo partial; exit 3", "exec false"] {
            let mut session = Session::new(shell.clone());
            let call = session.call(command);
            assert_ne!(call.status, 0, "{shell:?} {command}");
            assert!(
                !call.raw.contains("agentgrasp: capture"),
                "{shell:?} {command}: {}",
                call.raw
            );
            let dir = call.capture.expect("the capture directory exists");
            assert!(
                metadata(&dir).get("exit_code").is_none(),
                "finish did not run"
            );
        }
    }
}

#[test]
fn an_early_exit_with_status_zero_is_still_saved() {
    for shell in shells() {
        let mut session = Session::new(shell.clone());
        let call = session.call("echo partial; exit 0");
        let shown = call.shown.unwrap();
        assert!(
            shown.starts_with("partial\nexit_code: unknown"),
            "{shell:?}: {shown}"
        );
        assert_eq!(
            std::fs::read(call.capture.unwrap().join("output.log")).unwrap(),
            b"partial"
        );
    }
}

#[test]
fn finish_exits_0_with_the_footer_when_its_record_is_corrupt() {
    let temp = tempfile::tempdir().unwrap();
    for text in ["[]", "42", "not json"] {
        std::fs::write(temp.path().join("metadata.json"), text).unwrap();
        let output = Command::new(BINARY)
            .args(["finish"])
            .arg(temp.path())
            .arg("exit:5")
            .output()
            .unwrap();
        assert!(output.status.success(), "{text}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.ends_with(&format!(
                "agentgrasp: capture {} exit 5\n",
                temp.path().display()
            )),
            "{stdout}"
        );
    }
}
