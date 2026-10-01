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
    ("search_a_recovered_split_is_complete", || {
        Box::pin(search_a_recovered_split_is_complete())
    }),
    ("search_an_outage_is_not_split", || {
        Box::pin(search_an_outage_is_not_split())
    }),
    ("search_shows_five_ranges_and_reports_all", || {
        Box::pin(search_shows_five_ranges_and_reports_all())
    }),
    ("search_keeps_a_file_when_selection_fails", || {
        Box::pin(search_keeps_a_file_when_selection_fails())
    }),
    ("search_contextual_pass_adds_and_retracts", || {
        Box::pin(search_contextual_pass_adds_and_retracts())
    }),
    ("search_failed_contextual_pass_keeps_ranges", || {
        Box::pin(search_failed_contextual_pass_keeps_ranges())
    }),
    ("search_rediscovers_related_directories", || {
        Box::pin(search_rediscovers_related_directories())
    }),
    ("search_invalid_input", || Box::pin(search_invalid_input())),
    ("search_missing_key_fails_at_start", || {
        Box::pin(search_missing_key_fails_at_start())
    }),
    ("search_cancellation_is_interrupted", || {
        Box::pin(search_cancellation_is_interrupted())
    }),
];

/// Answers navigation requests by item path, and evidence requests by declaration name:
/// relevance and scope from `evidence`, and no references.
fn navigation(request: &Value, score: impl Fn(&str) -> f64) -> Reply {
    if let Some(declarations) = request["state"]["declarations"].as_array() {
        let answers: serde_json::Map<String, Value> = request["questions"]
            .as_object()
            .unwrap()
            .keys()
            .map(|key| {
                let digits = key.trim_start_matches(|c: char| c.is_ascii_alphabetic());
                let name = declarations[digits.parse::<usize>().unwrap()]["name"]
                    .as_str()
                    .unwrap();
                // Relevance is above scope, so the score shows that the smaller one counts.
                let p = if key.starts_with("ref") {
                    0.0
                } else if key.starts_with("scope") {
                    evidence(name)
                } else {
                    (evidence(name) + 0.04).min(1.0)
                };
                (key.clone(), json!({"type": "noul", "noul": p}))
            })
            .collect();
        return Reply::status(200, json!({"model": "jev-sel", "answers": answers, "usage": {"input_tokens": 5, "output_tokens": 1}}).to_string());
    }
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

fn evidence(name: &str) -> f64 {
    match name {
        "Refund" => 0.93,
        name if name.starts_with("Check") => 0.8,
        _ => 0.1,
    }
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
    assert!(output["model"].as_str().unwrap().starts_with("jev-"));
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
    assert_eq!(
        matches[0]["ranges"],
        json!([{"start_line": 4, "end_line": 4, "relevance": 0.93}])
    );
    assert_eq!(
        matches[1]["ranges"],
        json!([]),
        "a file with no qualifying range stays a match"
    );
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
    let summed: u64 = report["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["usage"]["input_tokens"].as_u64().unwrap())
        .sum();
    assert_eq!(report["usage"]["input_tokens"], summed);
    assert!(report["requests"][0]["latency_ms"].is_u64());
    assert!(report["exclusions"].as_array().unwrap().len() >= 9);
    assert_eq!(report["constants"]["threshold"], 0.5);
    let selection = &report["constants"]["selection"];
    assert_eq!(selection["source_unit_bytes"], 24_000);
    assert_eq!(selection["fallback_unit_bytes"], 3_000);
    assert_eq!(selection["block_lines"], 16);
    assert_eq!(selection["group_bytes"], 42_000);
    assert_eq!(selection["group_units"], 128);
    assert_eq!(selection["state_bytes"], 80_000);
    assert_eq!(selection["whole_source_bytes"], 16_000);
    assert_eq!(selection["context_lines"], 8);
    assert_eq!(selection["opening_lines"], 20);
    assert_eq!(report["constants"]["max_evidence_bytes"], 64_000);
    assert_eq!(report["constants"]["sampled_preview_bytes"], 28_000);
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
    // Any batch that holds late.go is overloaded, so it is split until late.go fails alone,
    // after its one retry. The other halves succeed.
    let jev = FakeJev::start(|_, request| {
        let has_late = request["state"]["items"]
            .as_array()
            .is_some_and(|items| items.iter().any(|i| i["path"] == "src/payments/late.go"));
        if has_late {
            Reply::status(529, "{}").header("retry-after", "0")
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

async fn search_shows_five_ranges_and_reports_all() {
    let world = World::new();
    let checks: String = (1..=7)
        .map(|i| format!("\nfunc Check{i}() {{\n\treturn\n}}\n"))
        .collect();
    world.file(
        "src/payments/refunds.go",
        format!("package payments\n{checks}"),
    );
    let jev = FakeJev::start(|_, request| navigation(request, payments_score)).await;
    let mut client = world.default_server(&jev).await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    let output = structured(&result);
    let ranges = output["matches"][0]["ranges"].as_array().unwrap();
    assert_eq!(ranges.len(), 5);
    // Equal relevance: by start line. Check1 is on lines 3-5 of the hashed snapshot.
    assert_eq!(
        ranges[0],
        json!({"start_line": 3, "end_line": 5, "relevance": 0.8})
    );
    assert_eq!(
        ranges[4],
        json!({"start_line": 19, "end_line": 21, "relevance": 0.8})
    );
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(output["report_path"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(report["matches"][0]["ranges"].as_array().unwrap().len(), 7);
}

async fn search_keeps_a_file_when_selection_fails() {
    let world = World::new();
    payments(&world);
    let jev = FakeJev::start(|_, request| {
        if request["state"]["declarations"].is_array() {
            Reply::status(200, "not json")
        } else {
            navigation(request, payments_score)
        }
    })
    .await;
    let mut client = world.default_server(&jev).await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    let output = structured(&result);
    assert_eq!(output["status"], "incomplete");
    assert_eq!(output["error"]["code"], "invalid_provider_response");
    assert_eq!(
        output["matches_found"], 2,
        "the files stay matches without ranges"
    );
    assert_eq!(output["matches"][0]["ranges"], json!([]));
}

/// Answers evidence requests per declaration name, with a different rule once selected
/// evidence is present.
fn contextual(
    request: &Value,
    first: impl Fn(&str) -> f64,
    second: impl Fn(&str, &str) -> f64,
) -> Reply {
    let declarations = request["state"]["declarations"].as_array().unwrap();
    let with_evidence = request["state"].get("selectedEvidence").is_some();
    let answers: serde_json::Map<String, Value> = request["questions"]
        .as_object()
        .unwrap()
        .keys()
        .map(|key| {
            let kind = key.trim_end_matches(|c: char| c.is_ascii_digit());
            let index: usize = key[kind.len()..].parse().unwrap();
            let name = declarations[index]["name"].as_str().unwrap();
            let p = if with_evidence {
                second(name, kind)
            } else if kind == "ref" {
                0.0
            } else {
                first(name)
            };
            (key.clone(), json!({"type": "noul", "noul": p}))
        })
        .collect();
    Reply::status(200, json!({"model": "jev-ctx", "answers": answers, "usage": {"input_tokens": 3, "output_tokens": 1}}).to_string())
}

fn ledger(world: &World) {
    world.file(
        "src/payments/refunds.go",
        "package payments\n\nfunc Refund() {\n\tHelper()\n}\n\nfunc Helper() {}\n\nfunc Stale() {}\n",
    );
}

fn first_pass(name: &str) -> f64 {
    if matches!(name, "Refund" | "Stale") {
        0.9
    } else {
        0.1
    }
}

async fn search_contextual_pass_adds_and_retracts() {
    let world = World::new();
    ledger(&world);
    let jev = FakeJev::start(|_, request| {
        if request["state"]["declarations"].is_array() {
            contextual(request, first_pass, |name, kind| match (name, kind) {
                ("Refund", "q" | "scope") => 0.9,
                ("Helper", "ref") => 0.92,
                _ => 0.1,
            })
        } else {
            navigation(request, payments_score)
        }
    })
    .await;
    let mut client = world.default_server(&jev).await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    let output = structured(&result);
    assert_eq!(output["status"], "complete", "{output}");
    assert_eq!(
        output["matches"][0]["ranges"],
        json!([
            {"start_line": 7, "end_line": 7, "relevance": 0.92},
            {"start_line": 3, "end_line": 5, "relevance": 0.9},
        ]),
        "Helper joins through the reference; Stale is retracted"
    );
    // The contextual requests carry the first pass's excerpts, with their source.
    let contextual: Vec<Value> = (0..jev.requests())
        .map(|i| jev.body(i))
        .filter(|b| b["state"].get("selectedEvidence").is_some())
        .collect();
    assert_eq!(contextual.len(), 1);
    let evidence = contextual[0]["state"]["selectedEvidence"]
        .as_array()
        .unwrap();
    assert_eq!(evidence[0]["path"], "src/payments/refunds.go");
    assert!(
        evidence
            .iter()
            .any(|e| e["source"].as_str().unwrap().contains("func Refund()"))
    );
    assert!(contextual[0]["questions"].get("ref0").is_some());
    let report = std::fs::read_to_string(output["report_path"].as_str().unwrap()).unwrap();
    assert!(!report.contains("func Refund"), "excerpts are never stored");
}

async fn search_a_recovered_split_is_complete() {
    let world = World::new();
    payments(&world);
    // The first batch of several items is overloaded once; its halves succeed.
    let overloaded = std::sync::atomic::AtomicBool::new(false);
    let jev = FakeJev::start(move |_, request| {
        let items = request["state"]["items"].as_array().map_or(0, |i| i.len());
        if items > 1 && !overloaded.swap(true, std::sync::atomic::Ordering::SeqCst) {
            Reply::status(529, "{}")
        } else {
            navigation(request, payments_score)
        }
    })
    .await;
    let mut client = world.default_server(&jev).await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    let output = structured(&result);
    assert_eq!(output["status"], "complete", "{output}");
    assert_eq!(output["issues"], json!([]));
    assert_eq!(output["matches_found"], 2);
}

async fn search_an_outage_is_not_split() {
    let world = World::new();
    payments(&world);
    let jev = FakeJev::start(|_, request| {
        if request["state"]["items"]
            .as_array()
            .is_some_and(|i| i.len() > 1)
        {
            Reply::status(500, "{}")
        } else {
            navigation(request, payments_score)
        }
    })
    .await;
    let mut client = world.default_server(&jev).await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    let output = structured(&result);
    assert_eq!(output["status"], "incomplete");
    let singles = (0..jev.requests())
        .filter(|&i| {
            jev.body(i)["state"]["items"]
                .as_array()
                .is_some_and(|items| items.len() == 1)
        })
        .count();
    assert_eq!(singles, 0, "a 500 does not split a batch into new requests");
}

async fn search_failed_contextual_pass_keeps_ranges() {
    let world = World::new();
    ledger(&world);
    let jev = FakeJev::start(|_, request| {
        if request["state"].get("selectedEvidence").is_some() {
            Reply::status(500, "{}")
        } else if request["state"]["declarations"].is_array() {
            contextual(request, first_pass, |_, _| 0.0)
        } else {
            navigation(request, payments_score)
        }
    })
    .await;
    let mut client = world.default_server(&jev).await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    let output = structured(&result);
    assert_eq!(output["status"], "incomplete");
    assert_eq!(output["error"]["code"], "provider_unavailable");
    let ranges: Vec<u64> = output["matches"][0]["ranges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["start_line"].as_u64().unwrap())
        .collect();
    assert_eq!(
        ranges,
        vec![3, 9],
        "the first pass stays when the contextual pass fails"
    );
}

async fn search_rediscovers_related_directories() {
    let world = World::new();
    world.file(
        "src/core/base.py",
        "class Base:\n    def run(self):\n        return 1\n",
    );
    world.file(
        "ext/plugins/impl.py",
        "from core.base import Base\n\nclass Impl(Base):\n    def run(self):\n        return 2\n",
    );
    let jev = FakeJev::start(|_, request| {
        if request["state"]["declarations"].is_array() {
            return contextual(request, |_| 0.1, |_, _| 0.1);
        }
        let anchored = request["state"].get("relationAnchor").is_some();
        navigation(request, move |path| match path {
            "src/core" | "src/core/base.py" => 0.9,
            "ext/plugins" if anchored => 0.9,
            "ext/plugins/impl.py" if anchored => 0.8,
            _ => 0.1,
        })
    })
    .await;
    let mut client = world.default_server(&jev).await;
    let result = client.call("search", search_args(&world, json!({}))).await;
    let output = structured(&result);
    let paths: Vec<&str> = output["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    let root = world.root.to_str().unwrap();
    assert_eq!(
        paths,
        vec![
            format!("{root}/src/core/base.py"),
            format!("{root}/ext/plugins/impl.py")
        ]
    );
    let anchored: Vec<Value> = (0..jev.requests())
        .map(|i| jev.body(i))
        .filter(|b| b["state"].get("relationAnchor").is_some())
        .collect();
    let first = &anchored[0]["state"];
    assert_eq!(
        first["relationAnchor"],
        json!({"path": "src/core/base.py", "classes": ["Base"]})
    );
    let samples = &first["items"][0]["childPreview"]["contentSamples"];
    assert_eq!(samples[0]["name"], "impl.py");
    assert!(
        samples[0]["source"]
            .as_str()
            .unwrap()
            .contains("class Impl(Base)")
    );
    assert!(
        anchored[0]["questions"]["q0"]["instructions"]
            .as_str()
            .unwrap()
            .contains("relationAnchor.classes")
    );
    assert_eq!(
        output["directories_pruned"], 1,
        "ext/plugins was pruned before the anchor found it"
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
        let report: Value = serde_json::from_str(
            &std::fs::read_to_string(output["report_path"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(report["error"]["code"], "invalid_input");
        assert_eq!(report["status"], "incomplete");
        assert_eq!(report["coverage"]["files_considered"], 0);
        assert_eq!(report["usage"]["input_tokens"], 0);
        assert_eq!(report["matches"], json!([]));
        assert!(report["constants"]["threshold"].is_number());
        assert!(report["exclusions"].as_array().unwrap().len() >= 9);
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
    let jev = FakeJev::start(|_, request| navigation(request, payments_score).never()).await;
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
        // The record is created before it is written: wait until it parses.
        let parsed = found
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        if let Some(record) = parsed {
            break record;
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
