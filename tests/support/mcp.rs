//! A raw MCP client over a child's stdin and stdout. It checks that every stdout line is a
//! JSON-RPC 2.0 message, and it answers `roots/list` with the roots it was given.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub struct McpClient {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    /// `None`: the client does not offer roots.
    roots: Option<Vec<PathBuf>>,
    next_id: u64,
    /// Every line the server wrote to stdout.
    pub stdout: Vec<String>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl McpClient {
    pub async fn start(mut command: Command, roots: Option<Vec<PathBuf>>) -> McpClient {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("server starts");
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut err = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let sink = stderr.clone();
        tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            while let Ok(n) = err.read(&mut buffer).await {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buffer[..n]);
            }
        });
        let mut client = McpClient {
            child,
            stdin,
            lines,
            roots,
            next_id: 0,
            stdout: Vec::new(),
            stderr,
        };
        let capabilities = if client.roots.is_some() {
            json!({"roots": {"listChanged": true}})
        } else {
            json!({})
        };
        let init = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": capabilities,
                    "clientInfo": {"name": "test", "version": "0"},
                }),
            )
            .await;
        assert!(init.get("result").is_some(), "initialize failed: {init}");
        client
            .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        client
    }

    pub async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.send_raw(&line).await;
    }

    pub async fn send_raw(&mut self, text: &str) {
        self.stdin.write_all(text.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Reads one stdout line and checks that it is a JSON-RPC 2.0 message.
    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(90), self.lines.next_line())
            .await
            .expect("the server answers in time")
            .unwrap()
            .expect("the server keeps stdout open");
        self.stdout.push(line.clone());
        let message: Value = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("stdout line is not JSON: {line}"));
        assert_eq!(
            message["jsonrpc"], "2.0",
            "stdout line is not JSON-RPC 2.0: {line}"
        );
        message
    }

    /// Sends a request and returns its response. Server requests in between are answered.
    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.response_to(json!(id)).await
    }

    pub async fn response_to(&mut self, id: Value) -> Value {
        loop {
            let message = self.read().await;
            if !self.answer_server_request(&message).await && message.get("id") == Some(&id) {
                return message;
            }
        }
    }

    /// Reads and handles at most one message within `wait`: a server request is answered.
    pub async fn step(&mut self, wait: Duration) {
        if let Ok(message) = tokio::time::timeout(wait, self.read()).await {
            self.answer_server_request(&message).await;
        }
    }

    /// Answers `roots/list`, and any other server request with "method not found". False when
    /// `message` is not a server request.
    async fn answer_server_request(&mut self, message: &Value) -> bool {
        if message.get("method").is_none() {
            return false;
        }
        if let Some(request_id) = message.get("id") {
            let reply = match (message["method"].as_str(), &self.roots) {
                (Some("roots/list"), Some(roots)) => json!({
                    "jsonrpc": "2.0", "id": request_id,
                    "result": {"roots": roots.iter().map(|r| json!({"uri": file_uri(r)})).collect::<Vec<_>>()},
                }),
                _ => json!({
                    "jsonrpc": "2.0", "id": request_id,
                    "error": {"code": -32601, "message": "method not found"},
                }),
            };
            self.send(reply).await;
        }
        true
    }

    /// Calls a tool and returns the JSON-RPC result.
    pub async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let response = self
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await;
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/call failed: {response}"))
    }

    pub fn set_roots(&mut self, roots: Vec<PathBuf>) {
        self.roots = Some(roots);
    }

    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }

    pub async fn close(mut self) -> String {
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await;
        while let Ok(Ok(Some(line))) =
            tokio::time::timeout(Duration::from_secs(1), self.lines.next_line()).await
        {
            self.stdout.push(line);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stderr = self.stderr.lock().unwrap().clone();
        String::from_utf8_lossy(&stderr).into_owned()
    }
}

fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}
