use super::Location;
use crate::i18n::text;
use crate::vfs::Endpoint;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};

const IDENTITY_PATH: &str = "PAWCOMMANDER_TERMINAL_IDENTITY_FILE";
const REMOTE_SHELL: &str = r#"folder=$(printf %b "$1.") || exit 1; folder=${folder%.}; cd "$folder" || exit 1; exec "${SHELL:-/bin/sh}" -i"#;

pub(crate) fn open(folder: &Location) -> Result<String, String> {
    match folder {
        Location::Local(path) => {
            let path = std::path::absolute(path).map_err(launch_error)?;
            if !std::fs::metadata(&path).map_err(launch_error)?.is_dir() {
                return Err(text("Выберите папку для открытия терминала.").into());
            }
            open_document(&path)?;
            Ok(text("Запрос открытия передан Terminal.").into())
        }
        Location::Sftp { endpoint, path } => {
            let ssh = ssh_command(endpoint, path)?;
            let mut script = create_launcher(&ssh).map_err(launch_error)?;
            open_document(script.path())?;
            // Terminal reads the document asynchronously and unlinks it on start.
            script.disable_cleanup(true);
            Ok(text("Запрос SSH-сеанса передан Terminal.").into())
        }
        Location::Archive { .. } => Err(crate::archive::readonly().into()),
    }
}

fn document_command(path: &Path) -> Command {
    let mut command = Command::new("/usr/bin/open");
    command.args(["-b", "com.apple.Terminal", "--"]).arg(path);
    command
}

fn open_document(path: &Path) -> Result<(), String> {
    let output = document_command(path)
        .stdin(Stdio::null())
        .output()
        .map_err(launch_error)?;
    if output.status.success() {
        return Ok(());
    }
    let status = output.status;
    let details = String::from_utf8_lossy(&output.stderr);
    let details = details.trim();
    Err(crate::tr_format!(
        "Terminal не принял запрос открытия ({status}). {details}",
        "Terminal did not accept the open request ({status}). {details}"
    )
    .trim_end()
    .to_owned())
}

fn launch_error(error: io::Error) -> String {
    crate::tr_format!(
        "Не удалось открыть терминал. {error}",
        "Could not open the terminal. {error}"
    )
}

pub(super) fn ssh_command(endpoint: &Endpoint, path: &Path) -> Result<Command, String> {
    if endpoint.jump.is_some() {
        return Err(text(
            "Для этого подключения нужен маршрут ProxyJump. Откройте SSH-алиас в Terminal или Zed.",
        )
        .into());
    }
    let host = endpoint.host_name();
    if host.is_empty()
        || host.starts_with('-')
        || !host
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '-' | '_' | ':' | '%'))
    {
        return Err(text("Некорректный сервер для SSH-сеанса.").into());
    }
    if endpoint.port == 0
        || endpoint.user.is_empty()
        || endpoint.user.starts_with('-')
        || !endpoint
            .user
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@' | '+'))
    {
        return Err(text("Проверьте пользователя и порт для SSH-сеанса.").into());
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
        let key = std::path::absolute(key).map_err(launch_error)?;
        let bytes = key.as_os_str().as_bytes();
        if bytes.contains(&0) {
            return Err(text("Путь к ключу SSH содержит NUL.").into());
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

fn remote_command(path: &Path) -> Result<String, String> {
    let path = path
        .to_str()
        .filter(|path| path.starts_with('/') && !path.contains('\0'))
        .ok_or_else(|| text("Для SSH-сеанса нужен абсолютный путь UTF-8.").to_owned())?;
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
