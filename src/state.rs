use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

/// The kinds of record that agentgrasp keeps, each in its own directory under the state root.
#[derive(Clone, Copy)]
pub enum Kind {
    Capture,
    Ask,
    Search,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Capture => "captures",
            Kind::Ask => "asks",
            Kind::Search => "searches",
        }
    }
}

/// `<state>/agentgrasp`, from the process environment.
pub fn root() -> Result<PathBuf> {
    root_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// The XDG base directory spec says a relative `XDG_STATE_HOME` is invalid and must be ignored.
pub fn root_from(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    let state = match xdg_state_home.map(PathBuf::from) {
        Some(dir) if dir.is_absolute() => dir,
        _ => match home.map(PathBuf::from) {
            Some(home) if home.is_absolute() => home.join(".local/state"),
            _ => bail!("neither XDG_STATE_HOME nor HOME is an absolute path"),
        },
    };
    Ok(state.join("agentgrasp"))
}

/// Creates a new, empty record directory `<root>/<kind>/<id>` and returns its absolute path.
/// The id starts with the UTC time, so ids sort by time. The directory is created with an
/// exclusive create, so two processes never get the same one and no record is ever reused.
pub fn allocate(root: &Path, kind: Kind) -> Result<PathBuf> {
    let stamp = format!("{}-{}", utc_stamp(SystemTime::now()), std::process::id());
    allocate_with(root, kind, &stamp)
}

fn allocate_with(root: &Path, kind: Kind, stamp: &str) -> Result<PathBuf> {
    let parent = root.join(kind.dir());
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    for attempt in 0..1000 {
        let id = match attempt {
            0 => stamp.to_owned(),
            n => format!("{stamp}-{n}"),
        };
        let dir = parent.join(id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot create {}", dir.display()));
            }
        }
    }
    bail!("no free record directory under {}", parent.display())
}

/// `YYYYMMDDTHHMMSS.ffffffZ`. Fixed width, so the text order is the time order.
pub fn utc_stamp(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}.{:06}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_micros()
    )
}

/// RFC 3339 UTC time with microseconds, for the times recorded in metadata.
pub fn rfc3339(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_micros()
    )
}

// Howard Hinnant's days-to-civil algorithm: day 0 is 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Writes `bytes` to `path` through a temporary file in the same directory and a rename, so a
/// reader never sees half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, bytes).with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("cannot write {}", path.display()))
}

/// Writes a finished record. It fails when `path` exists, so a finished record is never
/// overwritten.
pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn root_prefers_absolute_xdg_state_home() {
        let root = root_from(Some("/x/state".into()), Some("/home/me".into())).unwrap();
        assert_eq!(root, PathBuf::from("/x/state/agentgrasp"));
    }

    #[test]
    fn root_ignores_relative_xdg_state_home() {
        let root = root_from(Some("state".into()), Some("/home/me".into())).unwrap();
        assert_eq!(root, PathBuf::from("/home/me/.local/state/agentgrasp"));
        let root = root_from(None, Some("/home/me".into())).unwrap();
        assert_eq!(root, PathBuf::from("/home/me/.local/state/agentgrasp"));
    }

    #[test]
    fn root_needs_an_absolute_home() {
        assert!(root_from(None, None).is_err());
        assert!(root_from(None, Some("home".into())).is_err());
    }

    #[test]
    fn stamps_are_utc_and_fixed_width() {
        let time = UNIX_EPOCH + Duration::new(1_790_841_178, 282_990_000);
        assert_eq!(utc_stamp(time), "20261001T075258.282990Z");
        assert_eq!(rfc3339(time), "2026-10-01T07:52:58.282990Z");
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101T000000.000000Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(&utc_stamp(leap)[..8], "20000229");
    }

    #[test]
    fn allocations_are_distinct_and_never_reused() {
        let temp = tempfile::tempdir().unwrap();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let dir = allocate(temp.path(), Kind::Capture).unwrap();
            assert!(dir.is_dir() && dir.is_absolute());
            assert!(seen.insert(dir));
        }
    }

    #[test]
    fn allocation_skips_an_existing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let taken = temp.path().join("captures/stamp");
        std::fs::create_dir_all(&taken).unwrap();
        std::fs::write(taken.join("metadata.json"), "kept").unwrap();
        let dir = allocate_with(temp.path(), Kind::Capture, "stamp").unwrap();
        assert_eq!(dir, temp.path().join("captures/stamp-1"));
        let next = allocate_with(temp.path(), Kind::Capture, "stamp").unwrap();
        assert_eq!(next, temp.path().join("captures/stamp-2"));
        assert_eq!(
            std::fs::read_to_string(taken.join("metadata.json")).unwrap(),
            "kept"
        );
    }

    #[test]
    fn write_new_refuses_an_existing_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("record.json");
        write_new(&path, b"one").unwrap();
        assert!(write_new(&path, b"two").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
    }
}
