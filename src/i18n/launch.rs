use super::text;
use crate::tools::{launch::StartupError, DiffLaunchError};
use crate::tr_format;
use std::path::Path;

#[cfg(target_os = "macos")]
use super::display;
#[cfg(target_os = "macos")]
use crate::tools::{
    custom_action::{ArgumentError, TemplateError},
    launch::{CommandDetails, InvocationError, Target},
    system_open::{self, Mode},
};
#[cfg(target_os = "macos")]
use crate::vfs::Location;
pub(crate) fn startup_error(error: &StartupError) -> String {
    match error {
        StartupError::Spawn(error) | StartupError::Observe(error) => {
            tr_format!("Could not launch the external tool: {error}", error = error)
        }
        StartupError::Exited(status) => tr_format!(
            "The external tool exited with an error ({status})",
            status = status
        ),
    }
}

pub(crate) fn diff_error(tool: Option<&Path>, failure: &DiffLaunchError) -> String {
    let message = match failure {
        DiffLaunchError::ProgramPath if cfg!(target_os = "macos") => {
            text!("Укажите абсолютный путь к исполняемому файлу").into()
        }
        DiffLaunchError::ProgramMetadata(error) if cfg!(target_os = "macos") => tr_format!(
            "Could not inspect the external diff tool. {error}",
            error = error
        ),
        DiffLaunchError::ProgramNotExecutable if cfg!(target_os = "macos") => {
            text!("Для сравнения нужен исполняемый файл.").into()
        }
        DiffLaunchError::ProgramPath
        | DiffLaunchError::ProgramMetadata(_)
        | DiffLaunchError::ProgramNotExecutable => {
            text!("Внешний инструмент сравнения недоступен").into()
        }
        DiffLaunchError::RelativeInput { right, path } if cfg!(target_os = "macos") => {
            let error = text!("Внешнему инструменту нужны два локальных файла");
            format!("{} · {error} · {path:?}", diff_pane(*right))
        }
        DiffLaunchError::RelativeInput { .. } => {
            text!("Внешнему инструменту нужны два локальных файла").into()
        }
        DiffLaunchError::Input { right, source } => {
            let error = super::access_io_message(source);
            if cfg!(target_os = "macos") {
                format!("{} · {error}", diff_pane(*right))
            } else {
                error
            }
        }
        DiffLaunchError::Startup(error) => startup_error(error),
        DiffLaunchError::WorkerStart(error) if cfg!(target_os = "macos") => tr_format!(
            "Could not start the external comparison worker. {error}",
            error = error
        ),
        DiffLaunchError::WorkerStart(error) => {
            tr_format!("Could not launch the external tool: {error}", error = error)
        }
        DiffLaunchError::WorkerStopped => {
            text!("Не удалось запустить внешний инструмент сравнения").into()
        }
    };
    #[cfg(target_os = "macos")]
    if let Some(tool) = tool {
        return format!("{message}\n{}", executable(tool));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = tool;
    message
}

fn diff_pane(right: bool) -> &'static str {
    if right {
        text!("Правая панель")
    } else {
        text!("Левая панель")
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn system_open_error(
    mode: Mode,
    path: &Path,
    source: Option<&Location>,
    error: &system_open::Error,
) -> String {
    let path = format!("{path:?}");
    let title = match mode {
        Mode::Open => tr_format!("Could not open {path}.", path = path),
        Mode::Reveal | Mode::RevealWorkingCopy => {
            tr_format!("Could not show {path} in Finder.", path = path)
        }
    };
    let mut message = format!("{title}\n{}", system_open_detail(error));
    if let Some(source) = source {
        message.push('\n');
        message.push_str(&tr_format!(
            "SFTP source · {path}",
            path = display::launch_location(source)
        ));
    }
    message
}

#[cfg(target_os = "macos")]
pub(crate) fn system_open_detail(error: &system_open::Error) -> String {
    match error {
        system_open::Error::ResolvePath(error) => {
            tr_format!("Could not resolve the local path. {error}", error = error)
        }
        system_open::Error::InspectWorkingCopy(error) => {
            tr_format!("Could not inspect the working copy. {error}", error = error)
        }
        system_open::Error::NotRegularWorkingCopy => {
            text!("Рабочая копия должна быть обычным файлом.").into()
        }
        system_open::Error::RunCommand(error) => {
            tr_format!("The macOS open command failed. {error}", error = error)
        }
        system_open::Error::Rejected { status, stderr } => {
            let details = String::from_utf8_lossy(stderr);
            tr_format!(
                "macOS could not hand off the request ({status}). {details}",
                status = status,
                details = details.trim()
            )
            .trim_end()
            .into()
        }
        system_open::Error::StartWorker(error) => {
            tr_format!("Could not start the application. {error}", error = error)
        }
        system_open::Error::WorkerStopped => text!("Запуск приложения прервался.").into(),
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn started(details: &CommandDetails) -> String {
    let name = match &details.target {
        Target::Action { name, .. } => name,
        Target::Editor { .. } => text!("Внешний редактор"),
    };
    tr_format!("Started action “{name}”", name = name)
}

#[cfg(target_os = "macos")]
pub(crate) fn invocation_error(details: &CommandDetails, error: &InvocationError) -> String {
    let cause = match error {
        InvocationError::InspectFile(error) => tr_format!(
            "Could not inspect the file for the editor. {error}",
            error = error
        ),
        InvocationError::NotRegularFile => text!("Для редактора нужен обычный файл.").into(),
        InvocationError::Startup(StartupError::Spawn(error)) => error.to_string(),
        InvocationError::Startup(StartupError::Observe(error)) => tr_format!(
            "Could not check the external process. {error}",
            error = error
        ),
        InvocationError::Startup(error) => startup_error(error),
        InvocationError::WorkerStart(error) => {
            tr_format!("Could not start the application. {error}", error = error)
        }
        InvocationError::WorkerStopped => text!("Запуск приложения прервался.").into(),
    };
    let title = match &details.target {
        Target::Action { name, .. } => tr_format!(
            "Could not start action “{name}”. {error}",
            name = name,
            error = cause
        ),
        Target::Editor { source, .. } => editor_error(source, &cause),
    };
    let mut lines = vec![
        title,
        executable(&details.program),
        tr_format!(
            "Working folder · {path}",
            path = format_args!("{:?}", details.folder)
        ),
    ];
    match &details.target {
        Target::Action { files, .. } => {
            const MAX_PATHS: usize = 8;
            lines.push(tr_format!("Selected items ({count})", count = files.len()));
            lines.extend(files.iter().take(MAX_PATHS).map(|file| format!("{file:?}")));
            if files.len() > MAX_PATHS {
                lines.push(tr_format!(
                    "… and {count} more",
                    count = files.len() - MAX_PATHS
                ));
            }
        }
        Target::Editor { source, file } if source.remote() => {
            lines.push(tr_format!(
                "Working copy · {path}",
                path = format_args!("{file:?}")
            ));
        }
        Target::Editor { .. } => {}
    }
    lines.join("\n")
}

#[cfg(target_os = "macos")]
pub(crate) fn editor_preparation_error(source: &Location, program: &Path, error: &str) -> String {
    format!("{}\n{}", editor_error(source, error), executable(program))
}

#[cfg(target_os = "macos")]
fn editor_error(source: &Location, error: &str) -> String {
    tr_format!(
        "Could not open “{path}” in the external editor. {error}",
        path = display::launch_location(source),
        error = error
    )
}

#[cfg(target_os = "macos")]
fn executable(program: &Path) -> String {
    tr_format!("Executable · {path}", path = format_args!("{program:?}"))
}

#[cfg(target_os = "macos")]
pub(crate) fn template_error(error: &TemplateError) -> String {
    match error {
        TemplateError::Name => text!("Введите название действия.").into(),
        TemplateError::ProgramPath => text!("Укажите абсолютный путь к исполняемому файлу").into(),
        TemplateError::LocalSelection => {
            text!("Выберите локальные файлы или папки для запуска действия.").into()
        }
        TemplateError::Argument { index, problem } => {
            let message = match problem {
                ArgumentError::Nul => text!("Аргумент содержит нулевой байт."),
                ArgumentError::FilesMustBeSeparate => {
                    text!("{files} должен занимать отдельный аргумент.")
                }
                ArgumentError::Placeholder => text!(
                    "Неизвестная подстановка. Удваивайте фигурные скобки для буквального ввода."
                ),
                ArgumentError::SingleFile => text!("Для {file} выберите ровно один объект."),
            };
            tr_format!("Argument {index}. {error}", index = index, error = message)
        }
    }
}
