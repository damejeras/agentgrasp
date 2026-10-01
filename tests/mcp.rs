//! End-to-end tests of the MCP server. This binary has no libtest harness: run with
//! `AGENTGRASP_TEST_JEV`, it is the server itself, serving MCP on stdio against a fake Jev
//! server. So the tests drive real stdio streams while the production binary keeps its one
//! fixed endpoint. Libtest output would mix with the protocol stream, hence no harness.

mod support;

use std::future::Future;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use serde_json::{Value, json};
use support::mcp::McpClient;
use support::{FakeJev, Reply};
use tokio::process::Command;

const CHILD_ENV: &str = "AGENTGRASP_TEST_JEV";
const MARKER: &str = "SOURCE-TEXT-MARKER-7f3a";

fn main() {
    if let Ok(endpoint) = std::env::var(CHILD_ENV) {
        let config = agentgrasp::mcp::Config {
            endpoint,
            key: std::env::var("TYPESAFE_API_KEY").ok(),
            state_root: PathBuf::from(std::env::var("AGENTGRASP_TEST_STATE").unwrap()),
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(agentgrasp::mcp::serve(config)).unwrap();
        return;
    }
    type Test = fn() -> Pin<Box<dyn Future<Output = ()>>>;
    let tests: Vec<(&str, Test)> = vec![
        ("lists_ask_with_schemas", || {
            Box::pin(lists_ask_with_schemas())
        }),
        ("invalid_arguments_set_is_error", || {
            Box::pin(invalid_arguments_set_is_error())
        }),
        ("rejects_paths_outside_roots_and_credentials", || {
            Box::pin(rejects_paths_outside_roots_and_credentials())
        }),
        ("captures_of_this_project_only", || {
            Box::pin(captures_of_this_project_only())
        }),
        ("a_client_without_roots_allows_nothing", || {
            Box::pin(a_client_without_roots_allows_nothing())
        }),
        ("roots_are_read_at_each_call", || {
            Box::pin(roots_are_read_at_each_call())
        }),
        ("missing_key_is_provider_unavailable", || {
            Box::pin(missing_key_is_provider_unavailable())
        }),
        ("answers_keep_questions_order_and_duplicates", || {
            Box::pin(answers_keep_questions_order_and_duplicates())
        }),
        ("several_files_keep_paths_and_boundaries", || {
            Box::pin(several_files_keep_paths_and_boundaries())
        }),
        ("file_errors_and_input_limit", || {
            Box::pin(file_errors_and_input_limit())
        }),
        ("provider_failures_reach_error_not_is_error", || {
            Box::pin(provider_failures_reach_error_not_is_error())
        }),
        ("nothing_leaks_to_stdout_or_stderr", || {
            Box::pin(nothing_leaks_to_stdout_or_stderr())
        }),
        ("malformed_protocol_requests_get_protocol_errors", || {
            Box::pin(malformed_protocol_requests_get_protocol_errors())
        }),
        ("cancellation_is_recorded", || {
            Box::pin(cancellation_is_recorded())
        }),
        ("production_binary_smoke", || {
            Box::pin(production_binary_smoke())
        }),
    ];
    let filter: Option<String> = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let mut failed = Vec::new();
    for (name, test) in tests {
        if filter.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
            continue;
        }
        let outcome = std::panic::catch_unwind(|| {
            tokio::runtime::Runtime::new().unwrap().block_on(test());
        });
        println!(
            "test {name} ... {}",
            if outcome.is_ok() { "ok" } else { "FAILED" }
        );
        if outcome.is_err() {
            failed.push(name);
        }
    }
    if !failed.is_empty() {
        eprintln!("failed: {failed:?}");
        std::process::exit(1);
    }
}

/// A temp directory with a project root and a state directory.
struct World {
    _temp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    state: PathBuf,
}

impl World {
    fn new() -> World {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let root = base.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let state = base.join("state/agentgrasp");
        World {
            _temp: temp,
            base,
            root,
            state,
        }
    }

    fn file(&self, relative: &str, content: impl AsRef<[u8]>) -> String {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    async fn server(
        &self,
        jev: &FakeJev,
        key: Option<&str>,
        roots: Option<Vec<PathBuf>>,
    ) -> McpClient {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env(CHILD_ENV, &jev.url)
            .env("AGENTGRASP_TEST_STATE", &self.state);
        match key {
            Some(key) => command.env("TYPESAFE_API_KEY", key),
            None => command.env_remove("TYPESAFE_API_KEY"),
        };
        McpClient::start(command, roots).await
    }

    async fn default_server(&self, jev: &FakeJev) -> McpClient {
        self.server(jev, Some("key"), Some(vec![self.root.clone()]))
            .await
    }
}

async fn answering_jev() -> FakeJev {
    FakeJev::start(|_, request| {
        Reply::answers(
            request,
            |text| if text.contains("yes") { 0.95 } else { 0.05 },
        )
    })
    .await
}

fn structured(result: &Value) -> &Value {
    &result["structuredContent"]
}

fn assert_invalid(result: &Value) {
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(
        structured(result)["error"]["code"],
        "invalid_input",
        "{result}"
    );
    assert_eq!(structured(result)["evaluation_status"], "failed");
    assert_eq!(structured(result)["record_path"], Value::Null);
}

async fn lists_ask_with_schemas() {
    let world = World::new();
    let jev = answering_jev().await;
    let mut client = world.default_server(&jev).await;
    let listed = client.request("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    let ask = tools
        .iter()
        .find(|t| t["name"] == "ask")
        .expect("ask is listed");
    assert_eq!(ask["inputSchema"]["type"], "object");
    assert_eq!(
        ask["inputSchema"]["required"],
        json!(["paths", "questions"])
    );
    assert_eq!(
        ask["inputSchema"]["properties"]["questions"]["maxItems"],
        64
    );
    let output = &ask["outputSchema"];
    for field in [
        "answers",
        "evaluation_status",
        "model",
        "sources",
        "record_path",
        "error",
    ] {
        assert!(
            output["properties"].get(field).is_some(),
            "output schema has {field}"
        );
    }
    let description = ask["description"].as_str().unwrap();
    for phrase in [
        "probability",
        "0.9",
        "0.1",
        "read the file",
        "input_too_large",
        "record",
    ] {
        assert!(
            description.contains(phrase),
            "description mentions {phrase}"
        );
    }
}

async fn invalid_arguments_set_is_error() {
    let world = World::new();
    let file = world.file("a.log", "text");
    let jev = answering_jev().await;
    let mut client = world.default_server(&jev).await;
    let long = "x".repeat(2049);
    let many: Vec<String> = (0..65).map(|i| format!("q{i}?")).collect();
    let cases = [
        json!({"paths": file, "questions": ["q?"]}),
        json!({"paths": [file]}),
        json!({"paths": [], "questions": ["q?"]}),
        json!({"paths": [file], "questions": []}),
        json!({"paths": [file], "questions": many}),
        json!({"paths": [file], "questions": ["  "]}),
        json!({"paths": [file], "questions": [long]}),
        json!({"paths": [file], "questions": ["q?"], "extra": 1}),
        json!({"paths": ["a.log"], "questions": ["q?"]}),
    ];
    for arguments in cases {
        let result = client.call("ask", arguments.clone()).await;
        assert_invalid(&result);
    }
    assert_eq!(jev.requests(), 0);
    assert!(
        !world.state.join("asks").exists(),
        "no record for invalid input"
    );
}

async fn rejects_paths_outside_roots_and_credentials() {
    let world = World::new();
    let inside = world.file("ok.log", "fine");
    let env = world.file(".env", "KEY=1");
    let pem = world.file("certs/server.pem", "x");
    let outside = world.base.join("outside.log");
    std::fs::write(&outside, "secret").unwrap();
    symlink(&outside, world.root.join("escape.log")).unwrap();
    let jev = answering_jev().await;
    let mut client = world.default_server(&jev).await;
    for path in [
        outside.to_string_lossy().into_owned(),
        world.root.join("escape.log").to_string_lossy().into_owned(),
        env,
        pem,
        world
            .root
            .join("../outside.log")
            .to_string_lossy()
            .into_owned(),
    ] {
        // The good path first: one bad path makes the whole call invalid before any read.
        let result = client
            .call("ask", json!({"paths": [inside, path], "questions": ["q?"]}))
            .await;
        assert_invalid(&result);
    }
    assert_eq!(jev.requests(), 0);
}

fn make_capture(world: &World, id: &str, cwd: &Path) -> PathBuf {
    let dir = world.state.join("captures").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("metadata.json"),
        json!({"command": "make", "cwd": cwd}).to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("stderr.log"), "error: missing import").unwrap();
    dir
}

async fn captures_of_this_project_only() {
    let world = World::new();
    let other = world.base.join("other-project");
    std::fs::create_dir_all(world.root.join("sub")).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let mine = make_capture(&world, "1", &world.root.join("sub"));
    let theirs = make_capture(&world, "2", &other);
    let jev = answering_jev().await;
    let mut client = world.default_server(&jev).await;
    let result = client
        .call(
            "ask",
            json!({"paths": [mine.join("stderr.log")], "questions": ["yes?"]}),
        )
        .await;
    assert_eq!(
        structured(&result)["evaluation_status"],
        "complete",
        "{result}"
    );
    let result = client
        .call(
            "ask",
            json!({"paths": [theirs.join("stderr.log")], "questions": ["yes?"]}),
        )
        .await;
    assert_invalid(&result);
}

async fn a_client_without_roots_allows_nothing() {
    let world = World::new();
    let file = world.file("a.log", "text");
    let jev = answering_jev().await;
    let mut client = world.server(&jev, Some("key"), None).await;
    let result = client
        .call("ask", json!({"paths": [file], "questions": ["q?"]}))
        .await;
    assert_invalid(&result);
    assert_eq!(jev.requests(), 0);
}

async fn roots_are_read_at_each_call() {
    let world = World::new();
    let other = world.base.join("second");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("b.log"), "b").unwrap();
    let jev = answering_jev().await;
    let mut client = world.default_server(&jev).await;
    let call = json!({"paths": [other.join("b.log")], "questions": ["yes?"]});
    assert_invalid(&client.call("ask", call.clone()).await);
    client.set_roots(vec![world.root.clone(), other.clone()]);
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"}))
        .await;
    let result = client.call("ask", call).await;
    assert_eq!(
        structured(&result)["evaluation_status"],
        "complete",
        "{result}"
    );
}

async fn missing_key_is_provider_unavailable() {
    let world = World::new();
    let file = world.file("a.log", "text");
    let jev = answering_jev().await;
    let mut client = world
        .server(&jev, None, Some(vec![world.root.clone()]))
        .await;
    let result = client
        .call("ask", json!({"paths": [file], "questions": ["q?"]}))
        .await;
    assert!(result.get("isError").is_none_or(|v| v == false), "{result}");
    let output = structured(&result);
    assert_eq!(output["error"]["code"], "provider_unavailable");
    assert_eq!(output["answers"], json!([]));
    assert_eq!(output["model"], Value::Null);
    assert_eq!(jev.requests(), 0);
}

async fn answers_keep_questions_order_and_duplicates() {
    let world = World::new();
    let file = world.file("build.log", "error: cannot find module");
    let jev = FakeJev::start(|_, request| {
        // Answer by key, so a wrong key mapping shows up as a wrong probability.
        let answers: serde_json::Map<String, Value> = request["questions"]
            .as_object()
            .unwrap()
            .keys()
            .map(|key| {
                let index: f64 = key[1..].parse().unwrap();
                (key.clone(), json!({"type": "noul", "noul": index / 10.0}))
            })
            .collect();
        Reply::status(200, json!({"model": "jev-9", "answers": answers, "usage": {"input_tokens": 5, "output_tokens": 1}}).to_string())
    })
    .await;
    let mut client = world.default_server(&jev).await;
    let questions = json!(["Same?", "Other \"quoted\" ünïcode?", "Same?", " spaced "]);
    let result = client
        .call("ask", json!({"paths": [file], "questions": questions}))
        .await;
    let output = structured(&result);
    assert!(result.get("isError").is_none_or(|v| v == false));
    assert_eq!(output["evaluation_status"], "complete");
    assert_eq!(output["model"], "jev-9");
    assert_eq!(
        output["answers"],
        json!([
            {"question": "Same?", "p_yes": 0.0},
            {"question": "Other \"quoted\" ünïcode?", "p_yes": 0.1},
            {"question": "Same?", "p_yes": 0.2},
            {"question": " spaced ", "p_yes": 0.3},
        ])
    );
    let sent = jev.body(0);
    assert_eq!(sent["questions"]["q3"]["instructions"], " spaced ");
    assert_eq!(sent["questions"]["q2"]["type"], "noul");
    let record_path = output["record_path"].as_str().unwrap();
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(record_path).unwrap()).unwrap();
    assert_eq!(record["questions"], questions);
    assert_eq!(record["model"], "jev-9");
    assert_eq!(record["usage"]["input_tokens"], 5);
    assert_eq!(record["requests"][0]["status"], 200);
    assert!(record["latency_ms"].is_u64());
    assert!(
        !std::fs::read_to_string(record_path)
            .unwrap()
            .contains("cannot find module")
    );
    assert!(record_path.starts_with(world.state.join("asks").to_str().unwrap()));
}

async fn several_files_keep_paths_and_boundaries() {
    let world = World::new();
    let a = world.file("a.log", "alpha\n");
    let b = world.file("dir/b.log", "beta");
    symlink(world.root.join("a.log"), world.root.join("alias.log")).unwrap();
    let alias = world.root.join("alias.log").to_string_lossy().into_owned();
    let jev = answering_jev().await;
    let mut client = world.default_server(&jev).await;
    let result = client
        .call(
            "ask",
            json!({"paths": [alias, b, a], "questions": ["yes?"]}),
        )
        .await;
    let output = structured(&result);
    assert_eq!(output["evaluation_status"], "complete", "{result}");
    let sources = output["sources"].as_array().unwrap();
    assert_eq!(
        sources.len(),
        2,
        "a and its alias are one file: {sources:?}"
    );
    assert_eq!(sources[0]["path"], alias, "the first path given is shown");
    assert_eq!(sources[0]["bytes"], 6);
    assert_eq!(sources[0]["sha256"], agentgrasp::sha256_hex(b"alpha\n"));
    let files = jev.body(0)["state"]["files"].clone();
    assert_eq!(
        files,
        json!([{"path": alias, "content": "alpha\n"}, {"path": b, "content": "beta"}])
    );

    // One large file under three names counts once against the input limit.
    let big = world.file("big.log", "z".repeat(200 * 1024));
    std::fs::hard_link(&big, world.root.join("hard.log")).unwrap();
    symlink(&big, world.root.join("soft.log")).unwrap();
    let names = json!([
        big,
        world.root.join("hard.log"),
        world.root.join("soft.log")
    ]);
    let result = client
        .call("ask", json!({"paths": names, "questions": ["yes?"]}))
        .await;
    let output = structured(&result);
    assert_eq!(output["evaluation_status"], "complete", "{result}");
    assert_eq!(output["sources"].as_array().unwrap().len(), 1);
}

async fn file_errors_and_input_limit() {
    let world = World::new();
    let good = world.file("good.log", "ok");
    let latin1 = world.file("latin1.log", [0x63, 0x61, 0x66, 0xe9]);
    let missing = world
        .root
        .join("missing.log")
        .to_string_lossy()
        .into_owned();
    std::fs::create_dir_all(world.root.join("dir")).unwrap();
    let dir = world.root.join("dir").to_string_lossy().into_owned();
    let half = world.file("half1.log", "x".repeat(130 * 1024));
    let half2 = world.file("half2.log", "y".repeat(130 * 1024));
    let below_file = world
        .root
        .join("good.log/child")
        .to_string_lossy()
        .into_owned();
    let jev = answering_jev().await;
    let mut client = world.default_server(&jev).await;
    let cases = [
        (vec![good.clone(), latin1], "unsupported_encoding", 1),
        (vec![good.clone(), missing], "source_unreadable", 1),
        (vec![dir], "source_unreadable", 0),
        (vec![below_file], "source_unreadable", 0),
        (vec![good.clone(), half, half2], "input_too_large", 2),
    ];
    for (paths, code, sources) in cases {
        let result = client
            .call("ask", json!({"paths": paths, "questions": ["q?"]}))
            .await;
        assert!(result.get("isError").is_none_or(|v| v == false), "{result}");
        let output = structured(&result);
        assert_eq!(output["error"]["code"], code, "{result}");
        assert_eq!(output["evaluation_status"], "failed");
        assert_eq!(output["answers"], json!([]));
        assert_eq!(
            output["sources"].as_array().unwrap().len(),
            sources,
            "{result}"
        );
        assert!(output["record_path"].is_string());
    }
    assert_eq!(jev.requests(), 0, "nothing is sent when a file fails");
}

async fn provider_failures_reach_error_not_is_error() {
    let world = World::new();
    let file = world.file("a.log", "text");
    let cases = [
        (
            Reply::status(
                200,
                r#"{"model":"m","answers":{"q0":{"type":"noul","noul":0.5}},"usage":{"input_tokens":1,"output_tokens":1}}"#,
            ),
            "invalid_provider_response",
        ),
        (
            Reply::status(400, r#"{"detail":{"error_type":"max_tokens_exceeded"}}"#),
            "input_too_large",
        ),
        (Reply::status(500, "{}"), "provider_unavailable"),
    ];
    for (reply, code) in cases {
        let reply = std::sync::Mutex::new(Some(reply));
        let jev = FakeJev::start(move |_, _| {
            reply
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Reply::status(500, "{}"))
        })
        .await;
        let mut client = world.default_server(&jev).await;
        let result = client
            .call("ask", json!({"paths": [file], "questions": ["a?", "b?"]}))
            .await;
        assert!(result.get("isError").is_none_or(|v| v == false), "{result}");
        let output = structured(&result);
        assert_eq!(output["error"]["code"], code, "{result}");
        assert_eq!(
            output["answers"],
            json!([]),
            "a partial answer set gives no answers"
        );
        assert_eq!(output["sources"].as_array().unwrap().len(), 1);
    }
}

async fn nothing_leaks_to_stdout_or_stderr() {
    let world = World::new();
    let file = world.file("a.log", format!("line {MARKER}\n"));
    let body =
        format!(r#"{{"detail":{{"error_type":"x","message":"{MARKER}"}},"echo":"{MARKER}"}}"#);
    let jev = FakeJev::start(move |index, request| match index {
        0 => Reply::answers(request, |_| 0.5),
        1 => Reply::status(500, body.clone()),
        _ => Reply::status(200, format!("garbage {MARKER} \n\r\u{0} not json")),
    })
    .await;
    let mut client = world.default_server(&jev).await;
    for _ in 0..3 {
        client
            .call("ask", json!({"paths": [file], "questions": ["q?"]}))
            .await;
    }
    // Paths and question text may appear; file content and provider bodies may not.
    let stdout = client.stdout.join("\n");
    let stderr = client.close().await;
    assert!(
        !stdout.contains(MARKER),
        "stdout holds source or provider text"
    );
    assert!(
        !stderr.contains(MARKER),
        "stderr holds source or provider text: {stderr}"
    );
}

async fn malformed_protocol_requests_get_protocol_errors() {
    let world = World::new();
    let file = world.file("a.log", "text");
    let jev = answering_jev().await;
    let mut client = world.default_server(&jev).await;
    let response = client
        .request("tools/call", json!({"name": "nope", "arguments": {}}))
        .await;
    assert!(
        response.get("error").is_some(),
        "unknown tool is a protocol error: {response}"
    );
    let response = client.request("no/such/method", json!({})).await;
    assert_eq!(response["error"]["code"], -32601);
    // The server keeps serving after a bad line.
    client.send_raw("{not json}\n").await;
    let result = client
        .call("ask", json!({"paths": [file], "questions": ["yes?"]}))
        .await;
    assert_eq!(structured(&result)["evaluation_status"], "complete");
}

async fn cancellation_is_recorded() {
    let world = World::new();
    let file = world.file("a.log", "text");
    let jev = FakeJev::start(|_, request| {
        Reply::answers(request, |_| 0.5).delayed(Duration::from_secs(30))
    })
    .await;
    let mut client = world.default_server(&jev).await;
    client
        .send(json!({"jsonrpc": "2.0", "id": 99, "method": "tools/call",
                     "params": {"name": "ask", "arguments": {"paths": [file], "questions": ["q?"]}}}))
        .await;
    while jev.requests() == 0 {
        client.step(Duration::from_millis(50)).await;
    }
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 99}}))
        .await;
    // The cancelled response is not promised to arrive; the record is.
    let asks = world.state.join("asks");
    let started = std::time::Instant::now();
    let record = loop {
        let found = std::fs::read_dir(&asks)
            .ok()
            .and_then(|mut dirs| dirs.next())
            .map(|d| d.unwrap().path().join("record.json"));
        if let Some(text) = found.and_then(|path| std::fs::read_to_string(path).ok()) {
            break serde_json::from_str::<Value>(&text).unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no record after cancellation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(record["error"]["code"], "cancelled");
    assert_eq!(record["requests"].as_array().unwrap().len(), 1);
}

/// The real binary, as Claude Code starts it: no key, so no provider is needed.
async fn production_binary_smoke() {
    let world = World::new();
    let file = world.file("a.log", "text");
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentgrasp"));
    command
        .arg("mcp")
        .env_remove("TYPESAFE_API_KEY")
        .env("XDG_STATE_HOME", world.base.join("xdg"));
    let mut client = McpClient::start(command, Some(vec![world.root.clone()])).await;
    let listed = client.request("tools/list", json!({})).await;
    assert!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "ask")
    );
    let result = client
        .call("ask", json!({"paths": [file], "questions": ["q?"]}))
        .await;
    let output = structured(&result);
    assert_eq!(output["error"]["code"], "provider_unavailable");
    let record = output["record_path"].as_str().unwrap();
    assert!(
        record.starts_with(world.base.join("xdg/agentgrasp/asks").to_str().unwrap()),
        "{record}"
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
}
