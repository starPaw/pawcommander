use std::{
    io,
    process::{Child, ExitStatus},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Debug)]
pub(crate) enum StartupError {
    Spawn(io::Error),
    Observe(io::Error),
    Exited(ExitStatus),
}

pub(super) fn report_startup(mut child: Child, report: impl FnOnce(Result<(), StartupError>)) {
    let deadline = Instant::now() + Duration::from_millis(700);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                report(if status.success() {
                    Ok(())
                } else {
                    Err(StartupError::Exited(status))
                });
                return;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(15)),
            Ok(None) => {
                // Windowed tools may stay attached. Report startup and reap them on this worker.
                report(Ok(()));
                let _ = child.wait();
                return;
            }
            Err(error) => {
                report(Err(StartupError::Observe(error)));
                let _ = child.wait();
                return;
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Action {
        name: String,
        files: Vec<PathBuf>,
    },
    Editor {
        source: crate::vfs::Location,
        file: PathBuf,
    },
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandDetails {
    pub(crate) target: Target,
    pub(crate) program: PathBuf,
    pub(crate) folder: PathBuf,
}

#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub(crate) details: CommandDetails,
    pub(crate) arguments: Vec<OsString>,
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
pub(crate) enum InvocationError {
    InspectFile(io::Error),
    NotRegularFile,
    Startup(StartupError),
    WorkerStart(io::Error),
    WorkerStopped,
}

#[cfg(target_os = "macos")]
impl Invocation {
    pub(crate) fn launch(self, report: impl FnOnce(Result<(), InvocationError>)) {
        match self.start() {
            Ok(child) => report_startup(child, |result| {
                report(result.map_err(InvocationError::Startup));
            }),
            Err(error) => report(Err(error)),
        }
    }

    fn start(self) -> Result<Child, InvocationError> {
        if let Target::Editor { file, .. } = &self.details.target {
            let metadata = std::fs::metadata(file).map_err(InvocationError::InspectFile)?;
            if !metadata.is_file() {
                return Err(InvocationError::NotRegularFile);
            }
        }
        Command::new(&self.details.program)
            .args(&self.arguments)
            .current_dir(&self.details.folder)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| InvocationError::Startup(StartupError::Spawn(error)))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::vfs::Location;
    use std::path::Path;

    fn editor(folder: &Path, file: PathBuf) -> Invocation {
        Invocation {
            details: CommandDetails {
                target: Target::Editor {
                    source: Location::Local(file.clone()),
                    file,
                },
                program: PathBuf::from("/bin/sh"),
                folder: folder.to_owned(),
            },
            arguments: vec!["-c".into(), "exit 73".into()],
        }
    }

    #[test]
    fn configured_editor_preserves_the_file_problem_and_early_process_exit() {
        let folder = tempfile::tempdir().unwrap();
        let selected = folder.path().join("selected file");
        editor(folder.path(), selected.clone()).launch(|result| {
            assert!(matches!(
                result,
                Err(InvocationError::InspectFile(error)) if error.raw_os_error() == Some(libc::ENOENT)
            ));
        });
        editor(folder.path(), folder.path().to_owned()).launch(|result| {
            assert!(matches!(result, Err(InvocationError::NotRegularFile)));
        });
        std::fs::write(&selected, b"fixture").unwrap();
        editor(folder.path(), selected.clone()).launch(|result| {
            assert!(matches!(
                result,
                Err(InvocationError::Startup(StartupError::Exited(status))) if status.code() == Some(73)
            ));
        });
        assert_eq!(std::fs::read(selected).unwrap(), b"fixture");
    }
}
