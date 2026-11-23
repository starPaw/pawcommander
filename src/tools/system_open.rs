use std::io;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Open,
    Reveal,
    RevealWorkingCopy,
}

#[derive(Debug)]
pub(crate) enum Error {
    ResolvePath(io::Error),
    InspectWorkingCopy(io::Error),
    NotRegularWorkingCopy,
    RunCommand(io::Error),
    Rejected { status: ExitStatus, stderr: Vec<u8> },
    StartWorker(io::Error),
    WorkerStopped,
}

impl Mode {
    pub(crate) fn handoff(self, path: &Path) -> Result<(), Error> {
        let path = std::path::absolute(path).map_err(Error::ResolvePath)?;
        if matches!(self, Self::RevealWorkingCopy) {
            let metadata = std::fs::metadata(&path).map_err(Error::InspectWorkingCopy)?;
            if !metadata.is_file() {
                return Err(Error::NotRegularWorkingCopy);
            }
        }
        run_command(self.command(&path))
    }

    fn command(self, path: &Path) -> Command {
        let mut command = Command::new("/usr/bin/open");
        if matches!(self, Self::Reveal | Self::RevealWorkingCopy) {
            command.arg("-R");
        }
        command.arg("--").arg(path);
        command
    }
}

pub(crate) fn run_command(mut command: Command) -> Result<(), Error> {
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .map_err(Error::RunCommand)?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Rejected {
        status: output.status,
        stderr: output.stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn file_open_and_reveal_pass_the_path_as_one_native_argument() {
        let path = OsString::from_vec(b"/tmp/-folder ' & % \n\xff".to_vec());
        let open = Mode::Open.command(Path::new(&path));
        assert_eq!(open.get_program(), "/usr/bin/open");
        assert_eq!(
            open.get_args().collect::<Vec<_>>(),
            [OsStr::new("--"), path.as_os_str()]
        );
        let reveal = Mode::Reveal.command(Path::new(&path));
        assert_eq!(reveal.get_program(), "/usr/bin/open");
        assert_eq!(
            reveal.get_args().collect::<Vec<_>>(),
            [OsStr::new("-R"), OsStr::new("--"), path.as_os_str()]
        );
    }

    #[test]
    fn macos_open_launch_reports_the_original_errno() {
        let fixture = tempfile::tempdir().unwrap();
        let command = Command::new(fixture.path().join("missing-open"));
        assert!(matches!(run_command(command), Err(Error::RunCommand(error))
            if error.raw_os_error() == Some(libc::ENOENT)));
    }

    #[test]
    fn revealing_a_working_copy_requires_the_requested_regular_file() {
        let fixture = tempfile::tempdir().unwrap();
        let working_copy = fixture.path().join("copy.txt");
        assert!(matches!(
            Mode::RevealWorkingCopy.handoff(&working_copy),
            Err(Error::InspectWorkingCopy(error)) if error.raw_os_error() == Some(libc::ENOENT)
        ));
        std::fs::create_dir(&working_copy).unwrap();
        assert!(matches!(
            Mode::RevealWorkingCopy.handoff(&working_copy),
            Err(Error::NotRegularWorkingCopy)
        ));
    }
}
