use crate::vfs::FileEntry;
use icu_normalizer::ComposingNormalizerBorrowed;
use std::borrow::Cow;
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
        let pattern = fold(self.pattern(input)?).into_owned();
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
            Self::Mask(mask) => mask.matches(&fold(&entry.name)),
            Self::Extension(wanted) => {
                !entry.is_dir
                    && entry
                        .name
                        .rsplit_once('.')
                        .is_some_and(|(stem, extension)| {
                            !stem.is_empty() && fold(extension) == wanted.as_str()
                        })
            }
        }
    }
}

fn fold(text: &str) -> Cow<'_, str> {
    if text.is_ascii() && !text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Cow::Borrowed(text);
    }
    let lowercase: String = text.chars().flat_map(char::to_lowercase).collect();
    Cow::Owned(
        ComposingNormalizerBorrowed::new_nfc()
            .normalize(&lowercase)
            .into_owned(),
    )
}

pub(super) struct Mask(Vec<MaskPart>);

enum MaskPart {
    Star,
    One,
    Literal(String),
}

impl Mask {
    fn new(pattern: &str) -> Self {
        let mut parts = Vec::new();
        let mut start = 0;
        for (index, wildcard) in pattern.match_indices(['*', '?']) {
            if index > start {
                parts.push(MaskPart::Literal(pattern[start..index].into()));
            }
            match wildcard {
                "?" => parts.push(MaskPart::One),
                _ if !matches!(parts.last(), Some(MaskPart::Star)) => parts.push(MaskPart::Star),
                _ => {}
            }
            start = index + 1;
        }
        if start < pattern.len() {
            parts.push(MaskPart::Literal(pattern[start..].into()));
        }
        Self(parts)
    }

    fn matches(&self, mut name: &str) -> bool {
        use MaskPart::{Literal, One, Star};
        match self.0.as_slice() {
            [Star] => return true,
            [Literal(literal)] => return name == literal,
            [Literal(prefix), Star] => return name.starts_with(prefix),
            [Star, Literal(suffix)] => return name.ends_with(suffix),
            [Literal(prefix), Star, Literal(suffix)] => {
                return name
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.ends_with(suffix));
            }
            _ => {}
        }
        // Reject missing literals before walking possible wildcard positions.
        if self
            .0
            .iter()
            .any(|part| matches!(part, Literal(text) if !name.contains(text)))
        {
            return false;
        }

        let mut parts = self.0.as_slice();
        let mut star = None;
        while let Some((part, rest)) = parts.split_first() {
            let remainder = match part {
                Star => {
                    if rest.is_empty() {
                        return true;
                    }
                    star = Some((rest, name));
                    Some(name)
                }
                One => strip_character(name),
                Literal(text) => name.strip_prefix(text),
            };
            if let Some(remainder) = remainder {
                name = remainder;
                parts = rest;
                if !parts.is_empty() || name.is_empty() {
                    continue;
                }
            }
            // Backtrack only to the most recent star. A literal run is compared at once.
            let Some((after_star, retry)) = star else {
                return false;
            };
            let Some(retry) = strip_character(retry) else {
                return false;
            };
            name = retry;
            parts = after_star;
            star = Some((after_star, retry));
        }
        name.is_empty()
    }
}

fn strip_character(text: &str) -> Option<&str> {
    Some(&text[text.chars().next()?.len_utf8()..])
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
    fn glob_is_anchored_and_counts_unicode_characters() {
        let mask_matches = |pattern, name| Mask::new(pattern).matches(name);
        assert!(mask_matches("*", ""));
        assert!(mask_matches("*x?z**", "abxx☃z"));
        assert!(mask_matches("*?z", "πz"));
        assert!(mask_matches("?.txt", "é.txt"));
        assert!(!mask_matches("?.txt", "xy.txt"));
        assert!(!mask_matches("a*a", "a"));
        assert!(!mask_matches("a*a", "ab"));
        assert!(!mask_matches("a", "aa"));
        assert!(!mask_matches("x*?z", "abxx☃z"));
        assert_eq!(fold("CAFE\u{301}").as_ref(), "café");
        assert_eq!(fold("ǅ").as_ref(), "ǆ");
        assert!(!mask_matches(
            &format!("*{}?z*", "a".repeat(125)),
            &"a".repeat(255)
        ));
    }

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
