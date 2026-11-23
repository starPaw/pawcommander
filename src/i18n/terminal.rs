use crate::i18n::text;
use crate::terminal::{
    embedded::SessionError, OpenError, OpenRequest, PtySpawnError, SshCommandError,
};
use crate::tr_format;
use crate::vfs::Location;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

pub(crate) fn folder_label(folder: &Location) -> String {
    let label = match folder {
        Location::Sftp { endpoint, path } => format!("{} · {}", endpoint.label(), path.display()),
        _ => folder.display(),
    };
    label.escape_debug().to_string()
}

pub(crate) fn open_request(request: OpenRequest) -> &'static str {
    match request {
        OpenRequest::Folder => text!("Запрос открытия передан Terminal."),
        OpenRequest::Ssh => text!("Запрос SSH-сеанса передан Terminal."),
    }
}

pub(crate) fn open_error(folder: &Location, error: &OpenError) -> String {
    let details = match error {
        OpenError::ResolveFolder(error) => {
            tr_format!(
                "Could not resolve the terminal folder. {error}",
                error = error
            )
        }
        OpenError::ReadFolder(error) => {
            tr_format!(
                "Could not access the terminal folder. {error}",
                error = error
            )
        }
        OpenError::NotDirectory => text!("Выберите папку для открытия терминала.").into(),
        OpenError::Ssh(error) => ssh_error(error),
        OpenError::Launcher(error) => {
            tr_format!(
                "Could not prepare the SSH launch document. {error}",
                error = error
            )
        }
        OpenError::StartOpen(error) => {
            tr_format!("The macOS open command failed. {error}", error = error)
        }
        OpenError::Rejected { status, stderr } => {
            let details = String::from_utf8_lossy(stderr);
            tr_format!(
                "macOS could not hand off the request ({status}). {details}",
                status = status,
                details = details.trim()
            )
            .trim_end()
            .into()
        }
        OpenError::Archive => super::archive_read_only().into(),
        OpenError::StartWorker(error) => worker_start_error(error),
        OpenError::WorkerStopped => text!("Запуск приложения прервался.").into(),
    };
    tr_format!(
        "Could not open Terminal in “{folder}”. {details}",
        folder = folder_label(folder),
        details = details
    )
}

pub(crate) fn embedded_error(folder: &Location, error: &SessionError) -> String {
    let details = match error {
        SessionError::Control(error) => {
            tr_format!(
                "Could not prepare the terminal controls. {error}",
                error = error
            )
        }
        SessionError::StartWorker(error) => worker_start_error(error),
        SessionError::Ssh(error) => ssh_error(error),
        SessionError::Archive => super::archive_read_only().into(),
        SessionError::Spawn(PtySpawnError::Setup(error)) => {
            tr_format!(
                "Could not prepare the pseudoterminal. {error}",
                error = error
            )
        }
        SessionError::Spawn(PtySpawnError::Process { program, source }) => tr_format!(
            "Could not start “{program}”. {error}",
            program = program.display(),
            error = source
        ),
        SessionError::Communication(error) => {
            tr_format!(
                "Could not communicate with the terminal process. {error}",
                error = error
            )
        }
        SessionError::WorkerStopped => text!("Сеанс терминала прервался.").into(),
    };
    tr_format!(
        "Embedded terminal in “{folder}”. {details}",
        folder = folder_label(folder),
        details = details
    )
}

fn worker_start_error(error: &io::Error) -> String {
    tr_format!(
        "Could not start the terminal worker. {error}",
        error = error
    )
}

fn ssh_error(error: &SshCommandError) -> String {
    match error {
        SshCommandError::ProxyJump => text!(
            "Для этого подключения нужен маршрут ProxyJump. Откройте SSH-алиас в Terminal или Zed."
        )
        .into(),
        SshCommandError::Host => text!("Некорректный сервер для SSH-сеанса.").into(),
        SshCommandError::UserOrPort => {
            text!("Проверьте пользователя и порт для SSH-сеанса.").into()
        }
        SshCommandError::Folder => text!("Для SSH-сеанса нужен абсолютный путь UTF-8.").into(),
        SshCommandError::ResolveKey { path, source } => tr_format!(
            "Could not resolve SSH key path {path}. {error}",
            path = path.display(),
            error = source
        ),
        SshCommandError::NulKey => text!("Путь к ключу SSH содержит NUL.").into(),
    }
}

pub(crate) fn process_exit(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        tr_format!("The terminal process exited with code {code}.", code = code)
    } else if let Some(signal) = status.signal() {
        tr_format!(
            "The terminal process was terminated by signal {signal}.",
            signal = signal
        )
    } else {
        text!("Процесс терминала завершён.").into()
    }
}
