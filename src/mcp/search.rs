//! The `search` tool: files and source regions likely to help investigate a question.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use rmcp::model::JsonObject;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, schemars};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::locations;
use super::{Code, Config, ToolError};
use crate::jev::{self, Usage};
use crate::search::evaluator::{self, Evaluator};
use crate::search::fs::{self, Filesystem, Policy};
use crate::search::retrieve::{self, Context, Found, IssueCount};
use crate::search::selection;
use crate::state::{self, Kind};

pub const DESCRIPTION: &str = "\
Finds files and source regions likely to help investigate a natural-language question: \
implementations, callers, tests, fixtures and configuration. It returns locations and \
relevance scores only, never source text or a generated answer; read the files yourself. \
status 'complete' means the search policy finished within its scope and exclusions; it does \
not mean that every relevant file was found. search sends file content to TypeSafe's Jev \
model and writes a report under the agentgrasp state directory.";

pub const DEFAULT_LIMIT: u64 = 10;
pub const MAX_LIMIT: u64 = 100;
/// The ranges returned per file; the report keeps all of them.
pub const RANGES_SHOWN: usize = 5;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// The question to investigate, in natural language.
    pub question: String,
    /// Absolute path of a directory inside an MCP root.
    pub scope: String,
    /// Globs relative to scope; a file must match one. Omitted or empty means all eligible
    /// files. `/` separates names; `*` and `?` do not match it, `**` does.
    #[serde(default)]
    pub include: Vec<String>,
    /// Globs relative to scope, added to the default exclusions. Exclusions win.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// The most files returned. It does not limit discovery. Default 10.
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u64>,
}

#[derive(Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Complete,
    Incomplete,
    Interrupted,
}

#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
pub struct RangeMatch {
    pub start_line: usize,
    pub end_line: usize,
    pub relevance: f64,
}

#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
pub struct Match {
    pub path: String,
    pub relevance: f64,
    pub sha256: String,
    pub ranges: Vec<RangeMatch>,
}

#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
pub struct Issue {
    pub kind: String,
    pub code: Code,
    pub count: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct Output {
    pub question: String,
    pub scope: String,
    pub status: Status,
    /// The model that Jev reports it used; null when no request succeeded.
    pub model: Option<String>,
    /// Qualifying files, most relevant first, at most `limit`.
    pub matches: Vec<Match>,
    pub files_considered: usize,
    pub directories_pruned: usize,
    pub files_skipped: u64,
    pub matches_found: usize,
    pub results_limited: bool,
    pub issues: Vec<Issue>,
    pub error: Option<ToolError>,
    pub report_path: Option<String>,
}

impl Output {
    fn start_failure(question: String, scope: String, error: ToolError) -> Output {
        Output {
            question,
            scope,
            status: Status::Incomplete,
            model: None,
            matches: Vec::new(),
            files_considered: 0,
            directories_pruned: 0,
            files_skipped: 0,
            matches_found: 0,
            results_limited: false,
            issues: Vec::new(),
            error: Some(error),
            report_path: None,
        }
    }
}

pub async fn call(
    config: &Config,
    arguments: JsonObject,
    context: &RequestContext<RoleServer>,
) -> Output {
    let raw_question = arguments
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let raw_scope = arguments
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let invalid = |message: String| {
        Output::start_failure(
            raw_question.clone(),
            raw_scope.clone(),
            ToolError::new(Code::InvalidInput, message),
        )
    };
    let input: Input = match serde_json::from_value(arguments.clone().into()) {
        Ok(input) => input,
        Err(error) => return invalid(format!("arguments do not match the input schema: {error}")),
    };
    if input.question.trim().is_empty() {
        return invalid("question is empty".into());
    }
    let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return invalid(format!("limit must be from 1 to {MAX_LIMIT}"));
    }
    let include = match fs::globs(&input.include) {
        Ok(set) => set,
        Err(message) => return invalid(format!("include {message}")),
    };
    let exclude = match fs::exclude_globs(&input.exclude) {
        Ok(set) => set,
        Err(message) => return invalid(format!("exclude {message}")),
    };
    let roots = super::roots(&context.peer).await;
    if roots.is_empty() {
        return invalid("the client gave no MCP roots, so no scope is allowed".into());
    }
    let scope = match locations::allow_scope(Path::new(&input.scope), &roots) {
        Ok(scope) => scope,
        Err(denied) => return invalid(format!("scope {denied}")),
    };
    if !scope.is_dir() {
        return invalid("scope is not a directory".into());
    }
    let started_at = SystemTime::now();
    let clock = Instant::now();
    let mut output = Output::start_failure(
        input.question.clone(),
        input.scope.clone(),
        ToolError::new(Code::ProviderUnavailable, ""),
    );
    let mut found: Option<Found> = None;
    let mut attempts = Vec::new();
    match &config.key {
        None => {
            output.error = Some(ToolError::new(
                Code::ProviderUnavailable,
                "TYPESAFE_API_KEY is not set",
            ));
            output.issues = vec![Issue {
                kind: "provider".into(),
                code: Code::ProviderUnavailable,
                count: 1,
            }];
        }
        Some(key) => {
            let mut protected = vec![config.state_root.clone()];
            if let Ok(real) = std::fs::canonicalize(&config.state_root) {
                protected.push(real);
            }
            let filesystem = Filesystem::new(Policy {
                scope: scope.clone(),
                include,
                exclude,
                protected,
            });
            let evaluator =
                Evaluator::new(jev::Client::new(&config.endpoint, key), context.ct.clone());
            let search = Context::new(
                input.question.clone(),
                filesystem,
                evaluator,
                context.ct.clone(),
            );
            let result = search.run().await;
            attempts = search.evaluator.attempts();
            output = output_from(&input, &scope, &result, search.evaluator.model(), limit);
            found = Some(result);
        }
    }
    let report = report(
        &input,
        &scope,
        limit,
        &output,
        found.as_ref(),
        &attempts,
        started_at,
        clock,
    );
    match write_report(&config.state_root, &report) {
        Ok(path) => output.report_path = Some(path.to_string_lossy().into_owned()),
        Err(error) => eprintln!("agentgrasp mcp: search report not written: {error:#}"),
    }
    output
}

fn absolute(scope: &Path, relative: &str) -> String {
    scope.join(relative).to_string_lossy().into_owned()
}

fn output_from(
    input: &Input,
    scope: &Path,
    found: &Found,
    model: Option<String>,
    limit: u64,
) -> Output {
    let matches: Vec<Match> = found
        .files
        .iter()
        .take(limit as usize)
        .map(|file| Match {
            path: absolute(scope, &file.path),
            relevance: file.score,
            sha256: file.sha256.clone(),
            ranges: file
                .ranges
                .iter()
                .take(RANGES_SHOWN)
                .map(|r| RangeMatch {
                    start_line: r.start_line,
                    end_line: r.end_line,
                    relevance: r.relevance,
                })
                .collect(),
        })
        .collect();
    let status = if found.interrupted {
        Status::Interrupted
    } else if found.issues.is_empty() {
        Status::Complete
    } else {
        Status::Incomplete
    };
    Output {
        question: input.question.clone(),
        scope: input.scope.clone(),
        status,
        model,
        matches,
        files_considered: found.files_considered,
        directories_pruned: found.directories_pruned,
        files_skipped: found.files_skipped,
        matches_found: found.files.len(),
        results_limited: found.files.len() > limit as usize,
        issues: found.issues.iter().map(issue).collect(),
        error: found.error.clone(),
        report_path: None,
    }
}

fn issue(count: &IssueCount) -> Issue {
    Issue {
        kind: count.kind.to_string(),
        code: count.code,
        count: count.count,
    }
}

#[allow(clippy::too_many_arguments)]
fn report(
    input: &Input,
    scope: &Path,
    limit: u64,
    output: &Output,
    found: Option<&Found>,
    attempts: &[jev::Attempt],
    started_at: SystemTime,
    clock: Instant,
) -> serde_json::Value {
    let usage = attempts
        .iter()
        .filter_map(|a| a.usage)
        .fold(Usage::default(), |sum, u| Usage {
            input_tokens: sum.input_tokens + u.input_tokens,
            output_tokens: sum.output_tokens + u.output_tokens,
        });
    let matches: Vec<serde_json::Value> = found
        .map(|f| {
            f.files
                .iter()
                .map(|file| {
                    json!({
                        "path": absolute(scope, &file.path),
                        "relevance": file.score,
                        "sha256": file.sha256,
                        "ranges": file.ranges,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut exclusions: Vec<String> = fs::EXCLUSIONS.iter().map(|e| e.to_string()).collect();
    exclusions.extend(input.exclude.iter().map(|p| format!("exclude pattern {p}")));
    let constants = json!({
        "model": jev::MODEL,
        "threshold": retrieve::THRESHOLD,
        "max_entries": retrieve::MAX_ENTRIES,
        "max_navigation_request_bytes": retrieve::MAX_NAVIGATION_BYTES,
        "max_batch_items": retrieve::MAX_BATCH_ITEMS,
        "max_inspected_file_bytes": retrieve::MAX_INSPECTED_BYTES,
        "max_file_bytes": fs::MAX_FILE_BYTES,
        "preview_bytes": retrieve::PREVIEW_BYTES,
        "chunk_bytes": retrieve::CHUNK_BYTES,
        "request_limit": evaluator::REQUEST_LIMIT,
        "concurrency": evaluator::CONCURRENCY,
        "tokens_per_second": evaluator::TOKENS_PER_SECOND,
        "requests_per_minute": evaluator::REQUESTS_PER_MINUTE,
        "request_timeout_seconds": jev::REQUEST_TIMEOUT.as_secs(),
        "ranges_shown": RANGES_SHOWN,
        "selection": {
            "source_unit_bytes": selection::SOURCE_UNIT_BYTES,
            "fallback_unit_bytes": selection::FALLBACK_UNIT_BYTES,
            "block_lines": selection::BLOCK_LINES,
            "group_bytes": selection::GROUP_BYTES,
            "group_units": selection::GROUP_UNITS,
            "state_bytes": selection::STATE_BYTES,
            "whole_source_bytes": selection::WHOLE_SOURCE_BYTES,
            "context_lines": selection::CONTEXT_LINES,
            "opening_lines": selection::OPENING_LINES,
        },
    });
    json!({
        "started_at": state::rfc3339(started_at),
        "latency_ms": clock.elapsed().as_millis() as u64,
        "question": input.question,
        "scope": input.scope,
        "resolved_scope": scope,
        "include": input.include,
        "exclude": input.exclude,
        "limit": limit,
        "status": output.status,
        "model": output.model,
        "matches": matches,
        "coverage": {
            "files_considered": output.files_considered,
            "directories_pruned": output.directories_pruned,
            "files_skipped": output.files_skipped,
            "matches_found": output.matches_found,
            "results_limited": output.results_limited,
        },
        "issues": output.issues,
        "issue_locations": found.map(|f| f.locations.iter().map(|l| json!({
            "kind": l.kind,
            "code": l.code,
            "path": l.path.as_ref().map(|p| absolute(scope, p)),
        })).collect::<Vec<_>>()).unwrap_or_default(),
        "error": output.error,
        "exclusions": exclusions,
        "constants": constants,
        "usage": usage,
        "requests": attempts,
    })
}

fn write_report(state_root: &Path, report: &serde_json::Value) -> anyhow::Result<PathBuf> {
    let dir = state::allocate(state_root, Kind::Search)?;
    let path = dir.join("report.json");
    let mut text = serde_json::to_vec_pretty(report)?;
    text.push(b'\n');
    state::write_new(&path, &text)?;
    Ok(path)
}
