use super::launch::{CommandDetails, Invocation, Target};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TemplateError {
    Name,
    ProgramPath,
    LocalSelection,
    Argument {
        index: usize,
        problem: ArgumentError,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ArgumentError {
    Nul,
    FilesMustBeSeparate,
    Placeholder,
    SingleFile,
}

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
    fn program_path(&self) -> Result<&Path, TemplateError> {
        if self.name.trim().is_empty() {
            return Err(TemplateError::Name);
        }
        let program = Path::new(&self.program);
        if !program.is_absolute() || self.program.contains('\0') {
            return Err(TemplateError::ProgramPath);
        }
        Ok(program)
    }

    pub(crate) fn validate(&self) -> Result<(), TemplateError> {
        self.program_path()?;
        for (index, source) in self.arguments.iter().enumerate() {
            Argument::parse(source).map_err(|problem| TemplateError::Argument {
                index: index + 1,
                problem,
            })?;
        }
        Ok(())
    }

    pub(crate) fn prepare(
        &self,
        folder: &Path,
        files: &[PathBuf],
    ) -> Result<Invocation, TemplateError> {
        let program = self.program_path()?.to_owned();
        if !folder.is_absolute() || files.is_empty() || files.iter().any(|path| !path.is_absolute())
        {
            return Err(TemplateError::LocalSelection);
        }
        let mut arguments = Vec::with_capacity(self.arguments.len());
        for (index, source) in self.arguments.iter().enumerate() {
            let error = |problem| TemplateError::Argument {
                index: index + 1,
                problem,
            };
            match Argument::parse(source).map_err(error)? {
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
                                    return Err(error(ArgumentError::SingleFile));
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
            details: CommandDetails {
                target: Target::Action {
                    name: self.name.trim().to_owned(),
                    files: files.to_owned(),
                },
                program,
                folder: folder.to_owned(),
            },
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
    fn parse(source: &'a str) -> Result<Self, ArgumentError> {
        if source.contains('\0') {
            return Err(ArgumentError::Nul);
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
                return Err(ArgumentError::FilesMustBeSeparate);
            } else {
                return Err(ArgumentError::Placeholder);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::launch::{InvocationError, StartupError};
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
        assert_eq!(invocation.details.folder, folder);
        assert_eq!(
            invocation.details.target,
            Target::Action {
                name: "Example".into(),
                files: vec![first.clone(), second.clone()],
            }
        );
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
        assert_eq!(
            single
                .prepare(folder, &[first, folder.join("two")])
                .unwrap_err(),
            TemplateError::Argument {
                index: 1,
                problem: ArgumentError::SingleFile,
            }
        );
        for invalid in ["--input={files}", "{unknown}", "unmatched}", "\0"] {
            assert!(template(&[invalid]).validate().is_err(), "{invalid:?}");
        }
        assert_eq!(
            template(&["--literal", "{unknown}"])
                .validate()
                .unwrap_err(),
            TemplateError::Argument {
                index: 2,
                problem: ArgumentError::Placeholder,
            }
        );
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
        launch.launch(|result| {
            assert!(matches!(
                result,
                Err(InvocationError::Startup(StartupError::Spawn(error)))
                    if error.raw_os_error() == Some(libc::ENOENT)
            ));
        });
    }
}
