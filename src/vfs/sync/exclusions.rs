use crate::{
    file_error::{SyncError, SyncExclusionProblem},
    name_match::{normalize, Mask},
    operations::TransferError,
};
use std::{collections::BTreeSet, path::Path};

const MAX_PATTERNS: usize = 64;

#[derive(Default)]
pub(crate) struct Exclusions {
    patterns: Vec<String>,
    names: Vec<Mask>,
    paths: Vec<Vec<Mask>>,
}

impl Exclusions {
    pub(crate) const MAX_TEXT_BYTES: usize = 8 * 1024;

    pub(crate) fn parse(input: &str) -> Result<Self, TransferError> {
        if input.len() > Self::MAX_TEXT_BYTES {
            return Err(SyncError::ExclusionsTextLimit(Self::MAX_TEXT_BYTES).into());
        }
        let mut result = Self::default();
        let mut patterns = BTreeSet::new();
        for (index, pattern) in input.lines().enumerate() {
            if pattern.trim().is_empty() {
                continue;
            }
            let line = index + 1;
            let relative = relative_pattern(pattern)
                .map_err(|problem| SyncError::InvalidExclusion { line, problem })?;
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
                return Err(SyncError::ExclusionsCountLimit {
                    limit: MAX_PATTERNS,
                    line,
                }
                .into());
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

fn relative_pattern(pattern: &str) -> Result<&str, SyncExclusionProblem> {
    if pattern.starts_with('/') {
        return Err(SyncExclusionProblem::AbsolutePath);
    }
    let relative = pattern.strip_prefix("./").unwrap_or(pattern);
    if relative.is_empty() {
        return Err(SyncExclusionProblem::MissingName);
    }
    if relative.contains('\0') {
        return Err(SyncExclusionProblem::NulCharacter);
    }
    for component in relative.split('/') {
        match component {
            "" => return Err(SyncExclusionProblem::EmptyComponent),
            "." | ".." => return Err(SyncExclusionProblem::DotComponent),
            _ => {}
        }
    }
    Ok(relative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_error::Failure;

    fn parse_error(input: &str) -> SyncError {
        let error = Exclusions::parse(input)
            .err()
            .expect("expected exclusion validation to fail");
        let TransferError::Failed(Failure::Sync(error)) = error else {
            panic!("expected a typed sync error, got {error:?}");
        };
        *error
    }

    #[test]
    fn invalid_exclusions_keep_the_original_line_and_reason() {
        use SyncExclusionProblem::*;
        for (pattern, expected) in [
            ("/cache", AbsolutePath),
            ("./", MissingName),
            (".//cache", EmptyComponent),
            ("cache/", EmptyComponent),
            ("cache/.", DotComponent),
            ("../cache", DotComponent),
            ("ca\0che", NulCharacter),
        ] {
            let input = format!("cache-ok\r\n\r\n{pattern}");
            assert!(
                matches!(
                    parse_error(&input),
                    SyncError::InvalidExclusion { line: 3, problem } if problem == expected
                ),
                "unexpected result for {pattern:?}"
            );
        }
    }

    #[test]
    fn exclusion_limits_count_utf8_bytes_and_distinct_normalized_rules() {
        let byte_limit = Exclusions::MAX_TEXT_BYTES;
        let mut text = "ř".repeat(byte_limit / 2);
        assert_eq!(text.len(), byte_limit);
        assert!(Exclusions::parse(&text).is_ok());
        text.push('x');
        assert!(matches!(
            parse_error(&text),
            SyncError::ExclusionsTextLimit(limit) if limit == byte_limit
        ));

        let mut lines: Vec<_> = (0..MAX_PATTERNS).map(|n| format!("item-{n}")).collect();
        lines.push(String::new());
        lines.push("ITEM-0".into());
        let input = lines.join("\n");
        assert_eq!(
            Exclusions::parse(&input).unwrap().patterns().len(),
            MAX_PATTERNS
        );
        lines.push("one-more".into());
        assert!(matches!(
            parse_error(&lines.join("\n")),
            SyncError::ExclusionsCountLimit { limit: MAX_PATTERNS, line } if line == lines.len()
        ));
    }
}
