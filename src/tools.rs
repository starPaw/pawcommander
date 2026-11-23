//! Locate optional tools and hand off explicit application launches.
#[cfg(target_os = "macos")]
pub(crate) mod custom_action;
mod diff;
pub(crate) mod launch;
#[cfg(target_os = "macos")]
pub(crate) mod system_open;

use std::path::{Path, PathBuf};

pub(crate) use diff::{launch_diff, DiffLaunchError};

#[derive(Default)]
pub struct Tools {
    pub locate: Option<PathBuf>,
    pub zed: Option<PathBuf>,
    pub diff: Option<PathBuf>,
}

impl Tools {
    pub fn detect() -> Self {
        Self {
            locate: find(&["locate", "plocate"], []),
            zed: zed_cli(),
            diff: diff::detect(),
        }
    }

    pub(crate) fn diff_tool(&self, configured: &str) -> Option<PathBuf> {
        let configured = configured.trim();
        if configured.is_empty() {
            return self.diff.clone();
        }
        let path = PathBuf::from(configured);
        path.is_absolute().then_some(path)
    }
}

pub fn zed_cli() -> Option<PathBuf> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let mut candidates = Vec::new();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let candidates = Vec::new();
    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from("/Applications/Zed.app/Contents/MacOS/cli"));
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join("Applications/Zed.app/Contents/MacOS/cli"));
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/zed.app/bin/zed"));
    }
    find(&["zed", "zeditor"], candidates)
}

fn find(names: &[&str], extra: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    let mut candidates: Vec<_> = extra.into_iter().collect();
    let mut directories = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path).filter(|p| p.is_absolute()));
    }
    // Finder-launched apps may inherit only the system PATH.
    #[cfg(unix)]
    directories.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);
    for directory in directories {
        for name in names {
            #[cfg(windows)]
            candidates.push(directory.join(format!("{name}.exe")));
            #[cfg(not(windows))]
            candidates.push(directory.join(name));
        }
    }
    candidates.into_iter().find(|path| executable(path))
}

fn executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| executable_metadata(&metadata))
}

fn executable_metadata(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_rejects_directories_missing_and_non_executable_files() {
        let root = tempfile::tempdir().unwrap();
        assert!(!executable(root.path()));
        assert!(!executable(&root.path().join("missing")));
        let file = root.path().join("tool");
        std::fs::write(&file, b"test").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!executable(&file));
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(executable(&file));
    }
}
