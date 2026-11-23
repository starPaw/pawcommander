use super::launch::{report_startup, StartupError};
use std::{
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

#[derive(Debug)]
pub(crate) enum DiffLaunchError {
    ProgramPath,
    ProgramMetadata(io::Error),
    ProgramNotExecutable,
    RelativeInput { right: bool, path: PathBuf },
    Input { right: bool, source: io::Error },
    Startup(StartupError),
    WorkerStart(io::Error),
    WorkerStopped,
}

pub(super) fn detect() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut candidates = Vec::new();
        if let Some(developer) = std::env::var_os("DEVELOPER_DIR").map(PathBuf::from) {
            candidates.push(developer.join("usr/bin/opendiff"));
            candidates.push(developer.join("Contents/Developer/usr/bin/opendiff"));
        }
        candidates.push(PathBuf::from(
            "/Applications/Xcode.app/Contents/Developer/usr/bin/opendiff",
        ));
        if let Some(home) = dirs::home_dir() {
            candidates
                .push(home.join("Applications/Xcode.app/Contents/Developer/usr/bin/opendiff"));
        }
        // /usr/bin/opendiff can be present as a shim on Macs without Xcode.
        candidates
            .into_iter()
            .find(|path| path.is_absolute() && super::executable(path))
    }
    #[cfg(not(target_os = "macos"))]
    None
}

pub(crate) fn launch_diff(
    tool: &Path,
    files: &[PathBuf; 2],
    report: impl FnOnce(Result<(), DiffLaunchError>),
) {
    match spawn(tool, files) {
        Ok(child) => report_startup(child, |result| {
            report(result.map_err(DiffLaunchError::Startup))
        }),
        Err(error) => report(Err(error)),
    }
}

fn spawn(tool: &Path, files: &[PathBuf; 2]) -> Result<Child, DiffLaunchError> {
    if !tool.is_absolute() {
        return Err(DiffLaunchError::ProgramPath);
    }
    let metadata = tool.metadata().map_err(DiffLaunchError::ProgramMetadata)?;
    if !super::executable_metadata(&metadata) {
        return Err(DiffLaunchError::ProgramNotExecutable);
    }
    for (index, path) in files.iter().enumerate() {
        let right = index == 1;
        if !path.is_absolute() {
            return Err(DiffLaunchError::RelativeInput {
                right,
                path: path.clone(),
            });
        }
        crate::fs_ops::open_regular_source(path)
            .map_err(|source| DiffLaunchError::Input { right, source })?;
    }
    Command::new(tool)
        .args(files)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| DiffLaunchError::Startup(StartupError::Spawn(error)))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::file_error::{AccessAction, AccessCause, AccessError};

    #[test]
    fn external_diff_preflight_preserves_program_errno_and_the_affected_input() {
        let root = tempfile::tempdir().unwrap();
        let files = [
            root.path().join("left input"),
            root.path().join("right input"),
        ];
        let tool = root.path().join("configured diff tool");
        assert!(matches!(
            spawn(&tool, &files),
            Err(DiffLaunchError::ProgramMetadata(error)) if error.raw_os_error() == Some(libc::ENOENT)
        ));
        std::fs::write(&tool, b"fixture").unwrap();
        assert!(matches!(
            spawn(&tool, &files),
            Err(DiffLaunchError::ProgramNotExecutable)
        ));

        std::fs::write(&files[0], b"original left").unwrap();
        let DiffLaunchError::Input {
            right: true,
            source,
        } = spawn(Path::new("/bin/sh"), &files).unwrap_err()
        else {
            panic!("expected the missing right input");
        };
        let cause = source
            .get_ref()
            .unwrap()
            .downcast_ref::<AccessError>()
            .unwrap();
        assert_eq!(cause.location.path(), files[1]);
        assert!(matches!(cause.action, AccessAction::OpenFile));
        assert!(matches!(&cause.cause,
            AccessCause::Local(error) if error.raw_os_error() == Some(libc::ENOENT)
        ));
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"original left");
    }
}
