use super::Location;
use crate::vfs::Endpoint;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

const IDENTITY_PATH: &str = "PAWCOMMANDER_TERMINAL_IDENTITY_FILE";
const REMOTE_SHELL: &str = r#"folder=$(printf %b "$1.") || exit 1; folder=${folder%.}; cd "$folder" || exit 1; exec "${SHELL:-/bin/sh}" -i"#;

pub(crate) enum OpenRequest {
    Folder,
    Ssh,
}

#[derive(Debug)]
pub(crate) enum OpenError {
    ResolveFolder(io::Error),
    ReadFolder(io::Error),
    NotDirectory,
    Ssh(SshCommandError),
    Launcher(io::Error),
    StartOpen(io::Error),
    Rejected { status: ExitStatus, stderr: Vec<u8> },
    Archive,
    StartWorker(io::Error),
    WorkerStopped,
}

#[derive(Debug)]
pub(crate) enum SshCommandError {
    ProxyJump,
    Host,
    UserOrPort,
    Folder,
    ResolveKey { path: PathBuf, source: io::Error },
    NulKey,
}

pub(crate) fn open(folder: &Location) -> Result<OpenRequest, OpenError> {
    match folder {
        Location::Local(path) => {
            let path = std::path::absolute(path).map_err(OpenError::ResolveFolder)?;
            if !std::fs::metadata(&path)
                .map_err(OpenError::ReadFolder)?
                .is_dir()
            {
                return Err(OpenError::NotDirectory);
            }
            open_document(document_command(&path))?;
            Ok(OpenRequest::Folder)
        }
        Location::Sftp { endpoint, path } => {
            let ssh = ssh_command(endpoint, path).map_err(OpenError::Ssh)?;
            let mut script = create_launcher(&ssh).map_err(OpenError::Launcher)?;
            open_document(document_command(script.path()))?;
            // Terminal reads the document asynchronously and unlinks it on start.
            script.disable_cleanup(true);
            Ok(OpenRequest::Ssh)
        }
        Location::Archive { .. } => Err(OpenError::Archive),
    }
}

fn document_command(path: &Path) -> Command {
    let mut command = Command::new("/usr/bin/open");
    command.args(["-b", "com.apple.Terminal", "--"]).arg(path);
    command
}

fn open_document(mut command: Command) -> Result<(), OpenError> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(OpenError::StartOpen)?;
    if output.status.success() {
        return Ok(());
    }
    Err(OpenError::Rejected {
        status: output.status,
        stderr: output.stderr,
    })
}

pub(super) fn ssh_command(endpoint: &Endpoint, path: &Path) -> Result<Command, SshCommandError> {
    if endpoint.jump.is_some() {
        return Err(SshCommandError::ProxyJump);
    }
    let host = endpoint.host_name();
    if host.is_empty()
        || host.starts_with('-')
        || !host
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_' | ':' | '%'))
    {
        return Err(SshCommandError::Host);
    }
    if endpoint.port == 0
        || endpoint.user.is_empty()
        || endpoint.user.starts_with('-')
        || !endpoint
            .user
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@' | '+'))
    {
        return Err(SshCommandError::UserOrPort);
    }
    let remote = remote_command(path)?;
    let mut command = Command::new("/usr/bin/ssh");
    command.args([
        "-tt",
        "-p",
        &endpoint.port.to_string(),
        "-l",
        &endpoint.user,
    ]);
    if let Some(key) = &endpoint.key {
        let key = std::path::absolute(key).map_err(|source| SshCommandError::ResolveKey {
            path: key.clone(),
            source,
        })?;
        let bytes = key.as_os_str().as_bytes();
        if bytes.contains(&0) {
            return Err(SshCommandError::NulKey);
        }
        if bytes.contains(&b'%') || bytes.windows(2).any(|pair| pair == b"${") {
            // OpenSSH expands IdentityFile tokens. A variable value is inserted as data.
            command
                .env(IDENTITY_PATH, key)
                .args(["-o", "IdentityFile=${PAWCOMMANDER_TERMINAL_IDENTITY_FILE}"]);
        } else {
            command.arg("-i").arg(key);
        }
    }
    command.args(["--", host, &remote]);
    Ok(command)
}

fn remote_command(path: &Path) -> Result<String, SshCommandError> {
    let path = path
        .to_str()
        .filter(|path| path.starts_with('/') && !path.contains('\0'))
        .ok_or(SshCommandError::Folder)?;
    // ssh sends a command string to the account's shell. Encode the folder
    // as a data argument and decode it inside the fixed POSIX program.
    let mut command = format!("/bin/sh -c '{REMOTE_SHELL}' pawcommander '");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'-' | b'_') {
            command.push(char::from(byte));
        } else {
            write!(command, "\\0{byte:03o}").expect("write to String");
        }
    }
    command.push('\'');
    Ok(command)
}

fn create_launcher(command: &Command) -> io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("PawCommander-SSH-")
        .suffix(".command")
        .tempfile()?;
    file.write_all(b"#!/bin/sh\n/bin/rm -f -- \"$0\"\n")?;
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            file.write_all(b"export ")?;
            file.write_all(name.as_bytes())?;
            file.write_all(b"=")?;
            shell_word(&mut file, value)?;
            file.write_all(b"\n")?;
        }
    }
    file.write_all(b"exec")?;
    for arg in std::iter::once(command.get_program()).chain(command.get_args()) {
        file.write_all(b" ")?;
        shell_word(&mut file, arg)?;
    }
    file.write_all(b"\n")?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    Ok(file)
}

fn shell_word(file: &mut impl Write, argument: &OsStr) -> io::Result<()> {
    file.write_all(b"'")?;
    for (index, part) in argument.as_bytes().split(|byte| *byte == b'\'').enumerate() {
        if index > 0 {
            file.write_all(b"'\\''")?;
        }
        file.write_all(part)?;
    }
    file.write_all(b"'")
}

#[cfg(test)]
mod tests;
