use crate::app::{find_saved_workspace, Startup, WorkspaceError};
use crate::i18n::{text, Language};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, PartialEq, Eq)]
enum Request {
    Help,
    Restore,
    LegacyFolder(PathBuf),
    Open(PathBuf),
    Workspace(String),
    Search { query: String, folder: PathBuf },
}

#[derive(Debug)]
enum Error {
    Startup(String),
    Usage(String),
    Target(String),
}

impl Error {
    fn report(self) -> ExitCode {
        let (code, message) = match self {
            Self::Startup(message) => (1, message),
            Self::Usage(message) => (2, message),
            Self::Target(message) => (3, message),
        };
        eprintln!("pawcommander: {message}");
        if code == 2 {
            eprintln!("{}", text("Справка доступна через pawcommander --help."));
        }
        ExitCode::from(code)
    }
}

pub(crate) fn run(arguments: impl IntoIterator<Item = OsString>) -> ExitCode {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|locale| !locale.is_empty());
    let language = if locale
        .as_deref()
        .is_some_and(|locale| locale.split(['_', '.', '@', '-']).next() == Some("ru"))
    {
        Language::Russian
    } else {
        Language::English
    };
    crate::i18n::set_language(language);
    match parse(arguments).and_then(execute) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => error.report(),
    }
}

fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Request, Error> {
    let mut arguments = arguments.into_iter();
    let Some(first) = arguments.next() else {
        return Ok(Request::Restore);
    };
    if is_help(&first) {
        return if arguments.next().is_none() {
            Ok(Request::Help)
        } else {
            Err(bad_arguments())
        };
    }
    if first == "--" {
        return match (arguments.next(), arguments.next()) {
            (Some(folder), None) => Ok(Request::LegacyFolder(folder.into())),
            _ => Err(bad_arguments()),
        };
    }
    let Some(command @ ("open" | "workspace" | "search")) = first.to_str() else {
        return if is_option(&first) || arguments.next().is_some() {
            Err(bad_arguments())
        } else {
            Ok(Request::LegacyFolder(first.into()))
        };
    };

    let mut operands: Vec<_> = arguments.collect();
    if let [argument] = operands.as_slice()
        && is_help(argument)
    {
        return Ok(Request::Help);
    }
    if operands.first().is_some_and(|argument| argument == "--") {
        operands.remove(0);
    } else if operands.iter().any(|argument| is_option(argument)) {
        return Err(bad_arguments());
    }
    match (command, operands.as_slice()) {
        ("open", [folder]) => Ok(Request::Open(folder.into())),
        ("workspace", [name]) => match name.to_str().map(str::trim).filter(|name| !name.is_empty())
        {
            Some(name) => Ok(Request::Workspace(name.to_owned())),
            None => Err(Error::Usage(
                text("Введите имя рабочего пространства в UTF-8.").into(),
            )),
        },
        ("search", [query] | [query, _]) => {
            let query = query
                .to_str()
                .ok_or_else(|| Error::Usage(text("Введите запрос поиска в UTF-8.").into()))?;
            crate::search::validate_name_query(query).map_err(Error::Usage)?;
            let folder = operands
                .get(1)
                .map_or_else(|| PathBuf::from("."), PathBuf::from);
            Ok(Request::Search {
                query: query.to_owned(),
                folder,
            })
        }
        _ => Err(bad_arguments()),
    }
}

fn is_help(argument: &OsStr) -> bool {
    argument == "--help" || argument == "-h"
}

fn is_option(argument: &OsStr) -> bool {
    argument.as_encoded_bytes().starts_with(b"-")
}

fn bad_arguments() -> Error {
    Error::Usage(text("Неизвестная команда, отсутствующие или лишние аргументы.").into())
}

fn execute(request: Request) -> Result<(), Error> {
    let startup = match request {
        Request::Help => {
            print_help();
            return Ok(());
        }
        Request::Restore => return launch(Startup::Restore, None),
        Request::LegacyFolder(folder) => {
            return launch(Some(checked_folder(&folder)?).into(), None)
        }
        Request::Open(folder) => Some(checked_folder(&folder)?).into(),
        Request::Search { query, folder } => Startup::Folder {
            path: checked_folder(&folder)?,
            search: Some(query),
        },
        Request::Workspace(name) => {
            let profile = read_profile(saved_profile_path().as_deref())?;
            let folders = find_saved_workspace(profile.as_deref(), &name)
                .map_err(|error| workspace_error(error, &name))?;
            return launch_isolated(Startup::Workspace(folders), profile.as_deref());
        }
    };
    let profile = read_profile(saved_profile_path().as_deref())?;
    launch_isolated(startup, profile.as_deref())
}

fn checked_folder(folder: &Path) -> Result<PathBuf, Error> {
    if folder.as_os_str().is_empty() {
        return Err(Error::Usage(text("Укажите путь к папке.").into()));
    }
    let path = std::path::absolute(folder).map_err(|error| {
        Error::Startup(crate::tr_format!(
            "Не удалось разрешить путь {folder:?} ({error})",
            "Could not resolve the path {folder:?} ({error})"
        ))
    })?;
    fs::read_dir(&path).map_err(|error| {
        Error::Target(crate::tr_format!(
            "Не удалось открыть папку {path:?} ({error})",
            "Could not open the folder {path:?} ({error})"
        ))
    })?;
    Ok(path)
}

fn saved_profile_path() -> Option<PathBuf> {
    eframe::storage_dir(crate::APP_ID).map(|folder| folder.join("app.ron"))
}

fn read_profile(path: Option<&Path>) -> Result<Option<Vec<u8>>, Error> {
    let Some(path) = path else { return Ok(None) };
    match fs::read(path) {
        Ok(profile) => Ok(Some(profile)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Startup(crate::tr_format!(
            "Не удалось прочитать сохранённые настройки ({error})",
            "Could not read saved settings ({error})"
        ))),
    }
}

fn workspace_error(error: WorkspaceError, name: &str) -> Error {
    match error {
        WorkspaceError::Missing => Error::Target(crate::tr_format!(
            "Рабочее пространство {name:?} не найдено",
            "Workspace {name:?} was not found"
        )),
        WorkspaceError::Incompatible => Error::Target(crate::tr_format!(
            "Пути рабочего пространства {name:?} несовместимы с системой",
            "Workspace {name:?} contains incompatible paths"
        )),
        WorkspaceError::Corrupt => {
            Error::Startup(text("Не удалось прочитать сохранённые рабочие пространства.").into())
        }
    }
}

fn launch_isolated(startup: Startup, profile: Option<&[u8]>) -> Result<(), Error> {
    let isolated = tempfile::Builder::new()
        .prefix("pawcommander-cli-")
        .tempdir()
        .map_err(profile_error)?;
    let state = isolated.path().join("app.ron");
    if let Some(profile) = profile {
        fs::write(&state, profile).map_err(profile_error)?;
    }
    launch(startup, Some(state))
}

fn profile_error(error: io::Error) -> Error {
    Error::Startup(crate::tr_format!(
        "Не удалось подготовить настройки для окна ({error})",
        "Could not prepare settings for the window ({error})"
    ))
}

fn launch(startup: Startup, persistence_path: Option<PathBuf>) -> Result<(), Error> {
    crate::run_gui(startup, persistence_path).map_err(|error| {
        Error::Startup(crate::tr_format!(
            "Не удалось запустить PawCommander ({error})",
            "Could not start PawCommander ({error})"
        ))
    })
}

fn print_help() {
    println!("{}", crate::i18n::cli_help());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn args(values: &[&str]) -> Result<Request, Error> {
        parse(values.iter().map(OsString::from))
    }

    #[test]
    fn parses_launch_modes_and_preserves_native_folder_arguments() {
        assert_eq!(args(&[]).unwrap(), Request::Restore);
        assert_eq!(
            args(&["open", "My folder"]).unwrap(),
            Request::Open("My folder".into())
        );
        assert_eq!(
            args(&["workspace", "  Daily work  "]).unwrap(),
            Request::Workspace("Daily work".into())
        );
        assert_eq!(
            args(&["search", "*.rs"]).unwrap(),
            Request::Search {
                query: "*.rs".into(),
                folder: ".".into()
            }
        );
        assert_eq!(
            args(&["search", "--", "--help", "-project"]).unwrap(),
            Request::Search {
                query: "--help".into(),
                folder: "-project".into()
            }
        );
        assert_eq!(
            args(&["--", "search"]).unwrap(),
            Request::LegacyFolder("search".into())
        );
        assert_eq!(args(&["search", "-h"]).unwrap(), Request::Help);

        let native = OsString::from_vec(b"/tmp/work\xff\n'".to_vec());
        assert_eq!(
            parse(["open".into(), native.clone()]).unwrap(),
            Request::Open(native.into())
        );
    }

    #[test]
    fn rejects_extra_options_missing_operands_and_invalid_queries() {
        for values in [
            &["open"][..],
            &["open", "folder", "extra"],
            &["search", "  "],
            &["search", "--unknown"],
            &["--help", "extra"],
            &["workspace", "  "],
            &["--unsupported"],
        ] {
            assert!(matches!(args(values), Err(Error::Usage(_))), "{values:?}");
        }
        let over_limit = "x".repeat(2000);
        assert!(matches!(
            args(&["search", &over_limit]),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn distinguishes_invalid_folder_operands_from_unavailable_folders() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("not a folder");
        fs::write(&file, "fixture").unwrap();
        assert!(matches!(
            checked_folder(Path::new("")),
            Err(Error::Usage(_))
        ));
        assert!(matches!(checked_folder(&file), Err(Error::Target(_))));
        assert!(matches!(
            checked_folder(&temp.path().join("missing")),
            Err(Error::Target(_))
        ));
        assert_eq!(checked_folder(temp.path()).unwrap(), temp.path());
    }
}
