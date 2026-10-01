// Ported from jevgrep (https://github.com/dzhng/jevgrep) at commit
// 82ef1fd3f43161bb17395dba1e07a5338f3913db, packages/core/src/selection.ts. Reading leads
// and presentation excerpts are not ported: they only shape jevgrep's text output.
// Copyright (c) David Zhang. MIT License; see LICENSE-jevgrep.

//! Evidence selection: which source units of a qualifying file are worth reading, each judged
//! on its own.

use std::collections::BTreeMap;

use super::evaluator::{EvalError, Evaluator};
use super::fs::Snapshot;
use super::requests::{Declaration, Evidence, evidence_request};
use super::source::{self, Range, Text, Unit};

/// The largest unit; a larger one is cut into 16-line blocks.
pub const SOURCE_UNIT_BYTES: usize = 24_000;
/// The fragments of a file that has no declarations.
pub const FALLBACK_UNIT_BYTES: usize = 3_000;
/// The source of one group of units, and its most units.
pub const GROUP_BYTES: usize = 42_000;
pub const GROUP_UNITS: usize = 128;
/// The largest request state; a larger group is split in halves.
pub const STATE_BYTES: usize = 80_000;
/// A file at most this size is sent whole as context.
pub const WHOLE_SOURCE_BYTES: usize = 16_000;
/// Lines of context around a group, and lines of opening context.
pub const CONTEXT_LINES: usize = 8;
pub const OPENING_LINES: usize = 20;
/// The lines of a block that a large unit is cut into.
pub const BLOCK_LINES: usize = 16;
const THRESHOLD: f64 = 0.5;

/// Bytes `start..end` of the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// The judgment of one unit.
#[derive(Clone, Debug)]
pub struct Decision {
    pub span: Span,
    pub range: Range,
    /// The span starts and ends on line boundaries, so `range` describes it exactly.
    pub whole_lines: bool,
    pub score: f64,
}

/// What selection found in one file. A later pass with selected evidence starts from it.
#[derive(Debug, Default)]
pub struct Selection {
    /// The latest judgment of each unit, by span.
    pub decisions: BTreeMap<Span, Decision>,
    /// The selected spans, before merging.
    pub selected: Vec<Span>,
    /// The spans the excerpts cover, which a later pass keeps as context.
    pub rendered: Vec<Span>,
    /// The selected source with its surroundings: the evidence that a later pass shows Jev.
    /// It stays in memory; it is never returned or stored.
    pub excerpts: Vec<Excerpt>,
    /// Failures, by kind; a provider failure does not stop the file.
    pub failures: Vec<EvalError>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Excerpt {
    pub range: Range,
    pub source: String,
}

impl Selection {
    /// The units that qualify on their own and that line numbers describe exactly, most
    /// relevant first, then by start line, then by end line.
    pub fn ranges(&self) -> Vec<(Range, f64)> {
        let mut ranges: Vec<(Range, f64)> = self
            .decisions
            .values()
            .filter(|d| d.score > THRESHOLD && d.whole_lines)
            .map(|d| (d.range, d.score))
            .collect();
        ranges.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then(a.0.start_line.cmp(&b.0.start_line))
                .then(a.0.end_line.cmp(&b.0.end_line))
        });
        ranges
    }
}

/// Line coordinates of a snapshot, as selection.ts computes them.
pub struct Lines<'a> {
    pub text: Text<'a>,
}

impl<'a> Lines<'a> {
    pub fn new(source: &'a str) -> Lines<'a> {
        Lines {
            text: Text::new(source),
        }
    }

    pub fn range_for(&self, span: Span) -> Range {
        Range::new(
            self.text.line_at(span.start),
            self.text
                .line_at(span.start.max(span.end.saturating_sub(1))),
        )
    }

    /// The span starts at a line start and ends at the end of a line.
    pub fn whole_lines(&self, span: Span) -> bool {
        let range = self.range_for(span);
        span.start == self.text.line_start(range.start_line)
            && span.end == self.text.line_end(range.end_line)
    }
}

/// The units selection judges: declarations, or 3,000-byte fragments when there are none, with
/// units over 24,000 bytes cut into 16-line blocks. A file with a giant line keeps the
/// parser's byte-bounded units.
/// Also gives the comments, which bound the excerpt windows.
pub fn units(snapshot: &Snapshot) -> (Vec<Unit>, Vec<Range>) {
    let lines = Lines::new(&snapshot.source);
    let text = &lines.text;
    let giant_line =
        (1..=text.line_count).any(|line| text.lines(line, line).len() > SOURCE_UNIT_BYTES);
    let max_unit = if giant_line {
        SOURCE_UNIT_BYTES
    } else {
        SOURCE_UNIT_BYTES.max(text.len())
    };
    let syntax = source::inspect(
        &snapshot.path,
        &snapshot.source,
        max_unit,
        source::MAX_PARSE_BYTES,
    );
    let comments = syntax.comments;
    let mut units = syntax.units;
    if !giant_line && (syntax.mode == source::Mode::Text || units.iter().all(|u| u.partial)) {
        units = source::split_source(&snapshot.source, FALLBACK_UNIT_BYTES)
            .into_iter()
            .map(|unit| {
                let end_line = text.line_at(unit.byte_start.max(unit.byte_end.saturating_sub(1)));
                Unit {
                    range: Range::new(unit.range.start_line, end_line),
                    ..unit
                }
            })
            .collect();
    }
    if giant_line {
        return (units, comments);
    }
    let units = units
        .into_iter()
        .flat_map(|unit| {
            if text.lines(unit.range.start_line, unit.range.end_line).len() <= SOURCE_UNIT_BYTES {
                return vec![unit];
            }
            (unit.range.start_line..=unit.range.end_line)
                .step_by(BLOCK_LINES)
                .map(|start| {
                    let end = unit.range.end_line.min(start + BLOCK_LINES - 1);
                    Unit {
                        name: unit.name.clone(),
                        range: Range::new(start, end),
                        byte_start: text.line_start(start),
                        byte_end: text.line_end(end),
                        partial: true,
                        owner_headers: unit.owner_headers.clone(),
                    }
                })
                .collect()
        })
        .collect();
    (units, comments)
}

fn groups(units: Vec<Unit>, text: &Text) -> Vec<Vec<Unit>> {
    let mut groups = Vec::new();
    let mut pending: Vec<Unit> = Vec::new();
    for unit in units {
        if let Some(first) = pending.first()
            && (pending.len() >= GROUP_UNITS
                || text
                    .lines(first.range.start_line, unit.range.end_line)
                    .len()
                    > GROUP_BYTES)
        {
            groups.push(std::mem::take(&mut pending));
        }
        pending.push(unit);
    }
    if !pending.is_empty() {
        groups.push(pending);
    }
    groups
}

/// The source sent with a group: the whole file when it is small, else the opening lines and
/// the lines around the group. Units that cut a line are sent as their own byte spans.
fn context(snapshot: &Snapshot, lines: &Lines, group: &[Unit]) -> String {
    let text = &lines.text;
    let first = group[0]
        .range
        .start_line
        .saturating_sub(CONTEXT_LINES)
        .max(1);
    let last = text
        .line_count
        .min(group.last().expect("a group has units").range.end_line + CONTEXT_LINES);
    let opening = OPENING_LINES.min(text.line_count);
    let oversized = (1..=opening).chain(first..=last).any(|line| {
        let content = text.lines(line, line).len();
        content > SOURCE_UNIT_BYTES
    });
    let cuts_a_line = group.iter().any(|u| {
        !lines.whole_lines(Span {
            start: u.byte_start,
            end: u.byte_end,
        })
    });
    if cuts_a_line || oversized {
        group
            .iter()
            .map(|u| {
                format!(
                    "Source lines {}-{}; source bytes {}-{}:\n{}",
                    u.range.start_line,
                    u.range.end_line,
                    u.byte_start,
                    u.byte_end,
                    &snapshot.source[u.byte_start..u.byte_end]
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else if snapshot.source.len() <= WHOLE_SOURCE_BYTES {
        snapshot.source.clone()
    } else {
        format!(
            "Opening context:\n{}\nSource lines {first}-{last}:\n{}",
            text.lines(1, opening),
            text.lines(first, last)
        )
    }
}

/// Judges the units of one file. With `evidence`, each unit is also asked whether the selected
/// evidence references it, and a valid rejection retracts an earlier selection. A failed group
/// keeps what `previous` had.
pub async fn select_file(
    evaluator: &Evaluator,
    query: &str,
    snapshot: &Snapshot,
    evidence: Option<&[Evidence]>,
    previous: Option<Selection>,
) -> Selection {
    let lines = Lines::new(&snapshot.source);
    let mut selection = previous.unwrap_or_default();
    selection.failures.clear();
    let (units, comments) = units(snapshot);
    // Excerpts cover what this pass selects and what the previous pass showed.
    let mut context_spans = std::mem::take(&mut selection.rendered);
    let mut selected_lines: Vec<Range> = Vec::new();
    let mut queue: std::collections::VecDeque<Vec<Unit>> = groups(units, &lines.text).into();
    while let Some(group) = queue.pop_front() {
        let declarations: Vec<Declaration> = group
            .iter()
            .map(|u| Declaration {
                name: u.name.clone(),
                start_line: u.range.start_line,
                end_line: u.range.end_line,
            })
            .collect();
        let request = evidence_request(
            query,
            &snapshot.path,
            &context(snapshot, &lines, &group),
            &declarations,
            evidence,
        );
        // Shared evidence counts toward the state limit as well as local source.
        let state_bytes = serde_json::to_vec(&request.state)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if group.len() > 1 && state_bytes > STATE_BYTES {
            let mut first = group;
            let second = first.split_off(first.len().div_ceil(2));
            queue.push_front(second);
            queue.push_front(first);
            continue;
        }
        let answers = match evaluator.evaluate(&request, false).await {
            Ok(answers) => answers,
            Err(error) => {
                let stop = !matches!(error, EvalError::Provider { .. });
                selection.failures.push(error);
                if stop {
                    break;
                }
                continue;
            }
        };
        // Answers come in question order: relevance, then scope, then reference.
        let n = group.len();
        for (i, unit) in group.iter().enumerate() {
            let relevance = answers[i];
            let scope = answers[n + i];
            let reference = if evidence.is_some() {
                answers[2 * n + i]
            } else {
                0.0
            };
            let score = relevance.min(scope).max(reference);
            let span = Span {
                start: unit.byte_start,
                end: unit.byte_end,
            };
            selection.decisions.insert(
                span,
                Decision {
                    span,
                    range: lines.range_for(span),
                    whole_lines: lines.whole_lines(span),
                    score,
                },
            );
            // Only a valid contextual rejection retracts an earlier selection.
            if evidence.is_some() && score <= THRESHOLD {
                selection.selected = selection
                    .selected
                    .iter()
                    .flat_map(|s| {
                        if s.end <= span.start || s.start >= span.end {
                            return vec![*s];
                        }
                        let mut kept = Vec::new();
                        if s.start < span.start {
                            kept.push(Span {
                                start: s.start,
                                end: span.start,
                            });
                        }
                        if s.end > span.end {
                            kept.push(Span {
                                start: span.end,
                                end: s.end,
                            });
                        }
                        kept
                    })
                    .collect();
            }
            if score > THRESHOLD {
                selection.selected.push(span);
                context_spans.push(span);
                if lines.whole_lines(span) {
                    selected_lines.push(unit.range);
                }
            }
        }
    }
    let chosen = merge(&selection.selected);
    let mut whole_ranges = selected_lines;
    let mut rendered = Vec::new();
    for span in merge(&context_spans) {
        if lines.whole_lines(span) {
            whole_ranges.push(lines.range_for(span));
        } else {
            rendered.push(span);
        }
    }
    let mut ranges = whole_ranges.clone();
    if (snapshot.path.ends_with(".py") || snapshot.path.ends_with(".pyi"))
        && snapshot.source.len() <= source::MAX_PARSE_BYTES
        && !whole_ranges.is_empty()
    {
        ranges.extend(source::python_neighborhood(&snapshot.source, &whole_ranges));
    }
    let (spans, excerpts) = excerpts(&lines, &ranges, rendered, &comments, &chosen);
    selection.rendered = spans;
    selection.excerpts = excerpts;
    selection
}

/// Sorted, overlapping spans joined; empty spans dropped.
pub fn merge(spans: &[Span]) -> Vec<Span> {
    let mut sorted: Vec<Span> = spans.iter().copied().filter(|s| s.end > s.start).collect();
    sorted.sort();
    let mut merged: Vec<Span> = Vec::new();
    for span in sorted {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// Windows of three lines around each range, grown over comments that touch them or are
/// separated from them only by blank lines. A giant line in a window is shown only where a
/// selected span covers it. Returns the covered spans and their text.
fn excerpts(
    lines: &Lines,
    ranges: &[Range],
    mut rendered: Vec<Span>,
    comments: &[Range],
    chosen: &[Span],
) -> (Vec<Span>, Vec<Excerpt>) {
    let text = &lines.text;
    let blank =
        |from: usize, to: usize| (from..=to).all(|line| text.lines(line, line).trim().is_empty());
    let windows: Vec<Range> = ranges
        .iter()
        .map(|r| {
            Range::new(
                r.start_line.saturating_sub(3).max(1),
                text.line_count.min(r.end_line + 3),
            )
        })
        .collect();
    let mut grown = Vec::new();
    for mut window in windows.iter().copied() {
        let mut changed = true;
        while changed {
            changed = false;
            for comment in comments {
                let before = comment.end_line < window.start_line
                    && blank(comment.end_line + 1, window.start_line - 1);
                let after = comment.start_line > window.end_line
                    && blank(window.end_line + 1, comment.start_line - 1);
                let overlaps =
                    comment.start_line <= window.end_line && comment.end_line >= window.start_line;
                if overlaps || before || after {
                    let start = window.start_line.min(comment.start_line);
                    let end = window.end_line.max(comment.end_line);
                    if start != window.start_line || end != window.end_line {
                        window = Range::new(start, end);
                        changed = true;
                    }
                }
            }
        }
        let mut segment_start = text.line_start(window.start_line);
        for line in window.start_line..=window.end_line {
            let (start, end) = (text.line_start(line), text.line_end(line));
            // An adjacent selected declaration must not pull in an unselected giant line.
            if end - start > SOURCE_UNIT_BYTES {
                rendered.push(Span {
                    start: segment_start,
                    end: start,
                });
                for span in chosen {
                    if span.start < end && span.end > start {
                        rendered.push(Span {
                            start: span.start.max(start),
                            end: span.end.min(end),
                        });
                    }
                }
                segment_start = end;
            }
        }
        rendered.push(Span {
            start: segment_start,
            end: text.line_end(window.end_line),
        });
        grown.push(window);
    }
    let spans = merge(&rendered);
    let excerpts = spans
        .iter()
        .map(|&span| {
            let mut range = lines.range_for(span);
            if lines.whole_lines(span) {
                // A trailing empty line has no bytes but belongs to a window that ends there.
                if span.end == text.len() && grown.iter().any(|w| w.end_line == text.line_count) {
                    range.end_line = text.line_count;
                }
                Excerpt {
                    range,
                    source: text.lines(range.start_line, range.end_line).to_string(),
                }
            } else {
                Excerpt {
                    range,
                    source: text.source[span.start..span.end].to_string(),
                }
            }
        })
        .collect();
    (spans, excerpts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(path: &str, source: &str) -> Snapshot {
        Snapshot {
            path: path.into(),
            source: source.into(),
            sha256: String::new(),
        }
    }

    #[test]
    fn declarations_are_units_and_text_falls_back_to_fragments() {
        let go = snapshot("a.go", "package a\n\nfunc A() {}\n\nfunc B() {}\n");
        let names: Vec<String> = units(&go).0.into_iter().map(|u| u.name).collect();
        assert_eq!(names, vec!["package a", "A", "B"]);
        let text: String = (0..400).map(|i| format!("line {i} of notes\n")).collect();
        let notes = snapshot("notes.md", &text);
        let fragments = units(&notes).0;
        assert!(fragments.len() > 1);
        assert!(
            fragments
                .iter()
                .all(|u| u.byte_end - u.byte_start <= FALLBACK_UNIT_BYTES)
        );
        let lines = Lines::new(&notes.source);
        assert!(fragments.iter().all(|u| lines.whole_lines(Span {
            start: u.byte_start,
            end: u.byte_end
        })));
        assert_eq!(
            fragments.last().unwrap().range.end_line,
            400,
            "no empty trailing line"
        );
    }

    #[test]
    fn a_large_declaration_becomes_sixteen_line_blocks() {
        let body: String = (0..2000)
            .map(|i| format!("    x{i} = {i}  # padding padding\n"))
            .collect();
        let py = snapshot("a.py", &format!("def big():\n{body}"));
        let blocks = units(&py).0;
        assert!(blocks.len() > 100);
        assert!(
            blocks
                .iter()
                .all(|u| u.name == "big" && u.range.end_line - u.range.start_line < BLOCK_LINES)
        );
        assert_eq!(blocks[0].range, Range::new(1, 16));
    }

    #[test]
    fn ranges_skip_spans_that_cut_lines_and_sort_by_relevance() {
        let source = "a\nbb\nccc\n";
        let lines = Lines::new(source);
        let mut selection = Selection::default();
        for (start, end, score) in [(0, 2, 0.7), (2, 5, 0.9), (5, 7, 0.95), (5, 9, 0.4)] {
            let span = Span { start, end };
            selection.decisions.insert(
                span,
                Decision {
                    span,
                    range: lines.range_for(span),
                    whole_lines: lines.whole_lines(span),
                    score,
                },
            );
        }
        let ranges = selection.ranges();
        assert_eq!(
            ranges,
            vec![(Range::new(2, 2), 0.9), (Range::new(1, 1), 0.7)]
        );
    }

    #[test]
    fn excerpts_take_three_lines_around_and_adjacent_comments() {
        let source =
            "l1\nl2\n// about f\n\nfn f() {}\nl6\nl7\nl8\nl9\nl10\nl11\n// trailing\nl13\n";
        let lines = Lines::new(source);
        let comments = vec![Range::new(3, 3), Range::new(12, 12)];
        let (_, out) = excerpts(&lines, &[Range::new(5, 5)], Vec::new(), &comments, &[]);
        // Lines 2-8; the comment on line 3 is inside; line 12 is beyond a non-blank line.
        assert_eq!(
            out,
            vec![Excerpt {
                range: Range::new(2, 8),
                source: "l2\n// about f\n\nfn f() {}\nl6\nl7\nl8".into()
            }]
        );
        let (_, out) = excerpts(&lines, &[Range::new(9, 9)], Vec::new(), &comments, &[]);
        assert_eq!(
            out[0].range,
            Range::new(6, 12),
            "a comment right after the window joins it"
        );
    }

    #[test]
    fn excerpts_keep_a_giant_line_out_unless_selected() {
        let giant = "x".repeat(SOURCE_UNIT_BYTES + 10);
        let source = format!("a\nb\n{giant}\nc\n");
        let lines = Lines::new(&source);
        let (spans, out) = excerpts(&lines, &[Range::new(1, 1)], Vec::new(), &[], &[]);
        assert_eq!(out.len(), 2, "{spans:?}");
        assert_eq!(out[0].source, "a\nb");
        assert_eq!(
            out[1].source, "c",
            "whole lines, joined as jevgrep joins them"
        );
    }

    #[test]
    fn small_files_go_whole_and_large_ones_get_windows() {
        let small = snapshot("a.rs", "fn a() {}\nfn b() {}\n");
        let units_small = units(&small).0;
        let lines = Lines::new(&small.source);
        assert_eq!(context(&small, &lines, &units_small), small.source);
        let big_source: String = (0..3000).map(|i| format!("fn f{i}() {{}}\n")).collect();
        let big = snapshot("b.rs", &big_source);
        let lines = Lines::new(&big.source);
        let group: Vec<Unit> = units(&big).0.into_iter().skip(1000).take(3).collect();
        let window = context(&big, &lines, &group);
        assert!(window.starts_with("Opening context:\nfn f0() {}\n"));
        assert!(window.contains("\nSource lines 993-1011:\nfn f992() {}"));
    }
}
