// Ported from jevgrep (https://github.com/dzhng/jevgrep) at commit
// 82ef1fd3f43161bb17395dba1e07a5338f3913db: packages/core/src/source.ts,
// parser-helpers.mjs and parser-declarations.mjs. TypeScript and JavaScript use tree-sitter
// here, where jevgrep uses the TypeScript compiler.
// Copyright (c) David Zhang. MIT License; see LICENSE-jevgrep.

//! Source units: the declarations or bounded text regions of a snapshot, with their line and
//! byte coordinates in that snapshot.

use serde::Serialize;
use tree_sitter::{Language, Node, Parser, Tree};

/// Lines, from 1, both ends included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Range {
    pub start_line: usize,
    pub end_line: usize,
}

impl Range {
    pub fn new(start_line: usize, end_line: usize) -> Range {
        Range {
            start_line,
            end_line,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    pub range: Range,
    pub byte_start: usize,
    pub byte_end: usize,
    /// True when the unit is a part of a declaration or of the file, not all of one.
    pub partial: bool,
    /// The headers of the classes, impls or modules that hold the unit.
    pub owner_headers: Vec<Range>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Python,
    TypeScript,
    Go,
    Rust,
    Text,
}

#[derive(Debug)]
pub struct Inspection {
    pub units: Vec<Unit>,
    pub comments: Vec<Range>,
    pub mode: Mode,
}

/// The snapshot text with the byte offset where each line starts. Lines are split on LF only,
/// as jevgrep counts them.
pub struct Text<'a> {
    pub source: &'a str,
    /// `offsets[i]` is where line `i + 1` starts; the last entry is the length.
    pub offsets: Vec<usize>,
    pub line_count: usize,
}

impl<'a> Text<'a> {
    pub fn new(source: &'a str) -> Text<'a> {
        let mut offsets = vec![0];
        let mut at = 0;
        let mut line_count = 0;
        for line in source.split('\n') {
            at += line.len() + 1;
            offsets.push(at.min(source.len()));
            line_count += 1;
        }
        Text {
            source,
            offsets,
            line_count,
        }
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.source.as_bytes()
    }

    pub fn len(&self) -> usize {
        self.source.len()
    }

    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    /// The start of line `line` (from 1), or the length past the end.
    pub fn line_start(&self, line: usize) -> usize {
        self.offsets
            .get(line - 1)
            .copied()
            .unwrap_or(self.len())
            .min(self.len())
    }

    /// The end of line `line` with its newline, or the length past the end.
    pub fn line_end(&self, line: usize) -> usize {
        self.offsets
            .get(line)
            .copied()
            .unwrap_or(self.len())
            .min(self.len())
    }

    /// The line that holds byte `byte`.
    pub fn line_at(&self, byte: usize) -> usize {
        // The last offset entry is the end, not a line start.
        let starts = &self.offsets[..self.line_count];
        starts.partition_point(|&start| start <= byte).max(1)
    }

    /// The text of lines `start..=end`, joined by LF, as `lines.slice(...).join("\n")`.
    pub fn lines(&self, start: usize, end: usize) -> &'a str {
        let from = self.line_start(start);
        // Every line but the last ends one byte before the next line starts.
        let to = if end < self.line_count {
            self.line_end(end) - 1
        } else {
            self.len()
        };
        &self.source[from..to.max(from)]
    }
}

/// Splits lines `range` into units of at most `max_bytes`, cut after a newline where one is
/// in reach, else at a character boundary.
///
/// `max_bytes` must be at least 4, the longest UTF-8 character, so every unit holds one.
pub fn text_units(
    text: &Text,
    range: Range,
    name: &str,
    max_bytes: usize,
    partial: bool,
) -> Vec<Unit> {
    assert!(
        max_bytes >= 4,
        "a text unit needs room for one UTF-8 character"
    );
    let raw = text.bytes();
    let first = text.line_start(range.start_line);
    let end = text.line_end(range.end_line);
    let mut units = Vec::new();
    let mut start = first;
    let mut line = range.start_line;
    while start < end {
        let mut finish = end.min(start + max_bytes);
        if finish < end {
            while finish > start && (raw[finish] & 0xc0) == 0x80 {
                finish -= 1;
            }
            if let Some(newline) = raw[start..finish].iter().rposition(|&b| b == b'\n') {
                finish = start + newline + 1;
            }
        }
        let part = &raw[start..finish];
        let newlines = part.iter().filter(|&&b| b == b'\n').count();
        let end_line = line + newlines - usize::from(part.ends_with(b"\n"));
        units.push(Unit {
            name: name.to_string(),
            range: Range::new(line, end_line),
            byte_start: start,
            byte_end: finish,
            partial: partial || first != start || finish != end,
            owner_headers: Vec::new(),
        });
        line += newlines;
        start = finish;
    }
    // The final empty line has no bytes but still belongs to the snapshot coordinates.
    if raw.last() == Some(&b'\n')
        && range.end_line == text.line_count
        && let Some(last) = units.last_mut()
    {
        last.range.end_line = text.line_count;
    }
    units
}

/// Complete-file fragments of at most `max_bytes`.
pub fn split_source(source: &str, max_bytes: usize) -> Vec<Unit> {
    let text = Text::new(source);
    text_units(
        &text,
        Range::new(1, text.line_count),
        "source",
        max_bytes,
        false,
    )
}

/// The language of a path, by its extension, as jevgrep chooses it.
pub fn mode_for(path: &str) -> Mode {
    let extension = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match extension {
        "py" | "pyi" => Mode::Python,
        "ts" | "cts" | "mts" | "js" | "cjs" | "mjs" | "tsx" | "jsx" => Mode::TypeScript,
        "go" => Mode::Go,
        "rs" => Mode::Rust,
        _ => Mode::Text,
    }
}

fn language(path: &str, mode: Mode) -> Option<Language> {
    Some(match mode {
        Mode::Python => tree_sitter_python::LANGUAGE.into(),
        Mode::Go => tree_sitter_go::LANGUAGE.into(),
        Mode::Rust => tree_sitter_rust::LANGUAGE.into(),
        Mode::TypeScript if path.ends_with(".tsx") => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Mode::TypeScript if path.ends_with("ts") => {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
        Mode::TypeScript => tree_sitter_javascript::LANGUAGE.into(),
        Mode::Text => return None,
    })
}

/// Parses `source`, or `None` on a syntax error.
fn parse(source: &str, language: &Language) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;
    let tree = parser.parse(source, None)?;
    (!tree.root_node().has_error()).then_some(tree)
}

/// Parses Python, or `None` when the source has a syntax error, Python 2 syntax, or a bare
/// CR: coordinates count LF lines, so a bare CR would mislabel the original bytes.
pub(super) fn parse_python(source: &str) -> Option<Tree> {
    let bare_cr = source
        .as_bytes()
        .windows(2)
        .any(|w| w[0] == b'\r' && w[1] != b'\n')
        || source.ends_with('\r');
    if bare_cr {
        return None;
    }
    let tree = parse(source, &tree_sitter_python::LANGUAGE.into())?;
    valid_python(tree.root_node(), source).then_some(tree)
}

/// A declaration before it gets byte coordinates.
struct Declaration {
    name: String,
    range: Range,
    owner_headers: Vec<Range>,
}

pub const MAX_PARSE_BYTES: usize = 1_000_000;

/// The units and comments of a snapshot. Declarations come from tree-sitter for Python, Go,
/// Rust, TypeScript, TSX and JavaScript; other text, a syntax error and a file over
/// `max_parse_bytes` give bounded text regions. A declaration over `max_unit_bytes` is split
/// into partial units.
pub fn inspect(
    path: &str,
    source: &str,
    max_unit_bytes: usize,
    max_parse_bytes: usize,
) -> Inspection {
    let text = Text::new(source);
    let mode = mode_for(path);
    // Context windows use conservative whole-line Python comments, including inside strings.
    let python_comments: Vec<Range> = if mode == Mode::Python {
        source
            .split('\n')
            .enumerate()
            .filter(|(_, line)| line.trim_start().starts_with('#'))
            .map(|(i, _)| Range::new(i + 1, i + 1))
            .collect()
    } else {
        Vec::new()
    };
    let fallback = |comments: Vec<Range>| Inspection {
        mode: Mode::Text,
        comments,
        units: if source.is_empty() {
            Vec::new()
        } else {
            text_units(
                &text,
                Range::new(1, text.line_count),
                "source",
                max_unit_bytes,
                true,
            )
        },
    };
    if source.len() > max_parse_bytes {
        return fallback(python_comments);
    }
    let Some(language) = language(path, mode) else {
        return fallback(python_comments);
    };
    let (declarations, mut comments) = match mode {
        Mode::Python => {
            let Some(tree) = parse_python(source) else {
                return fallback(python_comments);
            };
            (
                python_declarations(tree.root_node(), source),
                python_comments,
            )
        }
        Mode::Go | Mode::Rust => {
            let Some(tree) = parse(source, &language) else {
                return fallback(python_comments);
            };
            let comments = all_comments(tree.root_node());
            let declarations = if mode == Mode::Go {
                go_declarations(tree.root_node(), source)
            } else {
                rust_declarations(tree.root_node(), source)
            };
            (declarations, comments)
        }
        Mode::TypeScript => {
            let mut parser = Parser::new();
            let tree = parser
                .set_language(&language)
                .ok()
                .and_then(|_| parser.parse(source, None));
            let Some(tree) = tree else {
                return fallback(Vec::new());
            };
            let comments = all_comments(tree.root_node());
            // Invalid declarations fall back to text, but comments still bound context windows.
            if tree.root_node().has_error() {
                return fallback(sorted(comments));
            }
            (script_declarations(tree.root_node(), source), comments)
        }
        Mode::Text => unreachable!("text has no language"),
    };
    comments = sorted(comments);
    if declarations.is_empty() && !source.is_empty() {
        return Inspection {
            units: text_units(
                &text,
                Range::new(1, text.line_count),
                "source",
                max_unit_bytes,
                true,
            ),
            comments,
            mode,
        };
    }
    let units = declarations
        .into_iter()
        .flat_map(|declaration| {
            // Parser ranges and returned byte spans stay tied to the original snapshot.
            let start = text.line_start(declaration.range.start_line);
            let end = text.line_end(declaration.range.end_line);
            if end - start <= max_unit_bytes {
                vec![Unit {
                    name: declaration.name,
                    range: declaration.range,
                    byte_start: start,
                    byte_end: end,
                    partial: false,
                    owner_headers: declaration.owner_headers,
                }]
            } else {
                text_units(
                    &text,
                    declaration.range,
                    &declaration.name,
                    max_unit_bytes,
                    true,
                )
                .into_iter()
                .map(|unit| Unit {
                    owner_headers: declaration.owner_headers.clone(),
                    ..unit
                })
                .collect()
            }
        })
        .collect();
    Inspection {
        units,
        comments,
        mode,
    }
}

fn sorted(mut comments: Vec<Range>) -> Vec<Range> {
    comments.sort_by_key(|r| r.start_line);
    comments.dedup();
    let mut seen = std::collections::HashSet::new();
    comments.retain(|r| seen.insert(*r));
    comments
}

fn text_of<'s>(node: Node, source: &'s str) -> &'s str {
    &source[node.byte_range()]
}

pub(super) fn named_children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn children<'t>(node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

fn is_comment(node: Node) -> bool {
    node.kind() == "comment" || node.kind().ends_with("_comment")
}

/// jevgrep's range: an end at column 0 belongs to the line before.
fn node_range(node: Node) -> Range {
    let end = node.end_position();
    Range::new(
        node.start_position().row + 1,
        end.row + usize::from(end.column > 0),
    )
}

fn all_comments(root: Node) -> Vec<Range> {
    let mut comments = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_comment(node) {
            comments.push(node_range(node));
        } else {
            stack.extend(named_children(node));
        }
    }
    comments
}

// ---------------------------------------------------------------------------------------
// Python (parser-helpers.mjs)

pub(super) fn unparenthesized(mut node: Option<Node>) -> Option<Node> {
    while let Some(n) = node.filter(|n| n.kind() == "parenthesized_expression") {
        node = named_children(n)
            .into_iter()
            .find(|c| c.kind() != "comment");
    }
    node
}

fn definition(node: Node) -> Option<Node> {
    if node.kind() == "decorated_definition" {
        node.child_by_field_name("definition")
    } else {
        Some(node)
    }
}

fn is_definition(node: Node) -> bool {
    definition(node).is_some_and(|d| matches!(d.kind(), "function_definition" | "class_definition"))
}

pub(super) fn body(node: Node) -> Vec<Node> {
    node.child_by_field_name("body")
        .map(|b| {
            named_children(b)
                .into_iter()
                .filter(|n| n.kind() != "comment")
                .collect()
        })
        .unwrap_or_default()
}

fn python_name(node: Node, source: &str) -> String {
    node.child_by_field_name("name")
        .map(|n| text_of(n, source).to_string())
        .unwrap_or_default()
}

/// Python AST ends exclude trailing comments; tree-sitter puts them in blocks.
pub(super) fn python_end_line(node: Node) -> usize {
    let mut last = node;
    while let Some(child) = children(last)
        .into_iter()
        .rev()
        .find(|c| c.kind() != "comment")
    {
        last = child;
    }
    last.end_position().row + 1
}

pub(super) fn python_start_line(node: Node) -> usize {
    if node.kind() == "decorated_definition" {
        let decorator = named_children(node)
            .into_iter()
            .find(|c| c.kind() == "decorator");
        let expression =
            unparenthesized(decorator.and_then(|d| named_children(d).into_iter().next()));
        return expression
            .or(decorator)
            .unwrap_or(node)
            .start_position()
            .row
            + 1;
    }
    node.start_position().row + 1
}

fn python_range(node: Node) -> Range {
    Range::new(python_start_line(node), python_end_line(node))
}

fn python_declarations(root: Node, source: &str) -> Vec<Declaration> {
    fn visit(
        nodes: Vec<Node>,
        prefix: &str,
        headers: &[Range],
        source: &str,
        units: &mut Vec<Declaration>,
    ) {
        for wrapped in nodes {
            if !is_definition(wrapped) {
                continue;
            }
            let n = definition(wrapped).expect("a definition");
            let named = format!("{prefix}{}", python_name(n, source));
            let r = python_range(wrapped);
            let defs: Vec<Node> = body(n).into_iter().filter(|c| is_definition(*c)).collect();
            if n.kind() == "class_definition" && !defs.is_empty() {
                let first = python_start_line(defs[0]);
                let mut own = headers.to_vec();
                if first > r.start_line {
                    own.push(Range::new(r.start_line, first - 1));
                }
                let mut cursor = r.start_line;
                for child in defs {
                    let start = python_start_line(child);
                    if cursor < start {
                        units.push(Declaration {
                            name: format!("{named}.context"),
                            range: Range::new(cursor, start - 1),
                            owner_headers: own.clone(),
                        });
                    }
                    visit(vec![child], &format!("{named}."), &own, source, units);
                    cursor = python_end_line(child) + 1;
                }
                if cursor <= r.end_line {
                    units.push(Declaration {
                        name: format!("{named}.context"),
                        range: Range::new(cursor, r.end_line),
                        owner_headers: own,
                    });
                }
            } else {
                units.push(Declaration {
                    name: named,
                    range: r,
                    owner_headers: headers.to_vec(),
                });
            }
        }
    }
    let mut units = Vec::new();
    visit(named_children(root), "", &[], source, &mut units);
    units
}

/// More context for selected Python methods: the header of their class (at most 40 lines,
/// up to its first method) and the methods next to them when those are at most 40 lines
/// long. jevgrep's `neighborhood`. Ranges are added, never removed.
pub fn python_neighborhood(source: &str, selected: &[Range]) -> Vec<Range> {
    let Some(tree) = parse_python(source) else {
        return Vec::new();
    };
    let mut extra = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(wrapped) = stack.pop() {
        stack.extend(named_children(wrapped).into_iter().rev());
        if wrapped.kind() == "function_definition"
            && wrapped
                .parent()
                .is_some_and(|p| p.kind() == "decorated_definition")
        {
            continue;
        }
        if definition(wrapped).is_none_or(|n| n.kind() != "function_definition") {
            continue;
        }
        let Some(owner) = wrapped.parent().and_then(|block| block.parent()) else {
            continue;
        };
        if owner.kind() != "class_definition" {
            continue;
        }
        let r = python_range(wrapped);
        if !selected
            .iter()
            .any(|s| s.start_line <= r.end_line && s.end_line >= r.start_line)
        {
            continue;
        }
        let siblings: Vec<Node> = body(owner)
            .into_iter()
            .filter(|n| is_definition(*n))
            .collect();
        let owner_wrapper = owner
            .parent()
            .filter(|p| p.kind() == "decorated_definition")
            .unwrap_or(owner);
        let start = python_start_line(owner_wrapper);
        let end = siblings
            .iter()
            .map(|s| python_start_line(*s).saturating_sub(1))
            .chain(std::iter::once(start + 39))
            .min()
            .unwrap_or(start + 39);
        if start <= end {
            extra.push(Range::new(start, end));
        }
        let index = siblings
            .iter()
            .position(|s| s.id() == wrapped.id())
            .unwrap_or(0);
        for sibling in siblings
            .iter()
            .skip(index.saturating_sub(1))
            .take(if index == 0 { 2 } else { 3 })
        {
            let rr = python_range(*sibling);
            if sibling.id() != wrapped.id() && rr.end_line + 1 - rr.start_line <= 40 {
                extra.push(rr);
            }
        }
    }
    extra
}

/// Tree-sitter accepts some Python 2 syntax. Those forms are rejected; modern Python is
/// accepted. This is structural recognition, not compile validation.
fn valid_python(root: Node, source: &str) -> bool {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        stack.extend(named_children(n));
        let text = text_of(n, source);
        let invalid = match n.kind() {
            "exec_statement" => true,
            "print_statement" => named_children(n)
                .first()
                .is_none_or(|c| c.kind() != "chevron"),
            "except_clause" => {
                let mut cursor = n.walk();
                n.children_by_field_name("value", &mut cursor).count() > 1
            }
            "raise_statement" => named_children(n)
                .first()
                .is_some_and(|c| c.kind() == "expression_list"),
            "for_in_clause" => {
                let mut cursor = n.walk();
                n.children_by_field_name("right", &mut cursor).count() > 1
            }
            "concatenated_string" => {
                let kinds: std::collections::HashSet<bool> = named_children(n)
                    .into_iter()
                    .filter(|c| c.kind() == "string")
                    .map(|c| {
                        let t = text_of(c, source);
                        let prefix: String = t
                            .chars()
                            .take_while(|ch| ch.is_ascii_alphabetic())
                            .collect();
                        let lower = prefix.to_ascii_lowercase();
                        lower.trim_start_matches(['r', 'u']).starts_with('b')
                    })
                    .collect();
                kinds.len() > 1
            }
            "function_definition" | "class_definition" => body(n).is_empty(),
            "integer" => {
                let lower = text.to_ascii_lowercase();
                lower.ends_with('l')
                    || (text.starts_with('0')
                        && text.len() > 1
                        && text.bytes().all(|b| b.is_ascii_digit() || b == b'_')
                        && text.bytes().skip(1).any(|b| (b'1'..=b'9').contains(&b)))
            }
            "comparison_operator" => children(n).iter().any(|c| text_of(*c, source) == "<>"),
            "string_start" => {
                let lower = text.to_ascii_lowercase();
                lower.starts_with("ur") || lower.starts_with("ru")
            }
            "string" => text.starts_with('`'),
            "parameters" | "lambda_parameters" => named_children(n).iter().any(|c| {
                c.kind() == "tuple_pattern"
                    || c.child_by_field_name("name")
                        .is_some_and(|name| name.kind() == "tuple_pattern")
            }),
            "delete_statement" => named_children(n).iter().any(|c| {
                !unparenthesized(Some(*c)).is_some_and(|u| {
                    matches!(
                        u.kind(),
                        "identifier"
                            | "attribute"
                            | "subscript"
                            | "expression_list"
                            | "tuple"
                            | "list"
                    )
                })
            }),
            _ => false,
        };
        if invalid {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------------------
// Go and Rust (parser-declarations.mjs)

fn go_declarations(root: Node, source: &str) -> Vec<Declaration> {
    let mut units = Vec::new();
    for node in named_children(root) {
        if is_comment(node) {
            continue;
        }
        let mut name = node
            .child_by_field_name("name")
            .map(|n| text_of(n, source).to_string())
            .unwrap_or_else(|| node.kind().to_string());
        match node.kind() {
            "method_declaration" => {
                let receiver = node.child_by_field_name("receiver").and_then(|r| {
                    named_children(r)
                        .into_iter()
                        .find(|c| c.kind() == "parameter_declaration")
                });
                let owner = receiver
                    .and_then(|r| r.child_by_field_name("type"))
                    .map(|t| text_of(t, source).trim_start_matches('*').to_string());
                if let Some(owner) = owner.filter(|o| !o.is_empty()) {
                    name = format!("{owner}.{name}");
                }
            }
            // Keep groups intact: iota and omitted constant values depend on earlier specs.
            "type_declaration" | "var_declaration" | "const_declaration" => {
                let specs: Vec<Node> = named_children(node)
                    .into_iter()
                    .flat_map(|c| {
                        if c.kind() == "var_spec_list" {
                            named_children(c)
                        } else {
                            vec![c]
                        }
                    })
                    .collect();
                let names: Vec<&str> = specs
                    .iter()
                    .flat_map(|spec| {
                        let mut cursor = spec.walk();
                        spec.children_by_field_name("name", &mut cursor)
                            .filter(|c| matches!(c.kind(), "identifier" | "type_identifier"))
                            .map(|c| text_of(c, source))
                            .collect::<Vec<_>>()
                    })
                    .collect();
                name = if names.is_empty() {
                    node.kind().to_string()
                } else {
                    names.join(", ")
                };
            }
            "package_clause" => {
                if let Some(id) = named_children(node)
                    .into_iter()
                    .find(|c| c.kind() == "package_identifier")
                {
                    name = format!("package {}", text_of(id, source));
                }
            }
            _ => {}
        }
        units.push(Declaration {
            name,
            range: node_range(node),
            owner_headers: Vec::new(),
        });
    }
    units
}

fn rust_declarations(root: Node, source: &str) -> Vec<Declaration> {
    fn is_attribute(node: Node) -> bool {
        matches!(node.kind(), "attribute_item" | "inner_attribute_item")
    }
    fn start_with_attributes(node: Node) -> usize {
        let mut start = node;
        while let Some(previous) = start.prev_named_sibling() {
            if previous.kind() != "attribute_item" && !is_comment(previous) {
                break;
            }
            let before = previous.prev_named_sibling();
            // A comment at the end of the line before belongs to that line's item.
            if is_comment(previous)
                && before.is_some_and(|b| {
                    !is_comment(b)
                        && b.kind() != "attribute_item"
                        && b.end_position().row == previous.start_position().row
                })
            {
                break;
            }
            start = previous;
        }
        node_range(start).start_line
    }
    fn visit(
        nodes: Vec<Node>,
        prefix: &str,
        headers: &[Range],
        source: &str,
        units: &mut Vec<Declaration>,
    ) {
        let mut owned = headers.to_vec();
        owned.extend(
            nodes
                .iter()
                .filter(|n| n.kind() == "inner_attribute_item")
                .map(|n| node_range(*n)),
        );
        for node in &nodes {
            let node = *node;
            if is_comment(node) || is_attribute(node) {
                continue;
            }
            let body = matches!(
                node.kind(),
                "impl_item" | "trait_item" | "mod_item" | "foreign_mod_item"
            )
            .then(|| node.child_by_field_name("body"))
            .flatten();
            let owner = ["name", "type", "macro"]
                .iter()
                .find_map(|field| node.child_by_field_name(field))
                .map(|n| text_of(n, source).to_string())
                .unwrap_or_else(|| node.kind().to_string());
            let start_line = start_with_attributes(node);
            let has_items = body.is_some_and(|b| {
                named_children(b)
                    .iter()
                    .any(|c| !is_comment(*c) && !is_attribute(*c))
            });
            if let (Some(body), true) = (body, has_items) {
                let header = Range::new(start_line, body.start_position().row + 1);
                let mut with_header = owned.clone();
                with_header.push(header);
                units.push(Declaration {
                    name: format!("{prefix}{owner}.context"),
                    range: header,
                    owner_headers: with_header.clone(),
                });
                let inner = if node.kind() == "foreign_mod_item" {
                    prefix.to_string()
                } else {
                    format!("{prefix}{owner}.")
                };
                visit(named_children(body), &inner, &with_header, source, units);
            } else {
                let range = node_range(node);
                units.push(Declaration {
                    name: format!("{prefix}{owner}"),
                    range: Range::new(start_line, range.end_line),
                    owner_headers: owned.clone(),
                });
            }
        }
    }
    let mut units = Vec::new();
    visit(named_children(root), "", &[], source, &mut units);
    units
}

// ---------------------------------------------------------------------------------------
// TypeScript, TSX and JavaScript (source.ts, on tree-sitter)

const CLASS_KINDS: [&str; 2] = ["class_declaration", "abstract_class_declaration"];
const VARIABLE_KINDS: [&str; 2] = ["lexical_declaration", "variable_declaration"];

fn script_declarations(root: Node, source: &str) -> Vec<Declaration> {
    fn name_of(node: Node, source: &str) -> String {
        // `export ...` is named by what it exports.
        let inner = if node.kind() == "export_statement" {
            node.child_by_field_name("declaration")
        } else {
            None
        };
        let named = inner.unwrap_or(node);
        if let Some(name) = named.child_by_field_name("name") {
            return text_of(name, source).to_string();
        }
        if VARIABLE_KINDS.contains(&named.kind()) {
            let names: Vec<&str> = named_children(named)
                .into_iter()
                .filter(|c| c.kind() == "variable_declarator")
                .filter_map(|d| d.child_by_field_name("name"))
                .map(|n| text_of(n, source))
                .collect();
            if !names.is_empty() {
                return names.join(", ");
            }
        }
        "source".to_string()
    }
    fn add(
        node: Node,
        prefix: &str,
        headers: &[Range],
        source: &str,
        units: &mut Vec<Declaration>,
    ) {
        let name = format!("{prefix}{}", name_of(node, source));
        let class = if node.kind() == "export_statement" {
            node.child_by_field_name("declaration")
                .filter(|d| CLASS_KINDS.contains(&d.kind()))
        } else {
            Some(node).filter(|n| CLASS_KINDS.contains(&n.kind()))
        };
        let members: Vec<Node> = class
            .and_then(|c| c.child_by_field_name("body"))
            .map(|b| {
                named_children(b)
                    .into_iter()
                    .filter(|m| !is_comment(*m) && m.kind() != "decorator")
                    .collect()
            })
            .unwrap_or_default();
        if !members.is_empty() {
            let start = node_range(node).start_line;
            let first = node_range(members[0]).start_line;
            let mut own = headers.to_vec();
            if first > start {
                let header = Range::new(start, first - 1);
                own.push(header);
                units.push(Declaration {
                    name: format!("{name}.context"),
                    range: header,
                    owner_headers: own.clone(),
                });
            }
            for member in members {
                add(member, &format!("{name}."), &own, source, units);
            }
        } else {
            units.push(Declaration {
                name,
                range: node_range(node),
                owner_headers: headers.to_vec(),
            });
        }
    }
    let mut units = Vec::new();
    for statement in named_children(root) {
        if is_comment(statement) {
            continue;
        }
        add(statement, "", &[], source, &mut units);
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(inspection: &Inspection) -> Vec<(String, usize, usize)> {
        inspection
            .units
            .iter()
            .map(|u| (u.name.clone(), u.range.start_line, u.range.end_line))
            .collect()
    }

    fn n(name: &str, start: usize, end: usize) -> (String, usize, usize) {
        (name.to_string(), start, end)
    }

    #[test]
    fn every_grammar_loads() {
        for path in ["a.py", "a.go", "a.rs", "a.ts", "a.tsx", "a.js"] {
            let language = language(path, mode_for(path)).unwrap();
            assert!(Parser::new().set_language(&language).is_ok(), "{path}");
        }
    }

    #[test]
    fn text_coordinates() {
        let text = Text::new("ab\ncd\n");
        assert_eq!(text.line_count, 3);
        assert_eq!(text.offsets, vec![0, 3, 6, 6]);
        assert_eq!(text.line_at(0), 1);
        assert_eq!(text.line_at(3), 2);
        assert_eq!(text.line_at(5), 2);
        assert_eq!(text.lines(1, 2), "ab\ncd");
        assert_eq!(text.lines(2, 3), "cd\n");
    }

    #[test]
    fn text_units_cut_after_newlines_and_keep_the_last_empty_line() {
        let units = split_source("aaaa\nbbbb\ncccc\n", 11);
        let spans: Vec<_> = units
            .iter()
            .map(|u| (u.range, u.byte_start, u.byte_end, u.partial))
            .collect();
        assert_eq!(
            spans,
            vec![
                (Range::new(1, 2), 0, 10, true),
                (Range::new(3, 4), 10, 15, true)
            ],
            "a file cut in pieces gives partial pieces"
        );
        // A line longer than the budget is cut at a character boundary.
        let units = split_source("ééééé", 4);
        assert!(
            units
                .iter()
                .all(|u| "ééééé".get(u.byte_start..u.byte_end).is_some())
        );
        assert_eq!(units.len(), 3);
    }

    #[test]
    #[should_panic(expected = "room for one UTF-8 character")]
    fn a_unit_budget_under_four_bytes_is_refused() {
        split_source("ééé", 1);
    }

    #[test]
    fn a_four_byte_budget_always_progresses() {
        let source = "😀😀😀";
        let units = split_source(source, 4);
        assert_eq!(units.len(), 3);
        assert!(units.iter().all(|u| u.byte_end - u.byte_start == 4));
    }

    #[test]
    fn python_classes_methods_and_context() {
        let source = "import os\n\n@dataclass\nclass Refund:\n    \"\"\"Doc.\"\"\"\n    limit = 3\n\n    def check(self):\n        return 1\n    # trailing comment\n\n    @property\n    def total(self):\n        return 2\n\ndef helper():\n    pass\n";
        let inspection = inspect("pay.py", source, 24_000, MAX_PARSE_BYTES);
        assert_eq!(inspection.mode, Mode::Python);
        assert_eq!(
            names(&inspection),
            vec![
                n("Refund.context", 3, 7),
                n("Refund.check", 8, 9),
                n("Refund.context", 10, 11),
                n("Refund.total", 12, 14),
                n("helper", 16, 17),
            ]
        );
        assert_eq!(inspection.units[1].owner_headers, vec![Range::new(3, 7)]);
        assert_eq!(inspection.comments, vec![Range::new(10, 10)]);
    }

    #[test]
    fn python_two_and_bare_cr_fall_back_to_text() {
        for source in [
            "print 'x'\n",
            "def f():\n    return 0777L\n",
            "x = 1\ry = 2\n",
            "def broken(:\n",
        ] {
            let inspection = inspect("a.py", source, 24_000, MAX_PARSE_BYTES);
            assert_eq!(inspection.mode, Mode::Text, "{source:?}");
            assert!(
                inspection
                    .units
                    .iter()
                    .all(|u| u.name == "source" && u.partial)
            );
        }
    }

    #[test]
    fn python_neighborhood_adds_the_class_header_and_short_neighbours() {
        let source = "class Pay:\n    limit = 3\n\n    def a(self):\n        pass\n\n    def b(self):\n        pass\n\n    def c(self):\n        pass\n\n    def d(self):\n        pass\n";
        let extra = python_neighborhood(source, &[Range::new(7, 8)]);
        assert_eq!(
            extra,
            vec![Range::new(1, 3), Range::new(4, 5), Range::new(10, 11)]
        );
        assert!(
            python_neighborhood(source, &[Range::new(1, 2)]).is_empty(),
            "not a method"
        );
        assert!(python_neighborhood("print 'x'\n", &[Range::new(1, 1)]).is_empty());
    }

    #[test]
    fn go_keeps_groups_and_names_methods() {
        let source = "package pay\n\n// Limit is the cap.\nconst (\n\tA = iota\n\tB\n)\n\ntype Refund struct{}\n\nfunc (r *Refund) Check() bool {\n\treturn true\n}\n\nfunc Helper() {}\n";
        let inspection = inspect("pay.go", source, 24_000, MAX_PARSE_BYTES);
        assert_eq!(
            names(&inspection),
            vec![
                n("package pay", 1, 1),
                n("A, B", 4, 7),
                n("Refund", 9, 9),
                n("Refund.Check", 11, 13),
                n("Helper", 15, 15),
            ]
        );
        assert_eq!(inspection.comments, vec![Range::new(3, 3)]);
    }

    #[test]
    fn rust_keeps_impl_headers_and_attributes() {
        let source = "//! Crate doc.\n#![allow(dead_code)]\n\n/// A refund.\n#[derive(Debug)]\nstruct Refund;\n\nimpl Refund {\n    #[inline]\n    fn check(&self) -> bool { true }\n}\n\nmod inner {\n    fn f() {}\n}\n";
        let inspection = inspect("pay.rs", source, 24_000, MAX_PARSE_BYTES);
        assert_eq!(
            names(&inspection),
            vec![
                n("Refund", 4, 6),
                n("Refund.context", 8, 8),
                n("Refund.check", 9, 10),
                n("inner.context", 13, 13),
                n("inner.f", 14, 14),
            ]
        );
        let check = &inspection.units[2];
        assert_eq!(
            check.owner_headers,
            vec![Range::new(2, 2), Range::new(8, 8)]
        );
    }

    #[test]
    fn typescript_classes_exports_and_variables() {
        let source = "import x from 'y';\n\nexport class Refund {\n  limit = 3;\n\n  // The check.\n  check(): boolean {\n    return true;\n  }\n}\n\nexport const a = 1, b = 2;\nfunction helper() {}\n";
        let inspection = inspect("pay.ts", source, 24_000, MAX_PARSE_BYTES);
        assert_eq!(
            names(&inspection),
            vec![
                n("source", 1, 1),
                n("Refund.context", 3, 3),
                n("Refund.limit", 4, 4),
                n("Refund.check", 7, 9),
                n("a, b", 12, 12),
                n("helper", 13, 13),
            ]
        );
        assert_eq!(inspection.comments, vec![Range::new(6, 6)]);
    }

    #[test]
    fn tsx_and_javascript_parse() {
        let tsx = "export function App(): JSX.Element {\n  return <div>hi</div>;\n}\n";
        assert_eq!(
            names(&inspect("App.tsx", tsx, 24_000, MAX_PARSE_BYTES)),
            vec![n("App", 1, 3)]
        );
        let js = "class A {\n  m() {}\n}\nconst f = () => <b/>;\n";
        let inspection = inspect("a.jsx", js, 24_000, MAX_PARSE_BYTES);
        assert_eq!(
            names(&inspection),
            vec![n("A.context", 1, 1), n("A.m", 2, 2), n("f", 4, 4)]
        );
    }

    #[test]
    fn syntax_errors_and_unknown_text_use_text_units() {
        let inspection = inspect("a.ts", "function (\n// note\n", 24_000, MAX_PARSE_BYTES);
        assert_eq!(inspection.mode, Mode::Text);
        assert_eq!(inspection.comments, vec![Range::new(2, 2)]);
        let inspection = inspect("notes.md", "# Title\n\nText.\n", 24_000, MAX_PARSE_BYTES);
        assert_eq!(names(&inspection), vec![n("source", 1, 4)]);
    }

    #[test]
    fn large_declarations_split_into_partial_units() {
        let body: String = (0..200).map(|i| format!("    x{i} = {i}\n")).collect();
        let source = format!("def big():\n{body}");
        let inspection = inspect("a.py", &source, 1000, MAX_PARSE_BYTES);
        assert!(inspection.units.len() > 1);
        assert!(
            inspection
                .units
                .iter()
                .all(|u| u.name == "big" && u.partial && u.byte_end - u.byte_start <= 1000)
        );
        assert_eq!(inspection.units.first().unwrap().range.start_line, 1);
        assert_eq!(inspection.units.last().unwrap().range.end_line, 201);
    }
}
