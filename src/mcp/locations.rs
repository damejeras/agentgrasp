//! Which paths the MCP tools may read: paths inside an MCP root, and capture directories whose
//! command ran inside a root.

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Symlinks followed in one resolution before it gives up, as Linux does (ELOOP).
const MAX_LINKS: usize = 40;

/// Turns the `file://` URIs of MCP roots into absolute paths. Other schemes are ignored.
pub fn root_paths<'a>(uris: impl IntoIterator<Item = &'a str>) -> Vec<PathBuf> {
    uris.into_iter().filter_map(file_uri_path).collect()
}

fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///p` and `file://localhost/p` are local; another host is not.
    let path = match rest.strip_prefix("localhost") {
        Some(path) => path,
        None => rest,
    };
    if !path.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut iter = path.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let hex = [iter.next()?, iter.next()?];
            bytes.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    use std::os::unix::ffi::OsStrExt;
    let path = PathBuf::from(OsStr::from_bytes(&bytes));
    Some(normalize(&path))
}

/// Removes `.` and resolves `..` by text only. `path` must be absolute.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// The real location of `path`, as `realpath` would give it, except that a path that cannot be
/// read still resolves: every symlink that can be read is followed (dangling ones too), and a
/// name that cannot be read stays as it is. So a missing or unreadable file can still be
/// checked for containment before the tool reports it as unreadable. Only a symlink loop
/// fails.
pub fn resolve(path: &Path) -> io::Result<PathBuf> {
    let mut links = 0;
    resolve_from(PathBuf::from("/"), path, &mut links)
}

fn resolve_from(mut resolved: PathBuf, path: &Path, links: &mut usize) -> io::Result<PathBuf> {
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        match component {
            Component::RootDir => resolved = PathBuf::from("/"),
            Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                let candidate = resolved.join(name);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        *links += 1;
                        if *links > MAX_LINKS {
                            return Err(io::Error::other("too many levels of symbolic links"));
                        }
                        let target = std::fs::read_link(&candidate)?;
                        let rest: PathBuf = components.collect();
                        let base = if target.is_absolute() {
                            PathBuf::from("/")
                        } else {
                            resolved.clone()
                        };
                        let joined = target.join(rest);
                        return resolve_from(base, &joined, links);
                    }
                    // A name that is missing, below a file, or in a directory that cannot be
                    // read is no symlink, so it resolves to itself. A later `..` can still
                    // climb back into existing directories, so the walk goes on.
                    Ok(_) | Err(_) => resolved = candidate,
                }
            }
        }
    }
    Ok(resolved)
}

/// The jevgrep rule for credential files: a path name that is `.env`, `.env.*`, a known
/// credential file name, or ends in a key or certificate suffix. Case is ignored.
pub fn is_sensitive_name(name: &OsStr) -> bool {
    const NAMES: [&str; 12] = [
        "credentials",
        "credentials.json",
        "secrets.json",
        "secrets.yaml",
        "secrets.yml",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        ".netrc",
        ".npmrc",
        ".pypirc",
    ];
    const SUFFIXES: [&str; 4] = [".pem", ".key", ".p12", ".pfx"];
    let lower = name.to_string_lossy().to_lowercase();
    lower == ".env"
        || lower.starts_with(".env.")
        || NAMES.contains(&lower.as_str())
        || SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// True when a name of `path` below `base` is a credential name.
pub fn has_sensitive_name(base: &Path, path: &Path) -> bool {
    path.strip_prefix(base)
        .map(|rest| rest.iter().any(is_sensitive_name))
        .unwrap_or(false)
}

/// Directories as given and as they resolve, so a directory that is itself a symlink contains
/// both its own paths and its target's.
pub struct Roots(Vec<PathBuf>);

impl Roots {
    pub fn new(paths: Vec<PathBuf>) -> Roots {
        let mut all = Vec::new();
        for path in paths {
            if let Ok(real) = std::fs::canonicalize(&path) {
                all.push(real);
            }
            all.push(path);
        }
        Roots(all)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The root that holds `path`, or none.
    pub fn containing(&self, path: &Path) -> Option<&Path> {
        self.0
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.as_os_str().len())
            .map(PathBuf::as_path)
    }
}

/// Why a path is not allowed. Each value is a message for `invalid_input`.
#[derive(Debug, PartialEq, Eq)]
pub enum Denied {
    NotAbsolute,
    OutsideRoots,
    Sensitive,
    Unresolvable,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Denied::NotAbsolute => "is not an absolute path",
            Denied::OutsideRoots => {
                "is not inside an MCP root or a capture made inside an MCP root"
            }
            Denied::Sensitive => "matches the credential-file exclusion",
            Denied::Unresolvable => "cannot be resolved",
        })
    }
}

/// A path that `ask` may read.
#[derive(Debug)]
pub struct Allowed {
    /// The path as given.
    pub given: PathBuf,
    /// The path with every symlink resolved.
    pub resolved: PathBuf,
}

/// Checks a path for `ask`. The supplied path and its resolved form must both be inside a root,
/// or both inside one capture directory whose recorded working directory is inside a root.
/// Neither may hold a credential name below that location.
pub fn allow_file(path: &Path, roots: &Roots, captures: &Roots) -> Result<Allowed, Denied> {
    if !path.is_absolute() {
        return Err(Denied::NotAbsolute);
    }
    let given = normalize(path);
    let resolved = resolve(path).map_err(|_| Denied::Unresolvable)?;
    let base = match (roots.containing(&given), roots.containing(&resolved)) {
        (Some(a), Some(b)) => Some((a.to_path_buf(), b.to_path_buf())),
        _ => None,
    };
    let (given_base, resolved_base) = match base {
        Some(bases) => bases,
        None => {
            let given_capture = capture_dir(&given, captures).ok_or(Denied::OutsideRoots)?;
            let resolved_capture = capture_dir(&resolved, captures).ok_or(Denied::OutsideRoots)?;
            if given_capture.file_name() != resolved_capture.file_name()
                || capture_cwd(&resolved_capture).is_none_or(|cwd| roots.containing(&cwd).is_none())
            {
                return Err(Denied::OutsideRoots);
            }
            (given_capture, resolved_capture)
        }
    };
    if has_sensitive_name(&given_base, &given) || has_sensitive_name(&resolved_base, &resolved) {
        return Err(Denied::Sensitive);
    }
    Ok(Allowed { given, resolved })
}

/// Checks a directory for `search`: it and its resolved form must be inside a root.
pub fn allow_scope(path: &Path, roots: &Roots) -> Result<PathBuf, Denied> {
    if !path.is_absolute() {
        return Err(Denied::NotAbsolute);
    }
    let given = normalize(path);
    let resolved = resolve(path).map_err(|_| Denied::Unresolvable)?;
    if roots.containing(&given).is_none() || roots.containing(&resolved).is_none() {
        return Err(Denied::OutsideRoots);
    }
    Ok(resolved)
}

/// `<captures>/<id>` when `path` is below it. `captures` holds the captures directory as
/// configured and as it resolves.
fn capture_dir(path: &Path, captures: &Roots) -> Option<PathBuf> {
    let base = captures.containing(path)?;
    match path.strip_prefix(base).ok()?.components().next()? {
        Component::Normal(id) => Some(base.join(id)),
        _ => None,
    }
}

/// The working directory that the capture's `metadata.json` records, resolved.
fn capture_cwd(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read(dir.join("metadata.json")).ok()?;
    let metadata: serde_json::Value = serde_json::from_slice(&text).ok()?;
    let cwd = PathBuf::from(metadata.get("cwd")?.as_str()?);
    cwd.is_absolute().then(|| resolve(&cwd).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn real(dir: &tempfile::TempDir) -> PathBuf {
        std::fs::canonicalize(dir.path()).unwrap()
    }

    #[test]
    fn file_uris_become_paths() {
        let paths = root_paths([
            "file:///a/b%20c",
            "file://localhost/x/./y/..",
            "https://h/p",
            "file://host/p",
        ]);
        assert_eq!(paths, vec![PathBuf::from("/a/b c"), PathBuf::from("/x")]);
    }

    #[test]
    fn resolve_follows_links_and_keeps_a_missing_tail() {
        let temp = tempfile::tempdir().unwrap();
        let base = real(&temp);
        std::fs::create_dir(base.join("dir")).unwrap();
        symlink(base.join("dir"), base.join("link")).unwrap();
        symlink("dir/missing", base.join("dangling")).unwrap();
        assert_eq!(
            resolve(&base.join("link/new/file")).unwrap(),
            base.join("dir/new/file")
        );
        assert_eq!(
            resolve(&base.join("dangling")).unwrap(),
            base.join("dir/missing")
        );
        assert_eq!(
            resolve(&base.join("link/../dir")).unwrap(),
            base.join("dir")
        );
        symlink("loop", base.join("loop")).unwrap();
        assert!(resolve(&base.join("loop")).is_err());
        // `..` after a missing name climbs back into a real directory, whose links count.
        assert_eq!(
            resolve(&base.join("missing/../link/f")).unwrap(),
            base.join("dir/f")
        );
        std::fs::write(base.join("file"), "x").unwrap();
        assert_eq!(
            resolve(&base.join("file/child")).unwrap(),
            base.join("file/child")
        );
    }

    #[test]
    fn credential_names() {
        for name in [
            ".env",
            ".ENV.local",
            "id_rsa",
            "cert.PEM",
            "secrets.yml",
            "credentials",
        ] {
            assert!(is_sensitive_name(OsStr::new(name)), "{name}");
        }
        for name in ["env", ".envrc", "keys.txt", "main.rs", "stderr.log"] {
            assert!(!is_sensitive_name(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn files_must_be_inside_a_root_before_and_after_resolving() {
        let temp = tempfile::tempdir().unwrap();
        let base = real(&temp);
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "x").unwrap();
        symlink(outside.join("secret.txt"), root.join("src/escape")).unwrap();
        symlink(root.join("src"), outside.join("into-root")).unwrap();
        let roots = Roots::new(vec![root.clone()]);
        let captures = Roots::new(vec![base.join("state/captures")]);

        assert!(
            allow_file(&root.join("src/main.rs"), &roots, &captures).is_ok(),
            "missing file inside"
        );
        assert_eq!(
            allow_file(&root.join("src/escape"), &roots, &captures).unwrap_err(),
            Denied::OutsideRoots
        );
        assert_eq!(
            allow_file(&outside.join("into-root/x"), &roots, &captures).unwrap_err(),
            Denied::OutsideRoots
        );
        assert_eq!(
            allow_file(&root.join("../outside/secret.txt"), &roots, &captures).unwrap_err(),
            Denied::OutsideRoots
        );
        assert_eq!(
            allow_file(Path::new("src/main.rs"), &roots, &captures).unwrap_err(),
            Denied::NotAbsolute
        );
        assert_eq!(
            allow_file(&root.join("src/.env"), &roots, &captures).unwrap_err(),
            Denied::Sensitive
        );
        assert_eq!(
            allow_file(&root.join("credentials/a.txt"), &roots, &captures).unwrap_err(),
            Denied::Sensitive
        );
        symlink(root.join("src/.env"), root.join("src/innocent")).unwrap();
        assert_eq!(
            allow_file(&root.join("src/innocent"), &roots, &captures).unwrap_err(),
            Denied::Sensitive
        );
        // `..` after a missing name must not skip the symlinks it climbs back to.
        symlink(&outside, root.join("out")).unwrap();
        let sneaky = root.join("missing/../out/secret.txt");
        assert_eq!(
            allow_file(&sneaky, &roots, &captures).unwrap_err(),
            Denied::OutsideRoots
        );
        std::fs::create_dir_all(root.join("credentials")).unwrap();
        symlink(root.join("credentials"), root.join("creds-link")).unwrap();
        let hidden = root.join("missing/../creds-link/token");
        assert_eq!(
            allow_file(&hidden, &roots, &captures).unwrap_err(),
            Denied::Sensitive
        );
    }

    #[test]
    fn a_root_that_is_a_symlink_holds_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let base = real(&temp);
        std::fs::create_dir(base.join("data")).unwrap();
        symlink(base.join("data"), base.join("project")).unwrap();
        let roots = Roots::new(vec![base.join("project")]);
        let captures = Roots::new(vec![base.join("c")]);
        let allowed = allow_file(&base.join("project/a.txt"), &roots, &captures).unwrap();
        assert_eq!(allowed.resolved, base.join("data/a.txt"));
    }

    #[test]
    fn captures_follow_their_recorded_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let base = real(&temp);
        let root = base.join("project");
        let other = base.join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let captures = base.join("state/captures");
        let mine = captures.join("1");
        let theirs = captures.join("2");
        for (dir, cwd) in [(&mine, &root), (&theirs, &other)] {
            std::fs::create_dir_all(dir).unwrap();
            let metadata = serde_json::json!({"cwd": cwd});
            std::fs::write(dir.join("metadata.json"), metadata.to_string()).unwrap();
            std::fs::write(dir.join("stderr.log"), "e").unwrap();
        }
        let roots = Roots::new(vec![root.clone()]);
        let captures = Roots::new(vec![captures]);
        assert!(allow_file(&mine.join("stderr.log"), &roots, &captures).is_ok());
        assert_eq!(
            allow_file(&theirs.join("stderr.log"), &roots, &captures).unwrap_err(),
            Denied::OutsideRoots
        );
        // A link from one capture into another does not borrow its permission.
        symlink(theirs.join("stderr.log"), mine.join("peek")).unwrap();
        assert_eq!(
            allow_file(&mine.join("peek"), &roots, &captures).unwrap_err(),
            Denied::OutsideRoots
        );
        std::fs::write(base.join("state/captures/loose.log"), "x").unwrap();
        let loose = base.join("state/captures/loose.log");
        assert_eq!(
            allow_file(&loose, &roots, &captures).unwrap_err(),
            Denied::OutsideRoots
        );
    }

    #[test]
    fn scopes_must_be_inside_a_root() {
        let temp = tempfile::tempdir().unwrap();
        let base = real(&temp);
        std::fs::create_dir_all(base.join("root/sub")).unwrap();
        let roots = Roots::new(vec![base.join("root")]);
        assert_eq!(
            allow_scope(&base.join("root/sub"), &roots).unwrap(),
            base.join("root/sub")
        );
        assert_eq!(
            allow_scope(&base, &roots).unwrap_err(),
            Denied::OutsideRoots
        );
    }
}
