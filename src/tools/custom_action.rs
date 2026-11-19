use super::launch::{report_startup, start_error};
use crate::i18n::text;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LOCAL_SELECTION: &str = "Выберите локальные файлы или папки для запуска действия.";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Template {
    pub(crate) name: String,
    pub(crate) program: String,
    pub(crate) arguments: Vec<String>,
}

impl Default for Template {
    fn default() -> Self {
        Self {
            name: String::new(),
            program: String::new(),
            arguments: vec!["{files}".into()],
        }
    }
}

impl Template {
    fn program_path(&self) -> Result<&Path, String> {
        if self.name.trim().is_empty() {
            return Err(text("Введите название действия.").into());
        }
        let program = Path::new(&self.program);
        if !program.is_absolute() || self.program.contains('\0') {
            return Err(text("Укажите абсолютный путь к исполняемому файлу").into());
        }
        Ok(program)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        self.program_path()?;
        for source in &self.arguments {
            Argument::parse(source)?;
        }
        Ok(())
    }

    pub(crate) fn prepare(&self, folder: &Path, files: &[PathBuf]) -> Result<Invocation, String> {
        let program = self.program_path()?.to_owned();
        if !folder.is_absolute() || files.is_empty() || files.iter().any(|path| !path.is_absolute())
        {
            return Err(text(LOCAL_SELECTION).into());
        }
        let mut arguments = Vec::with_capacity(self.arguments.len());
        for source in &self.arguments {
            match Argument::parse(source)? {
                Argument::Files => {
                    arguments.extend(files.iter().map(|path| path.as_os_str().to_owned()))
                }
                Argument::Value(parts) => {
                    let mut value = OsString::new();
                    for part in parts {
                        match part {
                            Part::Literal(literal) => value.push(literal),
                            Part::Folder => value.push(folder),
                            Part::File => {
                                let [file] = files else {
                                    return Err(
                                        text("Для {file} выберите ровно один объект.").into()
                                    );
                                };
                                value.push(file);
                            }
                        }
                    }
                    arguments.push(value);
                }
            }
        }
        Ok(Invocation {
            name: self.name.trim().to_owned(),
            program,
            folder: folder.to_owned(),
            arguments,
        })
    }
}

enum Argument<'a> {
    Files,
    Value(Vec<Part<'a>>),
}

enum Part<'a> {
    Literal(&'a str),
    Folder,
    File,
}

impl<'a> Argument<'a> {
    fn parse(source: &'a str) -> Result<Self, String> {
        if source.contains('\0') {
            return Err(text("Аргумент содержит нулевой байт.").into());
        }
        if source == "{files}" {
            return Ok(Self::Files);
        }
        let mut parts = Vec::new();
        let mut rest = source;
        while let Some(index) = rest.find(['{', '}']) {
            let (literal, suffix) = rest.split_at(index);
            if !literal.is_empty() {
                parts.push(Part::Literal(literal));
            }
            let (part, tail) = if let Some(tail) = suffix.strip_prefix("{{") {
                (Part::Literal("{"), tail)
            } else if let Some(tail) = suffix.strip_prefix("}}") {
                (Part::Literal("}"), tail)
            } else if let Some(tail) = suffix.strip_prefix("{file}") {
                (Part::File, tail)
            } else if let Some(tail) = suffix.strip_prefix("{folder}") {
                (Part::Folder, tail)
            } else if suffix.starts_with("{files}") {
                return Err(text("{files} должен занимать отдельный аргумент.").into());
            } else {
                return Err(text(
                    "Неизвестная подстановка. Удваивайте фигурные скобки для буквального ввода.",
                )
                .into());
            };
            parts.push(part);
            rest = tail;
        }
        if !rest.is_empty() {
            parts.push(Part::Literal(rest));
        }
        Ok(Self::Value(parts))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub(crate) name: String,
    pub(crate) program: PathBuf,
    pub(crate) folder: PathBuf,
    pub(crate) arguments: Vec<OsString>,
}

impl Invocation {
    pub(crate) fn launch(self, report: impl FnOnce(Result<String, String>)) {
        let child = Command::new(&self.program)
            .args(&self.arguments)
            .current_dir(&self.folder)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            Ok(child) => report_startup(child, |result| {
                let name = self.name;
                report(result.map(|()| {
                    crate::tr_format!("Запущено действие «{name}»", "Started action “{name}”")
                }));
            }),
            Err(error) => report(Err(start_error(error))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    fn template(arguments: &[&str]) -> Template {
        Template {
            name: "Example".into(),
            program: "/usr/bin/open".into(),
            arguments: arguments.iter().map(|source| (*source).into()).collect(),
        }
    }

    #[test]
    fn expands_selected_paths_in_order_without_converting_path_bytes() {
        let folder = Path::new("/work space");
        let first = folder.join(OsStr::from_bytes(b"first\xff '\n"));
        let second = folder.join("-second $literal.txt");
        let invocation = template(&[
            "-b",
            "org.videolan.vlc",
            "{files}",
            "dir={folder}",
            "{{literal}}",
            "",
        ])
        .prepare(folder, &[first.clone(), second.clone()])
        .unwrap();
        assert_eq!(invocation.folder, folder);
        assert_eq!(
            invocation.arguments,
            [
                "-b".into(),
                "org.videolan.vlc".into(),
                first.into_os_string(),
                second.into_os_string(),
                "dir=/work space".into(),
                "{literal}".into(),
                "".into(),
            ]
        );
    }

    #[test]
    fn validates_templates_and_requires_one_item_for_a_single_path() {
        let folder = Path::new("/work");
        let first = folder.join("one file");
        let single = template(&["--input={file}"]);
        let invocation = single
            .prepare(folder, std::slice::from_ref(&first))
            .unwrap();
        assert_eq!(invocation.arguments, ["--input=/work/one file"]);
        assert!(single
            .prepare(folder, &[first, folder.join("two")])
            .is_err());
        for invalid in ["--input={files}", "{unknown}", "unmatched}", "\0"] {
            assert!(template(&[invalid]).validate().is_err(), "{invalid:?}");
        }
        let relative_program = Template {
            program: "open".into(),
            ..template(&["{files}"])
        };
        assert!(relative_program.validate().is_err());
    }

    #[test]
    fn a_missing_executable_reports_a_launch_error() {
        let folder = tempfile::tempdir().unwrap();
        let action = Template {
            program: folder
                .path()
                .join("missing executable")
                .to_str()
                .unwrap()
                .into(),
            ..template(&["{files}"])
        };
        let launch = action
            .prepare(folder.path(), &[folder.path().join("item")])
            .unwrap();
        launch.launch(|result| assert!(result.is_err()));
    }
}
