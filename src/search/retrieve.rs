// Ported from jevgrep (https://github.com/dzhng/jevgrep) at commit
// 82ef1fd3f43161bb17395dba1e07a5338f3913db, packages/core/src/retrieve.ts. Its file
// assessment, local call context, test-body selection and repository context are not ported:
// they only shape jevgrep's text output, never which files and regions qualify.
// Copyright (c) David Zhang. MIT License; see LICENSE-jevgrep.

//! The search policy: navigate the tree with Jev, keep the files it qualifies.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::evaluator::{EvalError, Evaluator};
use super::fs::{self, Entry, FileRead, Filesystem, Snapshot};
use super::preview;
use super::requests::{
    ChildEntry, ContentSample, Declaration, DirectoryPreview, Evidence, FilePreview, Kind,
    NavigationItem, RelationAnchor, navigation_request,
};
use super::selection::{Selection, select_file};
use super::source;
use crate::mcp::{Code, ToolError};

/// The most directory entries one search looks at.
pub const MAX_ENTRIES: usize = 100_000;
/// The largest navigation request, in bytes.
pub const MAX_NAVIGATION_BYTES: usize = 38_000;
/// The most items in one navigation request.
pub const MAX_BATCH_ITEMS: usize = 128;
/// Files above this size are scored from their preview but not inspected further.
pub const MAX_INSPECTED_BYTES: usize = 1_000_000;
/// A file preview: the opening bytes, and its JSON bound.
pub const PREVIEW_BYTES: usize = 16_384;
pub const PREVIEW_JSON_BYTES: usize = 24_000;
/// The bound of a whole file preview with its declaration index.
pub const PREVIEW_WITH_INDEX_BYTES: usize = 32_000;
/// The chunks a file is split into when its preview is too large for one request.
pub const CHUNK_BYTES: usize = 12_000;
/// A directory preview: the most entries and their JSON bytes.
pub const DIRECTORY_ENTRIES: usize = 64;
pub const DIRECTORY_ENTRY_BYTES: usize = 4096;
/// A probability above this qualifies.
pub const THRESHOLD: f64 = 0.5;
/// The most selected evidence a contextual pass sends, in JSON bytes.
pub const MAX_EVIDENCE_BYTES: usize = 64_000;
/// The class names of a relation anchor, in JSON bytes, must be fewer than this.
pub const MAX_ANCHOR_BYTES: usize = 4_000;
/// Content samples of a directory: their share of characters, the least one sample keeps,
/// and the JSON bound of the sampled preview.
pub const SAMPLE_BYTES: usize = 16_000;
pub const MIN_SAMPLE_CHARS: usize = 80;
pub const SAMPLED_PREVIEW_BYTES: usize = 28_000;

/// A failure, counted by kind and code.
#[derive(Clone, Debug, Serialize)]
pub struct IssueCount {
    pub kind: &'static str,
    pub code: Code,
    pub count: u64,
}

/// Where a failure happened, for the report.
#[derive(Clone, Debug, Serialize)]
pub struct IssueLocation {
    pub kind: &'static str,
    pub code: Code,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Default)]
struct Issues {
    counts: Vec<IssueCount>,
    locations: Vec<IssueLocation>,
    first: Option<ToolError>,
    skipped: u64,
}

impl Issues {
    /// Counts one failure, at each of `paths` (none for a failure without a place).
    fn add(&mut self, kind: &'static str, code: Code, message: String, paths: Vec<String>) {
        match self
            .counts
            .iter_mut()
            .find(|c| c.kind == kind && c.code == code)
        {
            Some(count) => count.count += 1,
            None => self.counts.push(IssueCount {
                kind,
                code,
                count: 1,
            }),
        }
        if paths.is_empty() {
            self.locations.push(IssueLocation {
                kind,
                code,
                path: None,
            });
        }
        for path in paths {
            self.locations.push(IssueLocation {
                kind,
                code,
                path: Some(path),
            });
        }
        self.first.get_or_insert(ToolError::new(code, message));
    }
}

/// A region of a file that qualifies on its own. Lines from 1, both ends included.
#[derive(Clone, Debug, Serialize)]
pub struct FileRange {
    pub start_line: usize,
    pub end_line: usize,
    pub relevance: f64,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: String,
    pub sha256: String,
    /// The highest navigation probability a preview of the file got.
    pub score: f64,
    /// Most relevant first, then by start line, then by end line.
    pub ranges: Vec<FileRange>,
}

#[derive(Default)]
struct State {
    issues: Issues,
    candidates: BTreeMap<String, Candidate>,
    previews: HashMap<String, FilePreview>,
    visited: HashSet<String>,
    pruned: Vec<NavigationItem>,
    entries_seen: usize,
    considered: HashSet<String>,
    stop: bool,
}

pub struct Context {
    pub query: String,
    pub fs: Filesystem,
    pub evaluator: Evaluator,
    pub cancel: CancellationToken,
    state: Mutex<State>,
}

/// What a search found.
pub struct Found {
    /// Qualifying files, best first, then by path.
    pub files: Vec<Candidate>,
    pub files_considered: usize,
    pub directories_pruned: usize,
    pub files_skipped: u64,
    pub issues: Vec<IssueCount>,
    pub locations: Vec<IssueLocation>,
    pub error: Option<ToolError>,
    pub interrupted: bool,
}

impl Context {
    pub fn new(
        query: String,
        fs: Filesystem,
        evaluator: Evaluator,
        cancel: CancellationToken,
    ) -> Arc<Context> {
        Arc::new(Context {
            query,
            fs,
            evaluator,
            cancel,
            state: Mutex::new(State::default()),
        })
    }

    fn stopped(&self) -> bool {
        self.cancel.is_cancelled() || self.state.lock().unwrap().stop
    }

    fn issue(
        &self,
        kind: &'static str,
        code: Code,
        message: impl Into<String>,
        paths: Vec<String>,
    ) {
        let mut state = self.state.lock().unwrap();
        state.issues.add(kind, code, message.into(), paths);
        if matches!(kind, "authentication" | "budget" | "cancelled") {
            state.stop = true;
        }
    }

    /// Records a failed evaluation of the items at `paths`.
    fn eval_issue(&self, error: &EvalError, paths: Vec<String>) {
        match error {
            EvalError::Unauthorized => self.issue(
                "authentication",
                Code::ProviderUnavailable,
                "TypeSafe rejected the API key",
                paths,
            ),
            EvalError::BudgetExhausted => self.issue(
                "budget",
                Code::BudgetExhausted,
                format!(
                    "the search used its {} Jev requests",
                    super::evaluator::REQUEST_LIMIT
                ),
                paths,
            ),
            EvalError::Cancelled => self.issue(
                "cancelled",
                Code::Cancelled,
                "the search was cancelled",
                paths,
            ),
            EvalError::Provider { failure, .. } => self.issue(
                "provider",
                Code::from_jev(failure),
                failure.to_string(),
                paths,
            ),
        }
    }

    /// `skipped`: a candidate file could not be processed, as opposed to a directory or an
    /// ignore file.
    fn file_issue(&self, kind: fs::IssueKind, path: &str, skipped: bool) {
        let (name, code, message) = match kind {
            fs::IssueKind::Unreadable => (
                "unreadable",
                Code::SourceUnreadable,
                "a file or directory cannot be read",
            ),
            fs::IssueKind::Changed => (
                "changed",
                Code::SourceChanged,
                "a file changed while it was read",
            ),
            fs::IssueKind::SizeLimit => (
                "size_limit",
                Code::InputTooLarge,
                "a file is larger than the search reads",
            ),
            fs::IssueKind::Encoding => (
                "encoding",
                Code::UnsupportedEncoding,
                "a file is not valid UTF-8",
            ),
        };
        let mut state = self.state.lock().unwrap();
        state.issues.skipped += u64::from(skipped);
        state
            .issues
            .add(name, code, message.to_string(), vec![path.to_string()]);
    }

    fn resource_limit(&self, message: &str, paths: Vec<String>) {
        self.issue("resource_limit", Code::InputTooLarge, message, paths);
    }

    /// The snapshot of a candidate file; issues are recorded, exclusions are silent.
    pub fn snapshot(&self, path: &str) -> Option<Arc<Snapshot>> {
        match self.fs.read(path) {
            FileRead::Ok(snapshot) => Some(snapshot),
            FileRead::Excluded(_) => None,
            FileRead::Issue(kind) => {
                self.file_issue(kind, path, true);
                None
            }
        }
    }

    /// Scores navigation items in batches, up to `CONCURRENCY` at once. A batch that fails for
    /// a passing reason is split in halves; a recovered batch is not a failure.
    async fn score(
        self: &Arc<Self>,
        items: Vec<NavigationItem>,
        anchor: Option<RelationAnchor>,
    ) -> Vec<(NavigationItem, f64)> {
        let mut batches: VecDeque<Vec<NavigationItem>> = VecDeque::new();
        let mut batch: Vec<NavigationItem> = Vec::new();
        for item in items {
            if navigation_request(&self.query, std::slice::from_ref(&item), anchor.as_ref()).bytes()
                > MAX_NAVIGATION_BYTES
            {
                self.issue(
                    "request_size",
                    Code::InputTooLarge,
                    "a navigation item is too large for one request",
                    vec![item.path.clone()],
                );
                continue;
            }
            if !batch.is_empty() {
                let mut grown = batch.clone();
                grown.push(item.clone());
                if batch.len() >= MAX_BATCH_ITEMS
                    || navigation_request(&self.query, &grown, anchor.as_ref()).bytes()
                        > MAX_NAVIGATION_BYTES
                {
                    batches.push_back(std::mem::take(&mut batch));
                }
            }
            batch.push(item);
        }
        if !batch.is_empty() {
            batches.push_back(batch);
        }
        let mut results = Vec::new();
        let mut running = JoinSet::new();
        loop {
            while running.len() < super::evaluator::CONCURRENCY && !self.stopped() {
                let Some(group) = batches.pop_front() else {
                    break;
                };
                let context = self.clone();
                let anchor = anchor.clone();
                running.spawn(async move {
                    let request = navigation_request(&context.query, &group, anchor.as_ref());
                    let result = context.evaluator.evaluate(&request, true).await;
                    (group, result)
                });
            }
            let Some(joined) = running.join_next().await else {
                break;
            };
            let (group, result) = joined.expect("a scoring task does not panic");
            match result {
                Ok(scores) => results.extend(group.into_iter().zip(scores)),
                Err(EvalError::Provider { split: true, .. }) if group.len() > 1 => {
                    let mut first = group;
                    let second = first.split_off(first.len().div_ceil(2));
                    batches.push_back(first);
                    batches.push_back(second);
                }
                Err(error) => {
                    self.eval_issue(&error, group.into_iter().map(|item| item.path).collect())
                }
            }
        }
        results
    }

    fn preview_directory(&self, path: &str) -> Option<DirectoryPreview> {
        let listing = self.fs.list(path);
        if !listing.issues.is_empty() {
            for issue in &listing.issues {
                self.file_issue(issue.kind, &issue.path, false);
            }
            return None;
        }
        let mut preview = DirectoryPreview {
            entries: Vec::new(),
            truncated: false,
            sampled_files: 0,
            sampled_directories: 0,
            sampled_extensions: BTreeMap::new(),
            content_samples: None,
        };
        let mut bytes = 0;
        for entry in &listing.entries {
            let name = entry.path().rsplit('/').next().unwrap_or("").to_string();
            let kind = match entry {
                Entry::Directory { .. } => "directory",
                Entry::File { .. } => "file",
            };
            let child = ChildEntry { name, kind };
            let size = serde_json::to_vec(&child).map(|v| v.len()).unwrap_or(0);
            if preview.entries.len() >= DIRECTORY_ENTRIES || bytes + size > DIRECTORY_ENTRY_BYTES {
                preview.truncated = true;
                break;
            }
            bytes += size;
            match entry {
                Entry::Directory { .. } => preview.sampled_directories += 1,
                Entry::File { path, .. } => {
                    preview.sampled_files += 1;
                    let extension = extension(path);
                    let key = if extension.is_empty() {
                        "[no extension]".to_string()
                    } else {
                        extension
                    };
                    *preview.sampled_extensions.entry(key).or_default() += 1;
                }
            }
            preview.entries.push(child);
        }
        preview.entries.sort_by(|a, b| a.name.cmp(&b.name));
        Some(preview)
    }

    fn preview_file(&self, snapshot: &Snapshot) -> FilePreview {
        let source = &snapshot.source;
        let mut end = source.len().min(PREVIEW_BYTES);
        while !source.is_char_boundary(end) {
            end -= 1;
        }
        let mut text = source[..end].to_string();
        let mut truncated = source.len() > PREVIEW_BYTES;
        while json_len(&text) > PREVIEW_JSON_BYTES {
            let mut cut = text.chars().count() * 3 / 4;
            cut = text
                .char_indices()
                .nth(cut)
                .map(|(i, _)| i)
                .unwrap_or(text.len());
            text.truncate(cut);
            truncated = true;
        }
        let mut preview = FilePreview {
            size_bytes: source.len(),
            extension: extension(&snapshot.path),
            preview_bytes: text.len(),
            text,
            truncated,
            range: "opening bytes",
            declarations: Some(Vec::new()),
            declaration_index_truncated: Some(false),
        };
        let small = source.len() <= MAX_INSPECTED_BYTES;
        let path = &snapshot.path;
        if truncated && small && (path.ends_with(".py") || path.ends_with(".pyi")) {
            let sampled = preview::preview(path, source, &self.query, PREVIEW_BYTES);
            if sampled.truncated
                && !sampled.text.is_empty()
                && sampled.text.len() <= PREVIEW_BYTES
                && json_len(&sampled.text) <= PREVIEW_JSON_BYTES
            {
                preview.text = sampled.text;
                preview.preview_bytes = sampled.preview_bytes;
                preview.range = "sampled source ranges";
            }
        }
        if truncated && small && source::mode_for(path) != source::Mode::Text {
            let inspection =
                source::inspect(path, source, source.len().max(4), source::MAX_PARSE_BYTES);
            let mut declarations: Vec<Declaration> = inspection
                .units
                .iter()
                .filter(|unit| !unit.partial)
                .map(|unit| Declaration {
                    name: unit.name.clone(),
                    start_line: unit.range.start_line,
                    end_line: unit.range.end_line,
                })
                .collect();
            preview.declarations = Some(declarations.clone());
            while !declarations.is_empty()
                && serde_json::to_vec(&preview).map(|v| v.len()).unwrap_or(0)
                    > PREVIEW_WITH_INDEX_BYTES
            {
                declarations.pop();
                preview.declarations = Some(declarations.clone());
                preview.declaration_index_truncated = Some(true);
            }
        }
        preview
    }

    /// Breadth-first navigation from `seeds`. Directories and files are judged from their
    /// previews; qualifying directories are explored and qualifying files become candidates.
    async fn discover(self: &Arc<Self>, seeds: Vec<String>, anchor: Option<RelationAnchor>) {
        let mut directories = seeds;
        loop {
            if directories.is_empty()
                || self.stopped()
                || self.state.lock().unwrap().entries_seen >= MAX_ENTRIES
            {
                break;
            }
            // The children of a level's directories are listed without a judgment; the
            // directories below them are judged.
            let mut level: Vec<(String, usize)> =
                directories.drain(..).map(|path| (path, 0)).collect();
            let mut items: Vec<NavigationItem> = Vec::new();
            let mut hashes: HashMap<String, String> = HashMap::new();
            let mut index = 0;
            while index < level.len() && !self.stopped() {
                let (path, depth) = level[index].clone();
                index += 1;
                {
                    let mut state = self.state.lock().unwrap();
                    if state.entries_seen >= MAX_ENTRIES {
                        drop(state);
                        self.resource_limit("the search looked at its entry limit", Vec::new());
                        break;
                    }
                    if !state.visited.insert(path.clone()) {
                        continue;
                    }
                }
                let listing = self.fs.list(&path);
                for issue in &listing.issues {
                    self.file_issue(issue.kind, &issue.path, false);
                }
                for entry in listing.entries {
                    if self.stopped() {
                        break;
                    }
                    let over = {
                        let mut state = self.state.lock().unwrap();
                        state.entries_seen += 1;
                        state.entries_seen > MAX_ENTRIES
                    };
                    if over {
                        self.resource_limit("the search looked at its entry limit", Vec::new());
                        break;
                    }
                    match entry {
                        Entry::Directory { path } if depth == 0 => level.push((path, 1)),
                        Entry::Directory { path } => {
                            let Some(child_preview) = self.preview_directory(&path) else {
                                continue;
                            };
                            let item = NavigationItem {
                                path,
                                kind: Kind::Directory,
                                source_range: None,
                                file_preview: None,
                                child_preview: Some(child_preview),
                            };
                            items.push(match anchor {
                                Some(_) => self.with_directory_content(item),
                                None => item,
                            });
                        }
                        Entry::File { path, .. } => {
                            let Some(snapshot) = self.snapshot(&path) else {
                                continue;
                            };
                            hashes.insert(path.clone(), snapshot.sha256.clone());
                            let file_preview = self.preview_file(&snapshot);
                            self.state
                                .lock()
                                .unwrap()
                                .previews
                                .insert(path.clone(), file_preview.clone());
                            if snapshot.source.len() > MAX_INSPECTED_BYTES {
                                self.resource_limit(
                                    "a file is too large to inspect beyond its preview",
                                    vec![path.clone()],
                                );
                            }
                            items.push(NavigationItem {
                                path,
                                kind: Kind::File,
                                source_range: None,
                                file_preview: Some(file_preview),
                                child_preview: None,
                            });
                        }
                    }
                }
            }
            let (to_expand, to_score): (Vec<NavigationItem>, Vec<NavigationItem>) =
                items.into_iter().partition(|item| {
                    item.kind == Kind::File
                        && item
                            .file_preview
                            .as_ref()
                            .is_some_and(|p| p.size_bytes <= MAX_INSPECTED_BYTES)
                        && navigation_request(
                            &self.query,
                            std::slice::from_ref(item),
                            anchor.as_ref(),
                        )
                        .bytes()
                            > MAX_NAVIGATION_BYTES
                });
            let mut classified = self.score(to_score, anchor.clone()).await;
            // A file whose preview is too large for one request is judged in bounded chunks.
            let mut chunks = Vec::new();
            for item in to_expand {
                let Some(snapshot) = self.snapshot(&item.path) else {
                    continue;
                };
                let size_bytes = snapshot.source.len();
                for unit in source::split_source(&snapshot.source, CHUNK_BYTES) {
                    let text = snapshot.source[unit.byte_start..unit.byte_end].to_string();
                    chunks.push(NavigationItem {
                        path: item.path.clone(),
                        kind: Kind::File,
                        source_range: None,
                        file_preview: Some(FilePreview {
                            size_bytes,
                            extension: extension(&item.path),
                            preview_bytes: text.len(),
                            text,
                            truncated: true,
                            range: "sampled source ranges",
                            declarations: None,
                            declaration_index_truncated: None,
                        }),
                        child_preview: None,
                    });
                }
            }
            classified.extend(self.score(chunks, anchor.clone()).await);
            let mut state = self.state.lock().unwrap();
            for (item, probability) in classified {
                match item.kind {
                    Kind::Directory if probability > THRESHOLD => directories.push(item.path),
                    Kind::Directory => {
                        if anchor.is_none() {
                            state.pruned.push(item);
                        }
                    }
                    Kind::File => {
                        state.considered.insert(item.path.clone());
                        if probability > THRESHOLD {
                            let better = state
                                .candidates
                                .get(&item.path)
                                .is_none_or(|c| probability > c.score);
                            if better {
                                let sha256 = hashes.get(&item.path).cloned().unwrap_or_default();
                                state.candidates.insert(
                                    item.path.clone(),
                                    Candidate {
                                        path: item.path,
                                        sha256,
                                        score: probability,
                                        ranges: Vec::new(),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
        // Directories left because the entry limit was reached; a stop has its own issue.
        if !directories.is_empty() && !self.stopped() {
            self.resource_limit(
                "the search reached its entry limit with directories left to explore",
                directories,
            );
        }
    }

    /// Runs the search policy.
    pub async fn run(self: &Arc<Self>) -> Found {
        self.discover(vec![String::new()], None).await;
        if let Some(anchor) = self.anchor().filter(|_| !self.stopped()) {
            // One relationship reconsideration of the pruned directories, anchored on the
            // classes of the best candidate, before new candidates are admitted.
            let pruned = self.state.lock().unwrap().pruned.clone();
            let items: Vec<NavigationItem> = pruned
                .into_iter()
                .map(|item| self.with_directory_content(item))
                .collect();
            let seeds: Vec<String> = self
                .score(items, Some(anchor.clone()))
                .await
                .into_iter()
                .filter(|(_, score)| *score > THRESHOLD)
                .map(|(item, _)| item.path)
                .collect();
            self.discover(seeds, Some(anchor)).await;
        }
        let first = self.select().await;
        // Donors follow the order in which selection finished, as in jevgrep.
        let evidence: Vec<Evidence> = first
            .iter()
            .flat_map(|(path, selection)| {
                selection.excerpts.iter().map(|excerpt| Evidence {
                    path: path.clone(),
                    start_line: excerpt.range.start_line,
                    end_line: excerpt.range.end_line,
                    source: excerpt.source.clone(),
                })
            })
            .collect();
        let mut selections: HashMap<String, Selection> = first.into_iter().collect();
        let bytes = serde_json::to_vec(&evidence)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if !evidence.is_empty() && bytes <= MAX_EVIDENCE_BYTES && !self.stopped() {
            self.reselect(&evidence, &mut selections).await;
        }
        self.finish(&selections)
    }

    /// The best candidate that declares classes: their names anchor the relationship pass.
    fn anchor(&self) -> Option<RelationAnchor> {
        let mut candidates: Vec<Candidate> = self
            .state
            .lock()
            .unwrap()
            .candidates
            .values()
            .cloned()
            .collect();
        candidates.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.path.cmp(&b.path))
        });
        for candidate in candidates {
            if candidate.score <= THRESHOLD || self.stopped() {
                break;
            }
            let Some(snapshot) = self.snapshot(&candidate.path) else {
                continue;
            };
            let size = snapshot.source.len();
            let units =
                source::inspect(&snapshot.path, &snapshot.source, size.max(4), size.max(1)).units;
            let mut classes: Vec<String> = Vec::new();
            for unit in units.iter().filter(|u| u.name.ends_with(".context")) {
                let class = unit.name.split('.').next().unwrap_or("").to_string();
                if !classes.contains(&class) {
                    classes.push(class);
                }
            }
            let bytes = serde_json::to_vec(&classes)
                .map(|v| v.len())
                .unwrap_or(usize::MAX);
            if !classes.is_empty() && bytes < MAX_ANCHOR_BYTES {
                return Some(RelationAnchor {
                    path: candidate.path,
                    classes,
                });
            }
        }
        None
    }

    /// A directory preview with samples of its files: the opening, middle and end of each,
    /// for the relationship question.
    fn with_directory_content(&self, mut item: NavigationItem) -> NavigationItem {
        let Some(preview) = item.child_preview.as_mut() else {
            return item;
        };
        let names: Vec<String> = preview
            .entries
            .iter()
            .filter(|c| c.kind == "file")
            .map(|c| c.name.clone())
            .collect();
        let per_file = (SAMPLE_BYTES / names.len().max(1)).max(MIN_SAMPLE_CHARS);
        let mut samples = Vec::new();
        for name in names {
            if self.stopped() {
                break;
            }
            let path = if item.path.is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", item.path)
            };
            let Some(snapshot) = self.snapshot(&path) else {
                continue;
            };
            if snapshot.source.len() > MAX_INSPECTED_BYTES {
                continue;
            }
            let chars: Vec<char> = snapshot.source.chars().collect();
            let length = chars.len();
            let part = per_file / 3;
            let slice = |start: usize| {
                chars[start.min(length)..(start + part).min(length)]
                    .iter()
                    .collect::<String>()
            };
            let source = if length <= per_file {
                snapshot.source.clone()
            } else {
                [
                    0,
                    (length / 2).saturating_sub(part / 2),
                    length.saturating_sub(part),
                ]
                .iter()
                .map(|&start| format!("[character offset {start}]\n{}", slice(start)))
                .collect::<Vec<_>>()
                .join("\n...\n")
            };
            samples.push(ContentSample {
                name,
                truncated: length > per_file,
                source,
            });
        }
        preview.content_samples = Some(samples);
        let too_big = |p: &DirectoryPreview| {
            serde_json::to_vec(p).map(|v| v.len()).unwrap_or(usize::MAX) > SAMPLED_PREVIEW_BYTES
        };
        while too_big(preview)
            && preview
                .content_samples
                .iter()
                .flatten()
                .any(|s| s.source.chars().count() > MIN_SAMPLE_CHARS)
        {
            for sample in preview.content_samples.iter_mut().flatten() {
                let count = sample.source.chars().count();
                let keep = (count * 4 / 5).max(MIN_SAMPLE_CHARS);
                sample.source = sample.source.chars().take(keep).collect();
                sample.truncated = true;
            }
        }
        item
    }

    /// Selects the qualifying regions of every candidate file, up to `CONCURRENCY` files at
    /// once, in the order they finish.
    async fn select(self: &Arc<Self>) -> Vec<(String, Selection)> {
        let candidates: Vec<Candidate> = self
            .state
            .lock()
            .unwrap()
            .candidates
            .values()
            .cloned()
            .collect();
        let mut selections = Vec::new();
        let mut queue: VecDeque<Candidate> = candidates.into();
        let mut running = JoinSet::new();
        loop {
            while running.len() < super::evaluator::CONCURRENCY && !self.stopped() {
                let Some(candidate) = queue.pop_front() else {
                    break;
                };
                let Some(snapshot) = self.inspectable(&candidate.path) else {
                    continue;
                };
                let context = self.clone();
                running.spawn(async move {
                    let selection =
                        select_file(&context.evaluator, &context.query, &snapshot, None, None)
                            .await;
                    (candidate.path, selection)
                });
            }
            let Some(joined) = running.join_next().await else {
                break;
            };
            let (path, selection) = joined.expect("a selection task does not panic");
            for failure in &selection.failures {
                self.eval_issue(failure, vec![path.clone()]);
            }
            selections.push((path, selection));
        }
        selections
    }

    /// Judges every candidate again, one file at a time, with the selected evidence of all
    /// files: a unit that the evidence references can join, and a valid rejection retracts.
    /// A file that is not judged again keeps its first selection.
    async fn reselect(&self, evidence: &[Evidence], selections: &mut HashMap<String, Selection>) {
        let candidates: Vec<Candidate> = self
            .state
            .lock()
            .unwrap()
            .candidates
            .values()
            .cloned()
            .collect();
        for candidate in candidates {
            if self.stopped() {
                break;
            }
            let Some(snapshot) = self.inspectable(&candidate.path) else {
                continue;
            };
            let previous = selections.remove(&candidate.path);
            let selection = select_file(
                &self.evaluator,
                &self.query,
                &snapshot,
                Some(evidence),
                previous,
            )
            .await;
            for failure in &selection.failures {
                self.eval_issue(failure, vec![candidate.path.clone()]);
            }
            selections.insert(candidate.path, selection);
        }
    }

    /// The snapshot of a candidate whose regions can be selected; a larger file is an issue
    /// and stays a file-only match.
    fn inspectable(&self, path: &str) -> Option<Arc<Snapshot>> {
        let snapshot = self.snapshot(path)?;
        if snapshot.source.len() > MAX_INSPECTED_BYTES {
            self.issue(
                "inspection_limit",
                Code::InputTooLarge,
                "a file is too large to select regions from",
                vec![path.to_string()],
            );
            return None;
        }
        Some(snapshot)
    }

    fn finish(&self, selections: &HashMap<String, Selection>) -> Found {
        if self.cancel.is_cancelled()
            && !self
                .state
                .lock()
                .unwrap()
                .issues
                .counts
                .iter()
                .any(|c| c.kind == "cancelled")
        {
            self.issue(
                "cancelled",
                Code::Cancelled,
                "the search was cancelled",
                Vec::new(),
            );
        }
        let state = self.state.lock().unwrap();
        let mut files: Vec<Candidate> = state.candidates.values().cloned().collect();
        for file in &mut files {
            if let Some(selection) = selections.get(&file.path) {
                file.ranges = selection
                    .ranges()
                    .into_iter()
                    .map(|(range, relevance)| FileRange {
                        start_line: range.start_line,
                        end_line: range.end_line,
                        relevance,
                    })
                    .collect();
            }
        }
        files.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.path.cmp(&b.path))
        });
        Found {
            files,
            files_considered: state.considered.len(),
            directories_pruned: state.pruned.len(),
            files_skipped: state.issues.skipped,
            issues: state.issues.counts.clone(),
            locations: state.issues.locations.clone(),
            error: state.issues.first.clone(),
            interrupted: self.cancel.is_cancelled(),
        }
    }
}

/// Node's `path.extname`: the last `.` of the name and what follows, unless the name starts
/// with it.
pub fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(at) if at > 0 => name[at..].to_string(),
        _ => String::new(),
    }
}

fn json_len(text: &str) -> usize {
    serde_json::to_string(text)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev;

    #[tokio::test]
    async fn an_exhausted_request_budget_ends_the_search_incomplete() {
        let temp = tempfile::tempdir().unwrap();
        let scope = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::write(scope.join("a.rs"), "fn a() {}").unwrap();
        let policy = fs::Policy {
            scope,
            include: None,
            exclude: None,
            protected: Vec::new(),
        };
        let cancel = CancellationToken::new();
        let evaluator =
            Evaluator::new(jev::Client::new("http://127.0.0.1:9/", "k"), cancel.clone());
        evaluator.use_requests(super::super::evaluator::REQUEST_LIMIT);
        let found = Context::new("q".into(), Filesystem::new(policy), evaluator, cancel)
            .run()
            .await;
        assert!(!found.interrupted);
        assert_eq!(found.error.unwrap().code, Code::BudgetExhausted);
        assert_eq!(found.issues[0].kind, "budget");
    }

    #[tokio::test]
    async fn a_search_cancelled_before_discovery_is_only_interrupted() {
        let temp = tempfile::tempdir().unwrap();
        let scope = std::fs::canonicalize(temp.path()).unwrap();
        for dir in ["a/b", "c/d"] {
            std::fs::create_dir_all(scope.join(dir)).unwrap();
            std::fs::write(scope.join(dir).join("x.rs"), "fn x() {}").unwrap();
        }
        let policy = fs::Policy {
            scope,
            include: None,
            exclude: None,
            protected: Vec::new(),
        };
        let cancel = CancellationToken::new();
        cancel.cancel();
        let client = jev::Client::new("http://127.0.0.1:9/", "k");
        let evaluator = Evaluator::new(client, cancel.clone());
        let found = Context::new("q".into(), Filesystem::new(policy), evaluator, cancel)
            .run()
            .await;
        assert!(found.interrupted);
        assert_eq!(found.error.unwrap().code, Code::Cancelled);
        let kinds: Vec<&str> = found.issues.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, vec!["cancelled"]);
    }

    #[test]
    fn extensions_follow_node() {
        assert_eq!(extension("a/b.rs"), ".rs");
        assert_eq!(extension("a/.bashrc"), "");
        assert_eq!(extension("a.tar.gz"), ".gz");
        assert_eq!(extension("Makefile"), "");
    }
}
