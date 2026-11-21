//! Versioned, capability-limited viewer packages for macOS.

mod worker;

use crate::i18n::text;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::Arc,
};
use worker::Request;
pub(crate) use worker::{run as run_worker, WORKER_ARG};

const API_VERSION: u32 = 1;
const MAX_PACKAGE: usize = 128 * 1024;
const MAX_MODULE: usize = 64 * 1024;
const MAX_INPUT: usize = 32 * 1024;
const MAX_EXTENSIONS: usize = 24;
const BUILTIN_ID: &str = "pawcommander.line-count";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Permission {
    ReadSelectedFile,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Extension {
    api_version: u32,
    id: String,
    name: String,
    extensions: Vec<String>,
    permissions: Vec<Permission>,
    module: String,
}

#[derive(Clone)]
pub(crate) struct Catalog {
    builtin: Arc<Extension>,
    installed: Vec<Arc<Extension>>,
}

impl Catalog {
    pub(crate) fn restore(installed: Vec<Arc<Extension>>) -> Self {
        let builtin = serde_json::from_str(include_str!(
            "../assets/extensions/line-count.pawextension.json"
        ))
        .expect("bundled viewer manifest");
        let mut catalog = Self {
            builtin: Arc::new(builtin),
            installed: Vec::new(),
        };
        for extension in installed {
            let _ = catalog.install(extension);
        }
        catalog
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &Arc<Extension>> {
        std::iter::once(&self.builtin).chain(&self.installed)
    }

    pub(crate) fn installed(&self) -> &[Arc<Extension>] {
        &self.installed
    }

    pub(crate) fn install(&mut self, extension: Arc<Extension>) -> Result<(), String> {
        extension.check_metadata()?;
        if extension.id.starts_with("pawcommander.")
            || self.entries().any(|entry| entry.id == extension.id)
        {
            return Err(text!("Идентификатор расширения уже занят.").into());
        }
        if self.installed.len() >= MAX_EXTENSIONS {
            return Err(text!("Можно установить до 24 просмотрщиков.").into());
        }
        self.installed.push(extension);
        Ok(())
    }

    pub(crate) fn remove(&mut self, id: &str) {
        self.installed.retain(|extension| extension.id != id);
    }
}

impl Extension {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn name(&self) -> &str {
        if self.is_builtin() {
            text!("Счётчик строк")
        } else {
            &self.name
        }
    }

    pub(crate) fn is_builtin(&self) -> bool {
        self.id == BUILTIN_ID
    }

    pub(crate) fn extensions(&self) -> &[String] {
        &self.extensions
    }

    pub(crate) fn reads_file(&self) -> bool {
        self.permissions.contains(&Permission::ReadSelectedFile)
    }

    pub(crate) fn matches(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|suffix| suffix.to_str())
            .is_some_and(|suffix| {
                self.extensions
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(suffix))
            })
    }

    pub(crate) fn load(path: &Path) -> Result<Arc<Self>, String> {
        let bytes = read_prefix(path, MAX_PACKAGE + 1).map_err(|error| {
            crate::tr_format!(
                "Could not read the extension package ({error}).",
                error = error
            )
        })?;
        let extension = Self::decode(&bytes)?;
        worker::invoke(Request::Validate, &extension.wasm()?, &[])?;
        Ok(Arc::new(extension))
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_PACKAGE {
            return Err(text!("Пакет расширения превышает 128 КиБ.").into());
        }
        let extension: Self = serde_json::from_slice(bytes)
            .map_err(|_| text!("Неверный формат пакета расширения."))?;
        extension.check_metadata()?;
        Ok(extension)
    }

    fn check_metadata(&self) -> Result<(), String> {
        if self.api_version != API_VERSION {
            return Err(crate::tr_format!(
                "Unsupported extension API version {version}.",
                version = self.api_version
            ));
        }
        let valid_id = (1..=80).contains(&self.id.len())
            && self.id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
            });
        let valid_name = !self.name.trim().is_empty()
            && self.name.len() <= 120
            && !self.name.chars().any(char::is_control);
        let valid_extensions = (1..=16).contains(&self.extensions.len())
            && self.extensions.iter().all(|suffix| {
                (1..=16).contains(&suffix.len())
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            });
        if !valid_id
            || !valid_name
            || !valid_extensions
            || self.permissions.len() > 1
            || self.module.is_empty()
            || self.module.len() > 4 * MAX_MODULE.div_ceil(3)
        {
            return Err(text!("Недопустимые поля пакета расширения.").into());
        }
        Ok(())
    }

    fn wasm(&self) -> Result<Vec<u8>, String> {
        let invalid = || text!("Модуль просмотрщика повреждён или превышает лимиты.").to_owned();
        let bytes = STANDARD.decode(&self.module).map_err(|_| invalid())?;
        if bytes.len() > MAX_MODULE {
            return Err(invalid());
        }
        Ok(bytes)
    }

    pub(crate) fn preview(&self, path: &Path) -> Result<Preview, String> {
        if !self.matches(path) {
            return Err(text!("Просмотрщик не поддерживает этот тип файла.").into());
        }
        let wasm = self.wasm()?;
        let mut bytes = if self.reads_file() {
            read_prefix(path, MAX_INPUT + 1).map_err(|error| {
                crate::tr_format!("Could not read the selected file ({error}).", error = error)
            })?
        } else {
            Vec::new()
        };
        let truncated = bytes.len() > MAX_INPUT;
        bytes.truncate(MAX_INPUT);
        let text = worker::invoke(Request::Preview, &wasm, &bytes)?;
        Ok(Preview { text, truncated })
    }
}

#[derive(Debug)]
pub(crate) struct Preview {
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

fn read_prefix(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn package(wat: &str, reads_file: bool) -> serde_json::Value {
        let permissions = if reads_file {
            vec!["read_selected_file"]
        } else {
            Vec::new()
        };
        json!({
            "api_version": API_VERSION,
            "id": "example.test",
            "name": "Test viewer",
            "extensions": ["txt"],
            "permissions": permissions,
            "module": STANDARD.encode(wat::parse_str(wat).unwrap())
        })
    }

    fn viewer(expression: &str, reads_file: bool) -> Extension {
        let wat = format!(
            r#"(module
            (memory (export "memory") 1)
            (func (export "paw_preview") (param i32) (result i64) {expression})
        )"#
        );
        Extension::decode(package(&wat, reads_file).to_string().as_bytes()).unwrap()
    }

    #[test]
    fn bundled_viewer_matches_its_source_and_counts_only_the_prefix() {
        let catalog = Catalog::restore(Vec::new());
        let sample = catalog.entries().next().unwrap();
        let source = include_str!("../assets/extensions/line-count.wat");
        assert_eq!(
            STANDARD.decode(&sample.module).unwrap(),
            wat::parse_str(source).unwrap()
        );
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sample.TXT");
        for (contents, expected) in [("", "0"), ("one\ntwo\nthree", "3"), ("one\n", "1")] {
            fs::write(&path, contents.as_bytes()).unwrap();
            let result = sample.preview(&path).unwrap();
            assert_eq!(result.text, expected);
            assert!(!result.truncated);
        }
        fs::write(&path, vec![b'\n'; MAX_INPUT + 20]).unwrap();
        let result = sample.preview(&path).unwrap();
        assert_eq!(result.text, MAX_INPUT.to_string());
        assert!(result.truncated);
    }

    #[test]
    fn viewer_without_file_permission_gets_no_bytes_and_skips_opening_the_path() {
        let expression =
            "(if (result i64) (local.get 0) (then (unreachable)) (else (i64.const 0)))";
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing.txt");
        assert!(viewer(expression, false).preview(&path).is_ok());
        assert!(viewer(expression, true).preview(&path).is_err());
        fs::write(&path, b"123").unwrap();
        assert!(viewer(expression, false).preview(&path).is_ok());
        assert!(viewer(expression, true).preview(&path).is_err());
        let link = temp.path().join("link.txt");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(viewer("i64.const 0", true).preview(&link).is_err());
    }

    #[test]
    fn manifest_validation_rejects_incompatible_versions_permissions_imports_and_exports() {
        let base = package(
            r#"(module
            (memory (export "memory") 1)
            (func (export "paw_preview") (param i32) (result i64) (i64.const 0)))"#,
            false,
        );
        for (field, value) in [
            ("api_version", json!(2)),
            ("permissions", json!(["write_file"])),
            ("extensions", json!(["../txt"])),
        ] {
            let mut wrong = base.clone();
            wrong[field] = value;
            assert!(Extension::decode(wrong.to_string().as_bytes()).is_err());
        }
        assert!(Extension::decode(&vec![b' '; MAX_PACKAGE + 1]).is_err());
        let imported = package(
            r#"(module (import "host" "anything" (func))
            (memory (export "memory") 1)
            (func (export "paw_preview") (param i32) (result i64) (i64.const 0)))"#,
            false,
        );
        let imported = Extension::decode(imported.to_string().as_bytes()).unwrap();
        assert!(worker::invoke(Request::Validate, &imported.wasm().unwrap(), &[]).is_err());
        let invalid = package(
            r#"(module
            (memory (export "memory") 1)
            (func (export "paw_preview") (result i32) (i32.const 0)))"#,
            false,
        );
        let invalid = Extension::decode(invalid.to_string().as_bytes()).unwrap();
        assert!(worker::invoke(Request::Validate, &invalid.wasm().unwrap(), &[]).is_err());
    }

    #[test]
    fn fuel_memory_and_output_failures_leave_the_next_viewer_usable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("safe.txt");
        for expression in [
            "(loop $forever (br $forever)) (i64.const 0)",
            "(drop (memory.grow (i32.const 64))) (i64.const 0)",
            "(i64.const 16385)",
            "(i64.const -4294967295)",
            "(unreachable)",
        ] {
            assert!(
                viewer(expression, false).preview(&path).is_err(),
                "{expression}"
            );
            assert!(viewer("(i64.const 0)", false).preview(&path).is_ok());
        }
    }

    #[test]
    fn catalog_preserves_the_first_package_for_an_id_and_the_built_in_viewer() {
        let mut catalog = Catalog::restore(Vec::new());
        let first = Arc::new(viewer("(i64.const 0)", false));
        catalog.install(Arc::clone(&first)).unwrap();
        assert!(catalog
            .install(Arc::new(viewer("(i64.const 1)", false)))
            .is_err());
        assert!(Arc::ptr_eq(&catalog.installed()[0], &first));
        catalog.remove(BUILTIN_ID);
        assert_eq!(catalog.entries().count(), 2);
        catalog.remove(first.id());
        assert!(catalog.installed().is_empty());
        assert_eq!(catalog.entries().next().unwrap().id(), BUILTIN_ID);
    }
}
