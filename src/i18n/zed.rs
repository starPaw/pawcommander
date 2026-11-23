use super::text;
use crate::tr_format;
use crate::vfs::Location;
use crate::zed::{OpenError, OpenRequest};

pub(crate) fn open_request(request: OpenRequest) -> &'static str {
    match request {
        OpenRequest::Local => text!("Запрос открытия передан Zed."),
        OpenRequest::Ssh => text!(
            "Запрос открытия передан Zed. Подключение и SSH-аутентификация выполняются в Zed."
        ),
    }
}

pub(crate) fn open_error(path: &Location, error: &OpenError) -> String {
    let details = match error {
        OpenError::Archive => super::archive_read_only().into(),
        OpenError::ResolveLocal(error) => error.to_string(),
        OpenError::ProxyJump => text!(
            "Для этого подключения нужен маршрут ProxyJump. Откройте SSH-алиас в Terminal или Zed."
        )
        .into(),
        OpenError::PathEncoding => text!("Путь SFTP должен быть UTF-8").into(),
        OpenError::RemotePath => text!("Для Zed нужен абсолютный POSIX-путь SFTP").into(),
        OpenError::Host(source) => {
            let label = text!("Некорректный сервер SSH для Zed");
            if cfg!(target_os = "macos") && let Some(source) = source {
                format!("{label}. {source}")
            } else {
                label.into()
            }
        }
        OpenError::UserOrPort => text!("Укажите пользователя и порт SSH 1–65535").into(),
        OpenError::User => text!("Некорректный пользователь SSH для Zed").into(),
        OpenError::Port => text!("Некорректный порт SSH для Zed").into(),
        OpenError::BaseUrl(error) => error.to_string(),
        OpenError::Address => text!("Некорректный адрес SSH для Zed").into(),
        OpenError::CliMissing => text!("Zed не найден. Установите Zed; при нестандартной установке выполните в Zed команду «cli: install cli binary» и обновите проверку в настройках PawCommander.").into(),
        OpenError::ReadText(error) => tr_format!(
            "Could not check the SFTP text file: {e}",
            e = super::file_error(error)
        ),
        OpenError::NotText => text!("SFTP-файл не распознан как текстовый. Используйте «Открыть в Zed» в контекстном меню для явного открытия или скопируйте файл локально.").into(),
        OpenError::Launch { program, source } => {
            #[cfg(target_os = "macos")]
            {
                tr_format!(
                    "Could not start Zed at “{program}”. {error}",
                    program = program.display().to_string().escape_debug(),
                    error = source
                )
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = program;
                tr_format!("Could not start Zed: {e}", e = source)
            }
        }
        #[cfg(target_os = "macos")]
        OpenError::Rejected { status, stderr } => rejection(*status, stderr),
        #[cfg(not(target_os = "macos"))]
        OpenError::Rejected { status } => tr_format!(
            "Zed rejected the open request ({status}). Check the Zed installation and its SSH connection.",
            status = status
        ),
        #[cfg(target_os = "macos")]
        OpenError::WorkerStart(error) => tr_format!(
            "Could not start the Zed worker. {error}", error = error
        ),
        #[cfg(target_os = "macos")]
        OpenError::WorkerStopped => text!("Запуск приложения прервался.").into(),
    };
    #[cfg(target_os = "macos")]
    {
        tr_format!(
            "Could not open “{path}” in Zed. {details}",
            path = super::display::launch_location(path),
            details = details
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        details
    }
}

#[cfg(target_os = "macos")]
fn rejection(
    status: std::process::ExitStatus,
    stderr: &std::io::Result<crate::zed::CliStderr>,
) -> String {
    let mut message = tr_format!("Zed rejected the open request ({status}).", status = status);
    match stderr {
        Ok(stderr) => {
            let output = String::from_utf8_lossy(&stderr.bytes);
            if !output.trim().is_empty() {
                message.push('\n');
                message.push_str(output.trim());
            }
            if stderr.truncated {
                message.push('\n');
                message.push_str(text!("Часть диагностического вывода Zed скрыта."));
            }
        }
        Err(error) => {
            message.push('\n');
            message.push_str(&tr_format!(
                "Could not capture Zed's diagnostic output. {error}",
                error = error
            ));
        }
    }
    message
}
