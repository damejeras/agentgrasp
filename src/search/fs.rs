// Ported from jevgrep (https://github.com/dzhng/jevgrep) at commit
// 82ef1fd3f43161bb17395dba1e07a5338f3913db, packages/core/src/filesystem.ts.
// Copyright (c) David Zhang. MIT License; see LICENSE-jevgrep.

//! Which files a search may read, and the one snapshot of each file that it reads.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;

use crate::mcp::locations::is_sensitive_name;

/// Directory names that hold dependencies or build output, as in jevgrep.
pub const DEPENDENCY_DIRECTORIES: [&str; 13] = [
    "node_modules",
    "vendor",
    "venv",
    ".venv",
    ".tox",
    "__pycache__",
    "dist",
    "build",
    "coverage",
    "target",
    ".next",
    ".nuxt",
    ".turbo",
];
/// VCS metadata. Hidden names are excluded anyway; the names without a dot are not.
pub const VCS_DIRECTORIES: [&str; 6] = [".git", ".hg", ".svn", ".bzr", "_darcs", "CVS"];
/// The largest file a search reads.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// What the report lists as the exclusions in effect.
pub const EXCLUSIONS: [&str; 9] = [
    "git ignore rules (.gitignore, .git/info/exclude, the global excludes file) and .ignore files",
    "hidden paths",
    "VCS metadata directories",
    "dependency and build directories",
    "binary files (control characters other than tab, newline, carriage return and form feed)",
    "credential files (by name, and files that hold a private key)",
    "the agentgrasp state directory",
    "directory symlinks, and file symlinks whose target is outside the scope",
    "the exclude patterns of the call",
];

/// A problem that kept a candidate file from being processed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    Unreadable,
    Changed,
    SizeLimit,
    Encoding,
}

#[derive(Clone, Debug, Serialize)]
pub struct Issue {
    pub kind: IssueKind,
    /// The path relative to the scope.
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Directory { path: String },
    File { path: String, bytes: u64 },
}

impl Entry {
    pub fn path(&self) -> &str {
        match self {
            Entry::Directory { path } | Entry::File { path, .. } => path,
        }
    }
}

#[derive(Default, Debug)]
pub struct Listing {
    /// Eligible children, sorted by path.
    pub entries: Vec<Entry>,
    /// Children left out, by reason. Children that ignore rules or the hidden rule leave out
    /// are not seen, so they are not counted.
    pub excluded: BTreeMap<&'static str, u64>,
    pub issues: Vec<Issue>,
}

/// The one copy of a file that a search reads. Every preview, region and line number of the
/// file comes from it.
#[derive(Debug)]
pub struct Snapshot {
    /// Relative to the scope, with `/`.
    pub path: String,
    pub source: String,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub enum FileRead {
    Ok(Arc<Snapshot>),
    Excluded(&'static str),
    Issue(IssueKind),
}

pub struct Policy {
    /// The scope, resolved.
    pub scope: PathBuf,
    pub include: Option<GlobSet>,
    pub exclude: Option<Exclude>,
    /// Paths never read, resolved: the agentgrasp state directory.
    pub protected: Vec<PathBuf>,
}

/// Compiles globs relative to the scope. `*` and `?` do not match `/`; `**` does.
pub fn globs(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut set = GlobSetBuilder::new();
    for (i, pattern) in patterns.iter().enumerate() {
        if pattern.is_empty()
            || pattern.starts_with('/')
            || pattern.split('/').any(|part| part == "..")
        {
            return Err(format!(
                "pattern {i} must be a non-empty path relative to the scope"
            ));
        }
        let glob: Glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|error| format!("pattern {i} is not a valid glob: {}", error.kind()))?;
        set.add(glob);
    }
    set.build().map(Some).map_err(|error| error.to_string())
}

/// Exclude patterns. A directory is pruned when a pattern matches it, or when a pattern ends
/// in `/**`, `/*` or `/**/*` and its part before that matches it: such a pattern matches every
/// child. A pattern like `generated/*.rs` excludes files one by one and prunes nothing.
pub struct Exclude {
    paths: GlobSet,
    subtrees: Option<GlobSet>,
}

pub fn exclude_globs(patterns: &[String]) -> Result<Option<Exclude>, String> {
    let Some(paths) = globs(patterns)? else {
        return Ok(None);
    };
    let prefixes: Vec<String> = patterns
        .iter()
        .filter_map(|p| {
            ["/**/*", "/**", "/*"]
                .iter()
                .find_map(|suffix| p.strip_suffix(suffix))
        })
        .filter(|prefix| !prefix.is_empty())
        .map(String::from)
        .collect();
    let subtrees = globs(&prefixes)?;
    Ok(Some(Exclude { paths, subtrees }))
}

pub struct Filesystem {
    policy: Policy,
    snapshots: Mutex<HashMap<String, Arc<OnceLock<FileRead>>>>,
}

impl Filesystem {
    pub fn new(policy: Policy) -> Filesystem {
        Filesystem {
            policy,
            snapshots: Mutex::new(HashMap::new()),
        }
    }

    pub fn scope(&self) -> &Path {
        &self.policy.scope
    }

    pub fn absolute(&self, relative: &str) -> PathBuf {
        if relative.is_empty() {
            self.policy.scope.clone()
        } else {
            self.policy.scope.join(relative)
        }
    }

    /// The eligible children of a directory, given relative to the scope ("" is the scope).
    pub fn list(&self, directory: &str) -> Listing {
        let mut listing = Listing::default();
        let absolute = self.absolute(directory);
        let mut walk = ignore::WalkBuilder::new(&absolute);
        walk.max_depth(Some(1))
            .follow_links(false)
            .sort_by_file_name(|a, b| a.cmp(b));
        for entry in walk.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    let path = error_path(&error)
                        .map(|p| self.relative(&p))
                        .unwrap_or_else(|| directory.to_string());
                    listing.issues.push(Issue {
                        kind: IssueKind::Unreadable,
                        path,
                    });
                    continue;
                }
            };
            // The walker attaches a malformed ignore file to the entry of its directory.
            if let Some(error) = entry.error() {
                let path = error_path(error)
                    .map(|p| self.relative(&p))
                    .unwrap_or_else(|| directory.to_string());
                listing.issues.push(Issue {
                    kind: IssueKind::Unreadable,
                    path,
                });
            }
            if entry.depth() == 0 {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if directory.is_empty() {
                name.clone()
            } else {
                format!("{directory}/{name}")
            };
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            let reason = if file_type.is_dir() {
                self.directory_exclusion(&name, &path, entry.path())
            } else if file_type.is_file() {
                self.file_exclusion(&name, &path, entry.path())
            } else if file_type.is_symlink() {
                self.symlink_exclusion(&name, &path, entry.path())
            } else {
                Some("special_file")
            };
            if let Some(reason) = reason {
                *listing.excluded.entry(reason).or_default() += 1;
                continue;
            }
            if file_type.is_dir() {
                listing.entries.push(Entry::Directory { path });
            } else {
                let bytes = std::fs::metadata(entry.path())
                    .map(|m| m.len())
                    .unwrap_or(0);
                listing.entries.push(Entry::File { path, bytes });
            }
        }
        listing.entries.sort_by(|a, b| a.path().cmp(b.path()));
        listing
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.policy.scope)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    fn is_protected(&self, absolute: &Path) -> bool {
        self.policy
            .protected
            .iter()
            .any(|p| absolute.starts_with(p))
    }

    fn excluded_by_pattern(&self, path: &str) -> bool {
        self.policy
            .exclude
            .as_ref()
            .is_some_and(|exclude| exclude.paths.is_match(path))
    }

    fn subtree_excluded(&self, path: &str) -> bool {
        self.policy
            .exclude
            .as_ref()
            .and_then(|exclude| exclude.subtrees.as_ref())
            .is_some_and(|set| set.is_match(path))
    }

    fn directory_exclusion(&self, name: &str, path: &str, absolute: &Path) -> Option<&'static str> {
        if self.is_protected(absolute) {
            Some("protected")
        } else if name.starts_with('.') {
            // Also when an ignore rule such as `!.name` admits it.
            Some("hidden")
        } else if VCS_DIRECTORIES.contains(&name) {
            Some("vcs")
        } else if DEPENDENCY_DIRECTORIES.contains(&name) {
            Some("dependency")
        } else if is_sensitive_name(name.as_ref()) {
            Some("sensitive_name")
        } else if self.excluded_by_pattern(path) || self.subtree_excluded(path) {
            Some("exclude_pattern")
        } else {
            None
        }
    }

    // A directory is never dropped for not matching an include pattern: only files are.
    fn file_exclusion(&self, name: &str, path: &str, absolute: &Path) -> Option<&'static str> {
        if self.is_protected(absolute) {
            Some("protected")
        } else if name.starts_with('.') {
            Some("hidden")
        } else if is_sensitive_name(name.as_ref()) {
            Some("sensitive_name")
        } else if self.excluded_by_pattern(path) {
            Some("exclude_pattern")
        } else if self
            .policy
            .include
            .as_ref()
            .is_some_and(|set| !set.is_match(path))
        {
            Some("not_included")
        } else {
            None
        }
    }

    /// Directory symlinks are not followed. A file symlink is read when its target is a
    /// regular file inside the scope that the name rules allow.
    fn symlink_exclusion(&self, name: &str, path: &str, absolute: &Path) -> Option<&'static str> {
        let Ok(target) = std::fs::canonicalize(absolute) else {
            return Some("symlink");
        };
        match std::fs::metadata(&target) {
            Ok(meta) if meta.is_file() => {}
            _ => return Some("symlink"),
        }
        self.file_exclusion(name, path, absolute)
            .or_else(|| self.target_exclusion(&target))
    }

    /// The name rules applied to every name of a resolved file below the scope, so a symlink
    /// cannot reach a file that the listing would leave out. Ignore rules are not applied here.
    fn target_exclusion(&self, target: &Path) -> Option<&'static str> {
        let Ok(below) = target.strip_prefix(&self.policy.scope) else {
            return Some("symlink");
        };
        let names: Vec<String> = below
            .iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        let (file, directories) = names.split_last()?;
        let mut path = String::new();
        let mut absolute = self.policy.scope.clone();
        for name in directories {
            path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            absolute.push(name);
            if let Some(reason) = self.directory_exclusion(name, &path, &absolute) {
                return Some(reason);
            }
        }
        let path = if path.is_empty() {
            file.clone()
        } else {
            format!("{path}/{file}")
        };
        self.file_exclusion(file, &path, target)
    }

    /// The snapshot of a file, read once per search, also when callers ask at the same time.
    pub fn read(&self, path: &str) -> FileRead {
        let cell = self
            .snapshots
            .lock()
            .unwrap()
            .entry(path.to_string())
            .or_default()
            .clone();
        cell.get_or_init(|| self.read_once(path)).clone()
    }

    fn read_once(&self, path: &str) -> FileRead {
        // The path may have become a symlink since it was listed: check where it leads now.
        let absolute = match std::fs::canonicalize(self.absolute(path)) {
            Ok(target) => target,
            Err(_) => return FileRead::Issue(IssueKind::Unreadable),
        };
        if let Some(reason) = self.target_exclusion(&absolute) {
            return FileRead::Excluded(reason);
        }
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&absolute);
        let Ok(mut file) = file else {
            return FileRead::Issue(IssueKind::Unreadable);
        };
        let Ok(before) = file.metadata() else {
            return FileRead::Issue(IssueKind::Unreadable);
        };
        if !before.is_file() {
            return FileRead::Excluded("special_file");
        }
        if before.len() > MAX_FILE_BYTES {
            return FileRead::Issue(IssueKind::SizeLimit);
        }
        let mut bytes = Vec::new();
        if (&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return FileRead::Issue(IssueKind::Unreadable);
        }
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return FileRead::Issue(IssueKind::SizeLimit);
        }
        let Ok(after) = file.metadata() else {
            return FileRead::Issue(IssueKind::Unreadable);
        };
        let stamp = |m: &std::fs::Metadata| {
            (
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        if stamp(&before) != stamp(&after) || bytes.len() as u64 != after.len() {
            return FileRead::Issue(IssueKind::Changed);
        }
        if bytes
            .iter()
            .any(|&b| matches!(b, 0x00..=0x08 | 0x0b | 0x0e..=0x1f | 0x7f))
        {
            return FileRead::Excluded("binary");
        }
        let sha256 = crate::sha256_hex(&bytes);
        let Ok(source) = String::from_utf8(bytes) else {
            return FileRead::Issue(IssueKind::Encoding);
        };
        if has_private_key(&source) {
            return FileRead::Excluded("private_key");
        }
        FileRead::Ok(Arc::new(Snapshot {
            path: path.to_string(),
            source,
            sha256,
        }))
    }
}

/// jevgrep's `-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----`.
fn has_private_key(source: &str) -> bool {
    source.match_indices("-----BEGIN ").any(|(at, prefix)| {
        let rest = &source[at + prefix.len()..];
        let Some(end) = rest.find("PRIVATE KEY-----") else {
            return false;
        };
        let words = &rest[..end];
        words.is_empty()
            || (words.ends_with(' ')
                && words[..words.len() - 1].split(' ').all(|w| {
                    !w.is_empty()
                        && w.bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                }))
    })
}

fn error_path(error: &ignore::Error) -> Option<PathBuf> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path.clone()),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Tree {
        _temp: tempfile::TempDir,
        root: PathBuf,
    }

    impl Tree {
        fn new() -> Tree {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            Tree { _temp: temp, root }
        }

        fn file(&self, path: &str, content: &[u8]) {
            let path = self.root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn fs(&self, include: &[&str], exclude: &[&str]) -> Filesystem {
            let strings = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            Filesystem::new(Policy {
                scope: self.root.clone(),
                include: globs(&strings(include)).unwrap(),
                exclude: exclude_globs(&strings(exclude)).unwrap(),
                protected: vec![self.root.join("state")],
            })
        }
    }

    fn paths(listing: &Listing) -> Vec<&str> {
        listing.entries.iter().map(Entry::path).collect()
    }

    #[test]
    fn default_exclusions() {
        let tree = Tree::new();
        for path in [
            "src/main.rs",
            ".hidden/x.rs",
            ".env",
            "node_modules/a.js",
            "target/b.rs",
            "CVS/c",
            "secrets.json",
            "key.pem",
            "state/captures/1/stdout.log",
            "README.md",
        ] {
            tree.file(path, b"text");
        }
        let fs = tree.fs(&[], &[]);
        assert_eq!(paths(&fs.list("")), vec!["README.md", "src"]);
        let listing = fs.list("");
        assert_eq!(listing.excluded.get("dependency"), Some(&2));
        assert_eq!(listing.excluded.get("protected"), Some(&1));
        assert_eq!(listing.excluded.get("sensitive_name"), Some(&2));
        assert_eq!(listing.excluded.get("vcs"), Some(&1));
        assert_eq!(paths(&fs.list("src")), vec!["src/main.rs"]);
    }

    #[test]
    fn git_ignore_rules_apply_and_untracked_files_stay() {
        let tree = Tree::new();
        std::fs::create_dir_all(tree.root.join(".git")).unwrap();
        tree.file(".gitignore", b"*.log\n/generated/\n");
        tree.file("sub/.gitignore", b"local.txt\n");
        for path in [
            "a.log",
            "generated/x.rs",
            "keep.rs",
            "sub/local.txt",
            "sub/other.txt",
            "sub/deep/a.log",
        ] {
            tree.file(path, b"text");
        }
        let fs = tree.fs(&[], &[]);
        assert_eq!(paths(&fs.list("")), vec!["keep.rs", "sub"]);
        assert_eq!(paths(&fs.list("sub")), vec!["sub/deep", "sub/other.txt"]);
        assert!(paths(&fs.list("sub/deep")).is_empty());
    }

    #[test]
    fn include_and_exclude_patterns() {
        let tree = Tree::new();
        for path in [
            "main.go",
            "a/b.go",
            "a/b.rs",
            "a/generated/c.go",
            "docs/readme.md",
        ] {
            tree.file(path, b"text");
        }
        let fs = tree.fs(&["**/*.go"], &["**/generated/**"]);
        assert_eq!(
            paths(&fs.list("")),
            vec!["a", "docs", "main.go"],
            "directories stay"
        );
        assert_eq!(
            paths(&fs.list("a")),
            vec!["a/b.go"],
            "exclusion wins over inclusion"
        );
        assert!(paths(&fs.list("docs")).is_empty());
        let fs = tree.fs(&["*.go", "docs/*"], &[]);
        assert_eq!(paths(&fs.list("")), vec!["a", "docs", "main.go"]);
        assert_eq!(
            paths(&fs.list("a")),
            vec!["a/generated"],
            "* does not cross /"
        );
        assert_eq!(
            paths(&fs.list("docs")),
            vec!["docs/readme.md"],
            "include patterns are ORed"
        );
    }

    #[test]
    fn bad_patterns_are_refused() {
        for pattern in ["/abs/*.rs", "../x", "a/../b", "", "[unclosed"] {
            assert!(globs(&[pattern.to_string()]).is_err(), "{pattern}");
        }
    }

    #[test]
    fn symlinks() {
        let tree = Tree::new();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "secret").unwrap();
        tree.file("real/lib.rs", b"fn lib() {}");
        symlink(tree.root.join("real"), tree.root.join("dirlink")).unwrap();
        symlink(tree.root.join("real/lib.rs"), tree.root.join("inside.rs")).unwrap();
        symlink(
            outside.path().join("secret.rs"),
            tree.root.join("outside.rs"),
        )
        .unwrap();
        symlink(tree.root.join("missing.rs"), tree.root.join("dangling.rs")).unwrap();
        tree.file(".env", b"KEY=1");
        symlink(tree.root.join(".env"), tree.root.join("config.rs")).unwrap();
        let fs = tree.fs(&[], &[]);
        assert_eq!(paths(&fs.list("")), vec!["inside.rs", "real"]);
        assert_eq!(fs.list("").excluded.get("symlink"), Some(&3));
        // config.rs leads to .env; .env itself is dropped by the walker, so it is not counted.
        assert_eq!(fs.list("").excluded.get("hidden"), Some(&1));
        let FileRead::Ok(snapshot) = fs.read("inside.rs") else {
            panic!("inside link is read")
        };
        assert_eq!(snapshot.source, "fn lib() {}");
        assert_eq!(snapshot.path, "inside.rs");
        assert!(matches!(
            fs.read("outside.rs"),
            FileRead::Excluded("symlink")
        ));
    }

    #[test]
    fn a_file_replaced_by_a_symlink_after_listing_is_checked_again() {
        let tree = Tree::new();
        tree.file("a.rs", b"code");
        tree.file("secrets.json", b"{}");
        tree.file("state/captures/1/stdout.log", b"out");
        tree.file("b.rs", b"code");
        let fs = tree.fs(&[], &[]);
        assert_eq!(paths(&fs.list("")), vec!["a.rs", "b.rs"]);
        std::fs::remove_file(tree.root.join("a.rs")).unwrap();
        symlink(tree.root.join("secrets.json"), tree.root.join("a.rs")).unwrap();
        std::fs::remove_file(tree.root.join("b.rs")).unwrap();
        symlink(
            tree.root.join("state/captures/1/stdout.log"),
            tree.root.join("b.rs"),
        )
        .unwrap();
        assert!(matches!(
            fs.read("a.rs"),
            FileRead::Excluded("sensitive_name")
        ));
        assert!(matches!(fs.read("b.rs"), FileRead::Excluded("protected")));
    }

    #[test]
    fn hidden_names_stay_out_when_an_ignore_rule_admits_them() {
        let tree = Tree::new();
        std::fs::create_dir_all(tree.root.join(".git")).unwrap();
        tree.file(".ignore", b"!.hidden.txt\n!.hiddendir/\n");
        tree.file(".hidden.txt", b"x");
        tree.file(".hiddendir/a.txt", b"x");
        tree.file("seen.txt", b"x");
        let fs = tree.fs(&[], &[]);
        assert_eq!(paths(&fs.list("")), vec!["seen.txt"]);
    }

    #[test]
    fn a_malformed_ignore_file_is_an_issue() {
        let tree = Tree::new();
        std::fs::create_dir_all(tree.root.join(".git")).unwrap();
        tree.file("sub/.gitignore", b"{unclosed\n");
        tree.file("sub/x.txt", b"x");
        let fs = tree.fs(&[], &[]);
        let listing = fs.list("sub");
        assert_eq!(listing.issues.len(), 1, "{listing:?}");
    }

    #[test]
    fn pruning_needs_a_pattern_that_matches_every_child() {
        let tree = Tree::new();
        for path in ["a/long.rs", "b/x.rs", "c/x.rs", "d/x.rs"] {
            tree.file(path, b"x");
        }
        let fs = tree.fs(
            &[],
            &[
                "a/?",
                "a/???",
                "a/????????",
                "a/*[!s]",
                "b/**",
                "c/*",
                "d/*.rs",
            ],
        );
        let listing = fs.list("");
        assert_eq!(paths(&listing), vec!["a", "d"]);
        assert_eq!(paths(&fs.list("a")), vec!["a/long.rs"]);
        assert!(paths(&fs.list("d")).is_empty());
    }

    #[test]
    fn concurrent_reads_read_once() {
        let tree = Tree::new();
        tree.file("a.rs", b"one");
        let fs = tree.fs(&[], &[]);
        let reads: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16).map(|_| scope.spawn(|| fs.read("a.rs"))).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let FileRead::Ok(first) = &reads[0] else {
            panic!()
        };
        for read in &reads {
            let FileRead::Ok(snapshot) = read else {
                panic!()
            };
            assert!(Arc::ptr_eq(first, snapshot));
        }
    }

    #[test]
    fn snapshots_are_hashed_and_read_once() {
        let tree = Tree::new();
        tree.file("a.rs", b"one");
        let fs = tree.fs(&[], &[]);
        let FileRead::Ok(first) = fs.read("a.rs") else {
            panic!()
        };
        assert_eq!(first.sha256, crate::sha256_hex(b"one"));
        tree.file("a.rs", b"two");
        let FileRead::Ok(second) = fs.read("a.rs") else {
            panic!()
        };
        assert!(
            Arc::ptr_eq(&first, &second),
            "the search keeps its one snapshot"
        );
    }

    #[test]
    fn content_rules() {
        let tree = Tree::new();
        tree.file("bin.dat", b"abc\x00def");
        tree.file("ansi.log", b"\x1b[31mred");
        tree.file("latin1.txt", b"caf\xe9");
        tree.file("key.txt", b"x\n-----BEGIN RSA PRIVATE KEY-----\nabc");
        tree.file("ok.txt", b"tab\tform\x0cfeed\r\n");
        let fs = tree.fs(&[], &[]);
        assert!(matches!(fs.read("bin.dat"), FileRead::Excluded("binary")));
        assert!(matches!(fs.read("ansi.log"), FileRead::Excluded("binary")));
        assert!(matches!(
            fs.read("latin1.txt"),
            FileRead::Issue(IssueKind::Encoding)
        ));
        assert!(matches!(
            fs.read("key.txt"),
            FileRead::Excluded("private_key")
        ));
        assert!(matches!(fs.read("ok.txt"), FileRead::Ok(_)));
        assert!(matches!(
            fs.read("missing.txt"),
            FileRead::Issue(IssueKind::Unreadable)
        ));
    }

    #[test]
    fn private_key_pattern() {
        assert!(has_private_key("-----BEGIN PRIVATE KEY-----"));
        assert!(has_private_key("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(has_private_key("-----BEGIN EC2 X PRIVATE KEY-----"));
        assert!(!has_private_key("-----BEGIN PUBLIC KEY-----"));
        assert!(!has_private_key("-----BEGIN my PRIVATE KEY-----"));
    }

    #[test]
    fn large_files_are_an_issue() {
        let tree = Tree::new();
        let file = File::create(tree.root.join("big.txt")).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        let fs = tree.fs(&[], &[]);
        assert!(matches!(
            fs.read("big.txt"),
            FileRead::Issue(IssueKind::SizeLimit)
        ));
    }
}
