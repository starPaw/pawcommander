//! Case-insensitive, canonically normalized filename matching.
use icu_normalizer::ComposingNormalizerBorrowed;
use std::borrow::Cow;

pub(crate) fn normalize(text: &str) -> Cow<'_, str> {
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

pub(crate) struct Mask(Vec<MaskPart>);

enum MaskPart {
    Star,
    One,
    Literal(String),
}

impl Mask {
    pub(crate) fn new(pattern: &str) -> Self {
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

    pub(crate) fn matches(&self, mut name: &str) -> bool {
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
        assert_eq!(normalize("CAFE\u{301}").as_ref(), "café");
        assert_eq!(normalize("ǅ").as_ref(), "ǆ");
        assert!(!mask_matches(
            &format!("*{}?z*", "a".repeat(125)),
            &"a".repeat(255)
        ));
    }
}
