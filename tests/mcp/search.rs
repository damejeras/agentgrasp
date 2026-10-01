//! End-to-end tests of the search tool, run by the host in tests/mcp.rs.

use std::time::Duration;

use serde_json::{Value, json};

use super::support::{FakeJev, Reply};
use super::{MARKER, Test, World, structured};

pub const TESTS: &[(&str, Test)] = &[
    ("search_ranks_files_and_reports", || {
        Box::pin(search_ranks_files_and_reports())
    }),
    ("search_limit_and_include", || {
        Box::pin(search_limit_and_include())
    }),
    ("search_keeps_matches_after_a_provider_failure", || {
        Box::pin(search_keeps_matches_after_a_provider_failure())
    }),
    ("search_invalid_input", || Box::pin(search_invalid_input())),
    ("search_missing_key_fails_at_start", || {
        Box::pin(search_missing_key_fails_at_start())
    }),
    ("search_cancellation_is_interrupted", || {
        Box::pin(search_cancellation_is_interrupted())
    }),
];

/// Answers navigation requests by item path, and evidence requests with `evidence`.
fn navigation(request: &Value, score: impl Fn(&str) -> f64) -> Reply {
    let items = request["state"]["items"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let answers: serde_json::Map<String, Value> = request["questions"]
        .as_object()
        .unwrap()
        .keys()
        .map(|key| {
            let index: usize = key[1..].parse().unwrap_or(0);
            let path = items
                .get(index)
                .and_then(|i| i["path"].as_str())
                .unwrap_or("");
            (key.clone(), json!({"type": "noul", "noul": score(path)}))
        })
        .collect();
    Reply::status(200, json!({"model": "jev-nav", "answers": answers, "usage": {"input_tokens": 7, "output_tokens": 1}}).to_string())
}

fn payments_score(path: &str) -> f64 {
    match path {
        "src/payments" => 0.9,
        "src/payments/refunds.go" => 0.97,
        "src/payments/refunds_test.go" => 0.89,
        _ => 0.1,
    }
}

fn payments(world: &World) {
    world.file(
        "src/payments/refunds.go",
        format!("package payments\n\n// {MARKER}\nfunc Refund() {{}}\n"),
    );
    world.file(
        "src/payments/refunds_test.go",
        "package payments\n\nfunc TestRefund() {}\n",
    );
    world.file("src/other/util.go", "package other\n");
    world.file("docs/plan.md", "# Plan\nRefund rules.\n");
    world.file("README.md", "readme\n");
}

fn search_args(world: &World, extra: Value) -> Value {
    let mut args = json!({"question": "Where is the refund limit enforced?", "scope": world.root});
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    args
}

fn not_error(result: &Value) {
    assert!(result.get("isError").is_none_or(|v| v == false), "{result}");
}

async fn search_ranks_files_and_reports() {
    let world = World::new();
    payments(&world);
    let jev = FakeJev::start(|_, request| navigation(request, payments_score)).await;
    let mut client = world.default_server(&jev).await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    not_error(&result);
    let output = structured(&result);
    assert_eq!(output["status"], "complete", "{output}");
    assert_eq!(output["model"], "jev-nav");
    assert_eq!(output["error"], Value::Null);
    let matches = output["matches"].as_array().unwrap();
    let paths: Vec<&str> = matches
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    let root = world.root.to_str().unwrap();
    assert_eq!(
        paths,
        vec![
            format!("{root}/src/payments/refunds.go"),
            format!("{root}/src/payments/refunds_test.go")
        ]
    );
    assert_eq!(matches[0]["relevance"], 0.97);
    let bytes = std::fs::read(world.root.join("src/payments/refunds.go")).unwrap();
    assert_eq!(matches[0]["sha256"], agentgrasp::sha256_hex(&bytes));
    assert_eq!(matches[0]["ranges"], json!([]));
    assert_eq!(output["matches_found"], 2);
    assert_eq!(output["results_limited"], false);
    // README.md, docs/plan.md, refunds.go, refunds_test.go were judged from their previews.
    assert_eq!(output["files_considered"], 4);
    assert_eq!(output["directories_pruned"], 1, "src/other");
    assert_eq!(output["files_skipped"], 0);
    // Jev sees paths relative to the scope, never absolute ones.
    for index in 0..jev.requests() {
        let body = jev.body(index).to_string();
        assert!(!body.contains(root), "a request holds an absolute path");
    }
    let report_path = output["report_path"].as_str().unwrap();
    let text = std::fs::read_to_string(report_path).unwrap();
    assert!(!text.contains(MARKER), "the report holds source text");
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["matches"].as_array().unwrap().len(), 2);
    assert_eq!(report["status"], "complete");
    assert_eq!(report["requests"].as_array().unwrap().len(), jev.requests());
    assert_eq!(report["usage"]["input_tokens"], 7 * jev.requests() as u64);
    assert!(report["exclusions"].as_array().unwrap().len() >= 9);
    assert_eq!(report["constants"]["threshold"], 0.5);
    let stdout = client.stdout.join("\n");
    let stderr = client.close().await;
    assert!(!stdout.contains(MARKER) && !stderr.contains(MARKER));
}

async fn search_limit_and_include() {
    let world = World::new();
    payments(&world);
    let jev = FakeJev::start(|_, request| navigation(request, payments_score)).await;
    let mut client = world.default_server(&jev).await;
    let result = client
        .call("search", search_args(&world, json!({"limit": 1})))
        .await;
    let output = structured(&result);
    assert_eq!(output["matches"].as_array().unwrap().len(), 1);
    assert_eq!(output["matches_found"], 2);
    assert_eq!(output["results_limited"], true);
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(output["report_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        report["matches"].as_array().unwrap().len(),
        2,
        "the report keeps every match"
    );

    let result = client
        .call(
            "search",
            search_args(
                &world,
                json!({"include": ["**/*_test.go"], "exclude": ["docs/**"]}),
            ),
        )
        .await;
    let output = structured(&result);
    assert_eq!(output["files_considered"], 1, "{output}");
    assert_eq!(
        output["matches"][0]["path"],
        format!("{}/src/payments/refunds_test.go", world.root.display())
    );
}

async fn search_keeps_matches_after_a_provider_failure() {
    let world = World::new();
    payments(&world);
    world.file("src/payments/late.go", "package payments\n");
    // Any batch that holds late.go fails with a passing error, so it is split until late.go
    // fails alone. The other halves succeed.
    let jev = FakeJev::start(|_, request| {
        let has_late = request["state"]["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|i| i["path"] == "src/payments/late.go"));
        if has_late {
            Reply::status(500, "{}")
        } else {
            navigation(request, payments_score)
        }
    })
    .await;
    let mut client = world.default_server(&jev).await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    not_error(&result);
    let output = structured(&result);
    assert_eq!(output["status"], "incomplete", "{output}");
    assert_eq!(output["error"]["code"], "provider_unavailable");
    assert_eq!(
        output["matches_found"], 2,
        "matches found before the failure stay"
    );
    assert_eq!(
        output["issues"],
        json!([{"kind": "provider", "code": "provider_unavailable", "count": 1}]),
        "a recovered batch is not a failure"
    );
    let report_path = output["report_path"].as_str().unwrap();
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(report_path).unwrap()).unwrap();
    let late = format!("{}/src/payments/late.go", world.root.display());
    assert_eq!(
        report["issue_locations"],
        json!([{"kind": "provider", "code": "provider_unavailable", "path": late}])
    );
}

async fn search_invalid_input() {
    let world = World::new();
    payments(&world);
    std::fs::write(world.base.join("outside.txt"), "x").unwrap();
    let jev = FakeJev::start(|_, request| navigation(request, payments_score)).await;
    let mut client = world.default_server(&jev).await;
    let root = world.root.to_str().unwrap().to_string();
    let cases = [
        json!({"question": "", "scope": root}),
        json!({"question": "q", "scope": "relative/dir"}),
        json!({"question": "q", "scope": world.base}),
        json!({"question": "q", "scope": world.root.join("README.md")}),
        json!({"question": "q", "scope": root, "limit": 0}),
        json!({"question": "q", "scope": root, "limit": 101}),
        json!({"question": "q", "scope": root, "include": ["/abs/*.go"]}),
        json!({"question": "q", "scope": root, "exclude": ["{unclosed"]}),
        json!({"question": "q", "scope": root, "extra": true}),
        json!({"scope": root}),
    ];
    for arguments in cases {
        let result = client.call("search", arguments.clone()).await;
        assert_eq!(result["isError"], true, "{arguments}: {result}");
        let output = structured(&result);
        assert_eq!(output["error"]["code"], "invalid_input");
        assert_eq!(output["report_path"], Value::Null);
    }
    assert_eq!(jev.requests(), 0);
}

async fn search_missing_key_fails_at_start() {
    let world = World::new();
    payments(&world);
    let jev = FakeJev::start(|_, request| navigation(request, payments_score)).await;
    let mut client = world
        .server(&jev, None, Some(vec![world.root.clone()]))
        .await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    not_error(&result);
    let output = structured(&result);
    assert_eq!(output["status"], "incomplete");
    assert_eq!(output["error"]["code"], "provider_unavailable");
    assert_eq!(output["matches"], json!([]));
    assert!(output["report_path"].is_string(), "the report is written");
    assert_eq!(jev.requests(), 0);
}

async fn search_cancellation_is_interrupted() {
    let world = World::new();
    payments(&world);
    let jev = FakeJev::start(|_, request| {
        navigation(request, payments_score).delayed(Duration::from_secs(30))
    })
    .await;
    let mut client = world.default_server(&jev).await;
    client
        .send(json!({"jsonrpc": "2.0", "id": 77, "method": "tools/call",
                     "params": {"name": "search", "arguments": search_args(&world, json!({}))}}))
        .await;
    while jev.requests() == 0 {
        client.step(Duration::from_millis(50)).await;
    }
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 77}}))
        .await;
    let searches = world.state.join("searches");
    let started = std::time::Instant::now();
    let report = loop {
        let found = std::fs::read_dir(&searches)
            .ok()
            .and_then(|mut dirs| dirs.next())
            .map(|d| d.unwrap().path().join("report.json"));
        if let Some(text) = found.and_then(|path| std::fs::read_to_string(path).ok()) {
            break serde_json::from_str::<Value>(&text).unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "no report after cancellation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(report["status"], "interrupted");
    assert_eq!(report["error"]["code"], "cancelled");
}
