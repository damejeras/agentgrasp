// Ported from jevgrep (https://github.com/dzhng/jevgrep) at commit
// 82ef1fd3f43161bb17395dba1e07a5338f3913db: packages/core/src/parser-preview.mjs and the
// previewMatches helper of parser-helpers.mjs.
// Copyright (c) David Zhang. MIT License; see LICENSE-jevgrep.

//! The sampled preview of a large Python file: its opening, the declarations that the query
//! names, and windows spread through the rest. All offsets refer to the original source.

use std::collections::HashSet;

use tree_sitter::Node;

use super::source::{
    Range, Text, body, named_children, parse_python, python_end_line as end_line,
    python_start_line as start_line, unparenthesized,
};

pub struct Preview {
    pub text: String,
    pub preview_bytes: usize,
    pub truncated: bool,
}

/// A declaration whose name is a token of the query.
#[derive(Debug, PartialEq, Eq)]
struct Match {
    start: usize,
    end: usize,
    context: Option<Range>,
    header_end: usize,
    header_line: usize,
    header_column_end: usize,
    body_start: usize,
    body_column: usize,
}

/// The identifier-like tokens of the query.
fn tokens(query: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let mut current = String::new();
    for ch in query.chars() {
        let starts = ch == '_' || ch.is_alphabetic();
        if starts || (!current.is_empty() && ch.is_alphanumeric()) {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.insert(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.insert(current);
    }
    tokens
}

fn matches(root: Node, source: &str, tokens: &HashSet<String>) -> Vec<Match> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        stack.extend(named_children(n).into_iter().rev());
        if !matches!(n.kind(), "class_definition" | "function_definition") {
            continue;
        }
        let name = n
            .child_by_field_name("name")
            .map(|c| &source[c.byte_range()])
            .unwrap_or("");
        if !tokens.contains(name) {
            continue;
        }
        let wrapped = n
            .parent()
            .filter(|p| p.kind() == "decorated_definition")
            .unwrap_or(n);
        let statements = body(n);
        let Some(&first) = statements.first() else {
            continue;
        };
        let mut implementation = first;
        let expression = unparenthesized(
            (first.kind() == "expression_statement")
                .then(|| named_children(first).into_iter().next())
                .flatten(),
        );
        let strings: Vec<Option<Node>> = match expression {
            Some(e) if e.kind() == "concatenated_string" => named_children(e)
                .into_iter()
                .filter(|c| c.kind() == "string")
                .map(Some)
                .collect(),
            other => vec![other],
        };
        // A plain docstring is skipped, so the preview shows the implementation.
        let docstring = !strings.is_empty()
            && strings.iter().all(|s| {
                s.is_some_and(|s| {
                    s.kind() == "string" && !bytes_or_format_prefix(&source[s.byte_range()])
                })
            });
        if docstring && statements.len() > 1 {
            implementation = statements[1];
        }
        let mut owner = n.parent();
        while let Some(o) = owner.filter(|o| o.kind() != "class_definition") {
            owner = o.parent();
        }
        let context = owner.and_then(|owner| {
            let wrapper = owner
                .parent()
                .filter(|p| p.kind() == "decorated_definition")
                .unwrap_or(owner);
            body(owner)
                .first()
                .map(|first| Range::new(start_line(wrapper), first.start_position().row))
        });
        let same_row = first.start_position().row == n.start_position().row;
        found.push(Match {
            start: start_line(wrapped),
            end: end_line(n),
            context,
            header_end: first.start_position().row,
            header_line: n.start_position().row + 1,
            header_column_end: if same_row {
                first.start_position().column
            } else {
                0
            },
            body_start: implementation.start_position().row + 1,
            body_column: if implementation != first
                && implementation.start_position().row == first.start_position().row
            {
                implementation.start_position().column
            } else {
                0
            },
        });
    }
    found.sort_by_key(|m| (m.start, m.end));
    found
}

/// jevgrep's `/^[rub]*[fb]/i`: the string is a bytes or a format string.
fn bytes_or_format_prefix(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len()).any(|k| {
        bytes[..k]
            .iter()
            .all(|b| matches!(b.to_ascii_lowercase(), b'r' | b'u' | b'b'))
            && matches!(bytes[k].to_ascii_lowercase(), b'f' | b'b')
    })
}

/// Cuts `raw` to at most `limit` bytes at character boundaries, from its start or its end.
fn clip(raw: &[u8], limit: usize, from_end: bool) -> String {
    let mut start = if from_end {
        raw.len().saturating_sub(limit)
    } else {
        0
    };
    let mut end = if from_end {
        raw.len()
    } else {
        raw.len().min(limit)
    };
    while start < end && (raw[start] & 0xc0) == 0x80 {
        start += 1;
    }
    while end < raw.len() && end > start && (raw[end] & 0xc0) == 0x80 {
        end -= 1;
    }
    String::from_utf8_lossy(&raw[start..end]).into_owned()
}

struct Builder<'a> {
    lines: Vec<&'a str>,
    budget: usize,
    used: usize,
    parts: Vec<String>,
    seen: HashSet<(usize, usize, usize, String)>,
}

impl Builder<'_> {
    /// Adds lines `start..=end` within `allowance` bytes. False when nothing fits.
    fn add(
        &mut self,
        start: usize,
        end: usize,
        allowance: usize,
        basis: &str,
        from_end: bool,
    ) -> bool {
        if start > end {
            return false;
        }
        let mut start = start.max(1);
        let end = end.min(self.lines.len());
        let allowance = allowance.min(self.budget.saturating_sub(self.used));
        let header = format!("--- source lines {start}-{end}; {basis}; may be clipped ---\n");
        let Some(remaining) = allowance.checked_sub(header.len() + 1).filter(|r| *r > 0) else {
            return false;
        };
        let mut selected: Vec<String> = Vec::new();
        let mut size = 0;
        let mut partial = false;
        let mut candidates: Vec<&str> = self.lines[start - 1..end.max(start - 1)].to_vec();
        if from_end {
            candidates.reverse();
        }
        for line in candidates {
            let cost = line.len() + usize::from(!selected.is_empty());
            if size + cost > remaining {
                if selected.is_empty() {
                    let clipped = clip(line.as_bytes(), remaining, from_end);
                    if !clipped.is_empty() {
                        selected.push(clipped);
                        partial = true;
                    }
                }
                break;
            }
            selected.push(line.to_string());
            size += cost;
        }
        if selected.is_empty() {
            return false;
        }
        if from_end {
            selected.reverse();
            start = end - selected.len() + 1;
        }
        let actual_end = start + selected.len() - 1;
        let body = selected.join("\n");
        let identity = (start, actual_end, usize::MAX, body.clone());
        if self.seen.contains(&identity) {
            return true;
        }
        let label = format!(
            "--- source lines {start}-{actual_end}; {basis}{} ---\n",
            if partial { "; partial line" } else { "" }
        );
        let rendered = format!("{label}{body}\n");
        if rendered.len() > allowance {
            return false;
        }
        self.seen.insert(identity);
        self.used += rendered.len();
        self.parts.push(rendered);
        true
    }

    /// Adds bytes `start_column..end_column` of one line.
    fn inline(
        &mut self,
        line: usize,
        start_column: usize,
        end_column: usize,
        allowance: usize,
        basis: &str,
    ) -> bool {
        let raw = self.lines[line - 1].as_bytes();
        let start = start_column.min(raw.len());
        let end = end_column.min(raw.len());
        let allowance = allowance.min(self.budget.saturating_sub(self.used));
        let longest =
            format!("--- source line {line}, bytes {start}-{end}; {basis}; partial line ---\n");
        let Some(room) = allowance.checked_sub(longest.len() + 1).filter(|r| *r > 0) else {
            return false;
        };
        let body = clip(&raw[start..end.max(start)], room, false);
        if body.is_empty() {
            return false;
        }
        let actual_end = start + body.len();
        let identity = (line, start, actual_end, body.clone());
        if self.seen.contains(&identity) {
            return true;
        }
        self.seen.insert(identity);
        let rendered = format!(
            "--- source line {line}, bytes {start}-{actual_end}; {basis}; partial line ---\n{body}\n"
        );
        self.used += rendered.len();
        self.parts.push(rendered);
        true
    }
}

/// The preview of `source` within `budget` bytes. A Python source over the budget is sampled
/// around the declarations that the query names; other text gets its opening and spread
/// windows.
pub fn preview(path: &str, source: &str, query: &str, budget: usize) -> Preview {
    let text = Text::new(source);
    if source.len() <= budget {
        return Preview {
            text: source.to_string(),
            preview_bytes: source.len(),
            truncated: false,
        };
    }
    let found = if path.ends_with(".py") || path.ends_with(".pyi") {
        parse_python(source)
            .map(|tree| matches(tree.root_node(), source, &tokens(query)))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let lines: Vec<&str> = source.split('\n').collect();
    let line_count = text.line_count;
    let mut b = Builder {
        lines,
        budget,
        used: 0,
        parts: Vec::new(),
        seen: HashSet::new(),
    };
    b.add(1, line_count, budget / 4, "opening context", false);
    if !found.is_empty() {
        let per_match = ((budget - b.used) * 3 / 4 / found.len()).max(1);
        for m in &found {
            let mut context_cost = 0;
            if let Some(context) = m.context {
                let before = b.used;
                b.add(
                    context.start_line,
                    context.end_line,
                    (per_match / 3).min(512),
                    "enclosing class context",
                    false,
                );
                context_cost = b.used - before;
            }
            let before = b.used;
            let share = (per_match.saturating_sub(context_cost) / 3).min(512);
            b.add(
                m.start,
                m.header_end,
                share,
                "query-named declaration header",
                false,
            );
            if m.header_column_end > 0 {
                b.inline(
                    m.header_line,
                    0,
                    m.header_column_end,
                    share,
                    "query-named declaration header",
                );
            }
            let remaining = per_match
                .saturating_sub(context_cost)
                .saturating_sub(b.used - before);
            if m.body_column > 0 {
                let before = b.used;
                let line_length = b.lines[m.body_start - 1].len();
                b.inline(
                    m.body_start,
                    m.body_column,
                    line_length,
                    remaining,
                    "query-named implementation",
                );
                if m.end > m.body_start {
                    let left = remaining.saturating_sub(b.used - before);
                    b.add(
                        m.body_start + 1,
                        m.end,
                        left,
                        "query-named implementation continuation",
                        false,
                    );
                }
            } else {
                b.add(
                    m.body_start,
                    m.end,
                    remaining,
                    "query-named implementation",
                    false,
                );
            }
        }
    }
    let mut positions = vec![
        line_count / 3 + 1,
        2 * line_count / 3 + 1,
        line_count.saturating_sub(31).max(1),
    ];
    positions.sort_unstable();
    positions.dedup();
    let count = positions.len();
    for (i, start) in positions.into_iter().enumerate() {
        let allowance = (b.budget - b.used) / (count - i);
        b.add(
            start,
            (start + 31).min(line_count),
            allowance,
            "distributed context",
            i == count - 1,
        );
    }
    let text = b.parts.concat();
    Preview {
        preview_bytes: b.used,
        text,
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_tokens() {
        let t = tokens("Where is refund_limit() checked in Payments?");
        for token in ["Where", "is", "refund_limit", "checked", "in", "Payments"] {
            assert!(t.contains(token), "{token}");
        }
    }

    #[test]
    fn string_prefixes() {
        assert!(bytes_or_format_prefix("f'x'"));
        assert!(bytes_or_format_prefix("rb'x'"));
        assert!(bytes_or_format_prefix("B\"x\""));
        assert!(!bytes_or_format_prefix("\"\"\"doc\"\"\""));
        assert!(!bytes_or_format_prefix("r'doc'"));
        assert!(!bytes_or_format_prefix("u'doc'"));
    }

    #[test]
    fn small_sources_are_whole() {
        let p = preview("a.py", "x = 1\n", "x", 1024);
        assert_eq!(p.text, "x = 1\n");
        assert!(!p.truncated);
    }

    #[test]
    fn large_python_sources_sample_the_named_declaration() {
        let filler: String = (0..2000).map(|i| format!("value_{i} = {i}\n")).collect();
        let source = format!(
            "import os\n{filler}class Payments:\n    \"\"\"Doc.\"\"\"\n\n    def refund_limit(self, amount):\n        \"\"\"Doc.\"\"\"\n        return amount <= self.captured\n{filler}"
        );
        let p = preview("pay.py", &source, "Where is refund_limit checked?", 16384);
        assert!(p.truncated);
        assert!(p.preview_bytes <= 16384, "{}", p.preview_bytes);
        assert_eq!(p.preview_bytes, p.text.len());
        assert!(
            p.text.contains("query-named implementation"),
            "{}",
            &p.text[..400]
        );
        assert!(p.text.contains("return amount <= self.captured"));
        assert!(p.text.contains("enclosing class context"));
        assert!(p.text.starts_with("--- source lines 1-"));
        assert!(p.text.contains("distributed context"));
    }

    #[test]
    fn python_that_inspection_rejects_gets_no_declaration_windows() {
        let filler: String = (0..2000).map(|i| format!("value_{i} = {i}\n")).collect();
        for extra in ["print 'old'\n", "x = 1\ry = 2\n"] {
            let source = format!("def refund_limit():\n    return 1\n{extra}{filler}");
            let p = preview("pay.py", &source, "refund_limit", 16384);
            assert!(!p.text.contains("query-named"), "{extra:?}");
        }
    }

    #[test]
    fn large_text_gets_opening_and_spread_windows_within_budget() {
        let source: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        let p = preview("notes.txt", &source, "anything", 4096);
        assert!(p.truncated && p.preview_bytes <= 4096);
        assert!(p.text.contains("line 0\n"));
        assert!(
            p.text.contains("line 4999") || p.text.contains("line 4998"),
            "the end is sampled"
        );
    }

    #[test]
    fn a_giant_line_is_clipped_at_a_character_boundary() {
        let source = "é".repeat(20_000);
        let p = preview("a.py", &source, "x", 2048);
        assert!(p.preview_bytes <= 2048);
        assert!(p.text.contains("partial line"));
    }
}
