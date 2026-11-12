//! Locate database provenance and filesystem freshness metadata.
//!
//! Locate database formats do not expose a portable build timestamp. File
//! modification dates are therefore shown as freshness estimates.

use std::{
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_PROBE_BYTES: usize = 32 * 1024;
const READER_POLL: Duration = Duration::from_millis(5);

/// User-facing provenance and freshness strings, prepared off the UI thread.
pub(super) struct IndexInfo {
    pub(super) source: String,
    pub(super) freshness: String,
}

impl IndexInfo {
    pub(super) fn unknown() -> Self {
        Self {
            source: crate::tr_format!("Источник индекса неизвестен", "Index source unknown"),
            freshness: crate::tr_format!(
                "Дата актуальности индекса неизвестна",
                "Index freshness date unknown"
            ),
        }
    }
}

/// Inspect locate provenance without using the executable's modification
/// time. The caller should pass the remaining search budget (normally a few
/// hundred milliseconds).
pub(super) fn inspect(cli: &Path, cancelled: &AtomicBool, timeout: Duration) -> IndexInfo {
    inspect_with(cli, env::var_os("LOCATE_PATH"), cancelled, timeout)
}

fn inspect_with(
    cli: &Path,
    locate_path: Option<OsString>,
    cancelled: &AtomicBool,
    timeout: Duration,
) -> IndexInfo {
    if cancelled.load(Ordering::Relaxed) {
        return IndexInfo::unknown();
    }

    let started = Instant::now();
    let remaining = || timeout.saturating_sub(started.elapsed());
    let can_check_date = || !cancelled.load(Ordering::Relaxed) && !remaining().is_zero();

    // Apple BSD locate uses LOCATE_PATH as a replacement for its default.
    // Its system path and database path are documented; no probe is needed.
    #[cfg(target_os = "macos")]
    if cli == Path::new("/usr/bin/locate") {
        let paths = apple_database_paths(locate_path.as_deref());
        return info_for_paths(paths, can_check_date);
    }

    // plocate differs from BSD/GNU: its default remains in use when
    // LOCATE_PATH is set, then LOCATE_PATH databases are appended. Identify it
    // by its help output because `locate` is commonly a symlink to any variant.
    let plocate_default = run_capture(cli, "--help", cancelled, remaining())
        .and_then(|help| parse_plocate_default(&help));
    if let Some(default) = plocate_default {
        if cancelled.load(Ordering::Relaxed) {
            return IndexInfo::unknown();
        }
        let paths = plocate_database_paths(&default, locate_path.as_deref());
        return info_for_paths(paths, can_check_date);
    }

    // BSD, mlocate, and GNU locate statistics list their actual database
    // sources. Do not guess from LOCATE_PATH if the implementation cannot
    // report which defaults it uses.
    let Some(stats) = run_capture(cli, "-S", cancelled, remaining()) else {
        return IndexInfo::unknown();
    };
    info_for_paths(parse_database_paths(&stats), can_check_date)
}

/// Limit probe output and wait for the reader to retire before releasing the
/// inspection slot. Search results are delivered by a separate worker.
fn run_capture(
    cli: &Path,
    arg: &str,
    cancelled: &AtomicBool,
    timeout: Duration,
) -> Option<Vec<u8>> {
    if cancelled.load(Ordering::Relaxed) || timeout.is_zero() {
        return None;
    }
    let started = Instant::now();
    let mut child = Command::new(cli)
        .arg(arg)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take().expect("piped probe stdout");
    let (tx, rx) = mpsc::channel();
    let reader = thread::Builder::new().spawn(move || {
        let _ = tx.send(super::read_capped(stdout, MAX_PROBE_BYTES));
    });
    let Ok(reader) = reader else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };

    let output = (|| {
        let status = loop {
            if cancelled.load(Ordering::Relaxed) || started.elapsed() >= timeout {
                break None;
            }
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => thread::sleep(READER_POLL),
                Err(_) => break None,
            }
        };
        let Some(status) = status else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };
        if !status.success() {
            return None;
        }
        while !cancelled.load(Ordering::Relaxed) && started.elapsed() < timeout {
            let remaining = timeout.saturating_sub(started.elapsed());
            match rx.recv_timeout(remaining.min(READER_POLL)) {
                Ok(bytes) => return bytes.ok(),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
        None
    })();
    let _ = reader.join();
    output
}

fn parse_plocate_default(help: &[u8]) -> Option<PathBuf> {
    if !help.starts_with(b"Usage: plocate ") {
        return None;
    }
    help.split(|byte| *byte == b'\n')
        .find_map(|line| {
            trim_ascii(line)
                .strip_prefix(b"(default is ")?
                .strip_suffix(b")")
        })
        .filter(|path| !path.is_empty())
        .map(path_from_bytes)
}

fn parse_database_paths(output: &[u8]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in output.split(|byte| *byte == b'\n') {
        #[cfg(windows)]
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let candidate = if let Some(value) = line.strip_prefix(b"Database: ") {
            Some(value)
        } else if let Some(value) = line.strip_prefix(b"Database ") {
            // mlocate ends its heading with a colon. GNU findutils prints the
            // database format after the path on the same line.
            const FORMAT: &[u8] = b" is in the ";
            value.strip_suffix(b":").or_else(|| {
                value
                    .windows(FORMAT.len())
                    .rposition(|part| part == FORMAT)
                    .map(|index| &value[..index])
            })
        } else {
            None
        };
        if let Some(candidate) = candidate.filter(|value| !value.is_empty()) {
            paths.push(path_from_bytes(candidate));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

#[cfg(any(target_os = "macos", test))]
fn apple_database_paths(locate_path: Option<&OsStr>) -> Vec<PathBuf> {
    match locate_path.filter(|value| !value.is_empty() && *value != ":") {
        Some(value) => env::split_paths(value)
            .map(|path| {
                if path.as_os_str().is_empty() {
                    PathBuf::from("/var/db/locate.database")
                } else if path.as_os_str() == "-" {
                    PathBuf::new()
                } else {
                    path
                }
            })
            .collect(),
        None => vec![PathBuf::from("/var/db/locate.database")],
    }
}

fn plocate_database_paths(default: &Path, locate_path: Option<&OsStr>) -> Vec<PathBuf> {
    let mut paths = vec![default.to_path_buf()];
    if let Some(value) = locate_path {
        if let Some(extra) = parse_plocate_database_list(value) {
            paths.extend(extra);
        } else {
            // An invalid trailing escape makes plocate reject this environment
            // value. Preserve the known default but mark the override unknown.
            paths.push(PathBuf::new());
        }
    }
    paths
}

fn parse_plocate_database_list(value: &OsStr) -> Option<Vec<PathBuf>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let bytes = value.as_bytes();
        let mut paths = Vec::new();
        let mut path = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\\' => {
                    index += 1;
                    let next = *bytes.get(index)?;
                    path.push(next);
                }
                b':' => {
                    paths.push(PathBuf::from(OsString::from_vec(std::mem::take(&mut path))));
                }
                byte => path.push(byte),
            }
            index += 1;
        }
        paths.push(PathBuf::from(OsString::from_vec(path)));
        Some(paths)
    }
    #[cfg(not(unix))]
    {
        let value = value.to_string_lossy();
        let mut paths = Vec::new();
        let mut path = String::new();
        let mut chars = value.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => path.push(chars.next()?),
                ':' => paths.push(PathBuf::from(std::mem::take(&mut path))),
                other => path.push(other),
            }
        }
        paths.push(PathBuf::from(path));
        Some(paths)
    }
}

fn info_for_paths(paths: Vec<PathBuf>, can_check_date: impl Fn() -> bool) -> IndexInfo {
    if paths.is_empty() {
        return IndexInfo::unknown();
    }
    let source = paths
        .iter()
        .map(|path| {
            if path.as_os_str().is_empty() {
                crate::tr_format!("база не определена", "database unknown")
            } else {
                path.display().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    IndexInfo {
        source: crate::tr_format!("База индекса: {source}", "Index database: {source}"),
        freshness: freshness_for_paths(&paths, can_check_date),
    }
}

fn freshness_for_paths(paths: &[PathBuf], can_check_date: impl Fn() -> bool) -> String {
    if paths.is_empty() {
        return crate::tr_format!(
            "Дата актуальности индекса неизвестна",
            "Index freshness date unknown"
        );
    }
    let mut pieces = Vec::with_capacity(paths.len());
    for path in paths {
        if path.as_os_str().is_empty() {
            pieces.push(crate::tr_format!(
                "база данных не определена",
                "database path unknown"
            ));
            continue;
        }
        let metadata = can_check_date()
            .then(|| path.metadata().ok().filter(|metadata| metadata.is_file()))
            .flatten();
        let description = match metadata.and_then(|metadata| metadata.modified().ok()) {
            Some(modified) => {
                let date = chrono::DateTime::<chrono::Local>::from(modified)
                    .format("%Y-%m-%d %H:%M:%S %Z")
                    .to_string();
                crate::tr_format!(
                    "{} — файл базы изменён {date} (оценка актуальности)",
                    "{} — database file modified {date} (freshness estimate)",
                    path.display()
                )
            }
            None => crate::tr_format!(
                "{} — дата изменения неизвестна",
                "{} — modification date unknown",
                path.display()
            ),
        };
        pieces.push(description);
    }
    pieces.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File, FileTimes},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn recognizes_plocate_and_parses_compiled_default() {
        let help = "Usage: plocate [OPTION]... PATTERN...\n  -d, --database DBPATH search files\n                         (default is /var/lib/plocate/plocate.db)\n";
        assert_eq!(
            parse_plocate_default(help.as_bytes()),
            Some(PathBuf::from("/var/lib/plocate/plocate.db"))
        );
        assert_eq!(parse_plocate_default(b"Usage: locate [OPTION]..."), None);
        assert_eq!(parse_plocate_default(b"Usage: plocate ..."), None);
    }

    #[test]
    fn plocate_keeps_default_and_appends_override_databases() {
        let default = Path::new("/default.db");
        assert_eq!(
            plocate_database_paths(default, None),
            vec![PathBuf::from("/default.db")]
        );
        assert_eq!(
            plocate_database_paths(default, Some(OsStr::new("/extra.db:/other.db"))),
            vec![
                PathBuf::from("/default.db"),
                PathBuf::from("/extra.db"),
                PathBuf::from("/other.db")
            ]
        );
    }

    #[test]
    fn plocate_database_list_unescapes_colons_and_whitespace() {
        assert_eq!(
            parse_plocate_database_list(OsStr::new("/db/with\\:colon.db:/db/with space.db")),
            Some(vec![
                PathBuf::from("/db/with:colon.db"),
                PathBuf::from("/db/with space.db")
            ])
        );
        assert_eq!(
            parse_plocate_database_list(OsStr::new("/db/trailing\\")),
            None
        );
    }

    #[test]
    fn apple_bsd_environment_replaces_default() {
        assert_eq!(
            apple_database_paths(None),
            vec![PathBuf::from("/var/db/locate.database")]
        );
        assert_eq!(
            apple_database_paths(Some(OsStr::new(":"))),
            apple_database_paths(None)
        );
        assert_eq!(
            apple_database_paths(Some(OsStr::new("/custom/database"))),
            vec![PathBuf::from("/custom/database")]
        );
    }

    #[test]
    fn parses_bsd_and_gnu_statistics_only_when_the_paths_are_reported() {
        assert_eq!(
            parse_database_paths(b"Database: /var/db/locate.database\nCompression: front"),
            vec![PathBuf::from("/var/db/locate.database")]
        );
        assert_eq!(
            parse_database_paths(b"Database: /tmp/with-space.db \n"),
            vec![PathBuf::from("/tmp/with-space.db ")]
        );
        assert_eq!(
            parse_database_paths(
                b"Database /var/lib/mlocate/mlocate.db:\n\t123 entries\nDatabase /tmp/extra.db is in the GNU LOCATE02 format."
            ),
            vec![
                PathBuf::from("/tmp/extra.db"),
                PathBuf::from("/var/lib/mlocate/mlocate.db")
            ]
        );
        assert!(parse_database_paths(b"no database statistics available").is_empty());
    }

    #[test]
    fn missing_database_and_failed_statistics_are_unknown() {
        let missing = Path::new("/pawcommander-test-index-that-does-not-exist.db");
        let freshness = freshness_for_paths(&[missing.to_path_buf()], || true);
        assert!(
            freshness.contains("modification date unknown")
                || freshness.contains("дата изменения неизвестна")
        );
        let missing_default =
            PathBuf::from("/pawcommander-test-plocate-default-that-does-not-exist.db");
        let plocate_paths = plocate_database_paths(&missing_default, None);
        assert_eq!(plocate_paths, vec![missing_default]);
        let plocate_freshness = freshness_for_paths(&plocate_paths, || true);
        assert!(
            plocate_freshness.contains("modification date unknown")
                || plocate_freshness.contains("дата изменения неизвестна")
        );

        #[cfg(unix)]
        {
            let root = temp_dir("failed-stats");
            fs::create_dir(&root).unwrap();
            let cli = root.join("locate");
            fs::write(&cli, "#!/bin/sh\nexit 1\n").unwrap();
            let mut permissions = fs::metadata(&cli).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&cli, permissions).unwrap();
            let cancelled = AtomicBool::new(false);
            let info = inspect_with(&cli, None, &cancelled, Duration::from_millis(300));
            assert!(info.source.contains("unknown") || info.source.contains("неизвестен"));
            assert!(info.freshness.contains("unknown") || info.freshness.contains("неизвестна"));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn freshness_shows_each_index_separately_when_ages_differ() {
        let root = temp_dir("mixed-indexes");
        fs::create_dir(&root).unwrap();
        let old = root.join("old.db");
        let new = root.join("new.db");
        fs::write(&old, b"old").unwrap();
        fs::write(&new, b"new").unwrap();
        let old_time = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        File::open(&old)
            .unwrap()
            .set_times(FileTimes::new().set_modified(old_time))
            .unwrap();

        let freshness = freshness_for_paths(&[old.clone(), new.clone()], || true);
        let old_mtime = old.metadata().unwrap().modified().unwrap();
        let new_mtime = new.metadata().unwrap().modified().unwrap();
        assert!(old_mtime < new_mtime);
        for modified in [old_mtime, new_mtime] {
            let date = chrono::DateTime::<chrono::Local>::from(modified)
                .format("%Y-%m-%d %H:%M:%S %Z")
                .to_string();
            assert!(freshness.contains(&date));
        }
        assert!(freshness.contains("old.db"));
        assert!(freshness.contains("new.db"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fake_plocate_help_avoids_statistics_and_keeps_override_sources() {
        let root = temp_dir("plocate-probe");
        fs::create_dir(&root).unwrap();
        let default = root.join("default.db");
        let extra = root.join("extra: db.db");
        fs::write(&default, b"default").unwrap();
        fs::write(&extra, b"extra").unwrap();
        let cli = root.join("plocate");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = --help ]; then printf 'Usage: plocate [OPTION]... PATTERN...\\n  -d, --database DBPATH\\n                         (default is {})\\n'; else exit 42; fi\n",
            default.display()
        );
        fs::write(&cli, script).unwrap();
        let mut permissions = fs::metadata(&cli).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cli, permissions).unwrap();
        let extra_escaped = extra.to_string_lossy().replace(':', "\\:");
        let cancelled = AtomicBool::new(false);
        let info = inspect_with(
            &cli,
            Some(OsString::from(extra_escaped)),
            &cancelled,
            Duration::from_millis(500),
        );
        assert!(info.source.contains("default.db"));
        assert!(info.source.contains("extra: db.db"));
        assert!(info.freshness.contains("default.db"));
        assert!(info.freshness.contains("extra: db.db"));
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_dir(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "pawcommander-locate-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
