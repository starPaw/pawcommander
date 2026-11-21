use crate::{
    i18n::text,
    name_match::{normalize, Mask},
    operations::TransferError,
};
use std::{collections::BTreeSet, path::Path};

const MAX_PATTERNS: usize = 64;
const MAX_TEXT_BYTES: usize = 8 * 1024;

#[derive(Default)]
pub(crate) struct Exclusions {
    patterns: Vec<String>,
    names: Vec<Mask>,
    paths: Vec<Vec<Mask>>,
}

impl Exclusions {
    pub(crate) fn parse(input: &str) -> Result<Self, TransferError> {
        let too_many = || text!("Используйте до 64 исключений общим размером до 8 КиБ.").into();
        if input.len() > MAX_TEXT_BYTES {
            return Err(too_many());
        }
        let mut result = Self::default();
        let mut patterns = BTreeSet::new();
        for (index, pattern) in input.lines().enumerate() {
            if pattern.trim().is_empty() {
                continue;
            }
            let relative = pattern.strip_prefix("./").unwrap_or(pattern);
            if relative.contains('\0')
                || relative
                    .split('/')
                    .any(|part| matches!(part, "" | "." | ".."))
            {
                let line = index + 1;
                return Err(crate::tr_format!(
                    "Исключение в строке {line} должно быть относительным именем или путём.",
                    "The exclusion on line {line} must be a relative name or path."
                )
                .into());
            }
            let relative = normalize(relative);
            if pattern.contains('/') {
                let key = format!("./{relative}");
                if patterns.insert(key) {
                    result
                        .paths
                        .push(relative.split('/').map(Mask::new).collect());
                }
            } else if patterns.insert(relative.to_string()) {
                result.names.push(Mask::new(&relative));
            }
            if patterns.len() > MAX_PATTERNS {
                return Err(too_many());
            }
        }
        result.patterns = patterns.into_iter().collect();
        Ok(result)
    }

    pub(super) fn patterns(&self) -> &[String] {
        &self.patterns
    }

    pub(super) fn skips(&self, path: &Path) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        if !self.names.is_empty()
            && let Some(name) = path.file_name()
        {
            let name = name.to_string_lossy();
            let name = normalize(&name);
            if self.names.iter().any(|mask| mask.matches(&name)) {
                return true;
            }
        }
        if self.paths.is_empty() {
            return false;
        }
        self.paths.iter().any(|masks| {
            masks.len() == path.iter().count()
                && masks
                    .iter()
                    .zip(path.iter())
                    .all(|(mask, component)| mask.matches(&normalize(&component.to_string_lossy())))
        })
    }
}
