use crate::name_match::{normalize, Mask};
use crate::vfs::FileEntry;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug)]
pub(super) enum RuleKind {
    Mask,
    Extension,
}

impl RuleKind {
    pub(super) fn pattern(self, input: &str) -> Option<&str> {
        // Reject oversized input instead of truncating a mask and widening its match.
        if input.is_empty() || input.len() > 1024 {
            return None;
        }
        match self {
            Self::Mask => Some(input),
            Self::Extension => {
                let input = input.trim();
                let extension = input.strip_prefix('.').unwrap_or(input);
                (!extension.is_empty() && !extension.contains(['.', '*', '?', '/', '\\']))
                    .then_some(extension)
            }
        }
    }

    pub(super) fn compile(self, input: &str) -> Option<Rule> {
        let pattern = normalize(self.pattern(input)?).into_owned();
        Some(match self {
            Self::Mask => Rule::Mask(Mask::new(&pattern)),
            Self::Extension => Rule::Extension(pattern),
        })
    }
}

pub(super) enum Rule {
    Mask(Mask),
    Extension(String),
}

impl Rule {
    pub(super) fn matches(&self, entry: &FileEntry) -> bool {
        match self {
            Self::Mask(mask) => mask.matches(&normalize(&entry.name)),
            Self::Extension(wanted) => {
                !entry.is_dir
                    && entry
                        .name
                        .rsplit_once('.')
                        .is_some_and(|(stem, extension)| {
                            !stem.is_empty() && normalize(extension) == wanted.as_str()
                        })
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EntryIdentity {
    is_dir: bool,
    is_symlink: bool,
    created: Option<SystemTime>,
    // The server may only provide timestamps and size, with no creation time or file ID.
    fallback: Option<(Option<SystemTime>, u64)>,
}

impl From<&FileEntry> for EntryIdentity {
    fn from(entry: &FileEntry) -> Self {
        Self {
            is_dir: entry.is_dir,
            is_symlink: entry.is_symlink,
            created: entry.metadata.created,
            fallback: entry
                .metadata
                .created
                .is_none()
                .then_some((entry.modified, entry.size)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_inputs_are_validated_without_truncation() {
        assert!(RuleKind::Mask.compile(&"*".repeat(1025)).is_none());
        assert!(RuleKind::Mask.compile("").is_none());
        for invalid in [".", " ", "tar.gz", "*.gz", "dir/ext", "dir\\ext"] {
            assert!(RuleKind::Extension.compile(invalid).is_none());
        }
        assert!(matches!(
            RuleKind::Extension.compile(" .TXT "),
            Some(Rule::Extension(extension)) if extension == "txt"
        ));
    }
}
