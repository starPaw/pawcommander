use crate::{i18n::text, operations::TransferError};
use icu_normalizer::{ComposingNormalizerBorrowed, DecomposingNormalizerBorrowed};
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
};

const MAX_UTF16_UNITS: usize = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reason {
    Encoding,
    Characters,
    Trailing,
    Reserved,
    Unicode,
    Length,
    Collision,
    MetadataName,
}

impl Reason {
    pub(crate) fn label(self) -> &'static str {
        text(match self {
            Self::Encoding => "Некорректный UTF-8",
            Self::Characters => "Недопустимые символы",
            Self::Trailing => "Точки или пробелы в конце",
            Self::Reserved => "Зарезервированное имя",
            Self::Unicode => "Нормализация Unicode (NFC)",
            Self::Length => "Ограничение длины",
            Self::Collision => "Совпадение имён или регистра",
            Self::MetadataName => "Имя зарезервировано для метаданных macOS",
        })
    }
}

pub(super) struct Name {
    pub(super) value: String,
    pub(super) reasons: Vec<Reason>,
}

impl Name {
    pub(super) fn suggest(
        original: &OsStr,
        bytes: usize,
        apple_double: bool,
    ) -> Result<Self, TransferError> {
        let mut reasons = Vec::new();
        let raw = original.to_string_lossy();
        if original.to_str().is_none() {
            reasons.push(Reason::Encoding);
        }
        let mut invalid = false;
        let cleaned: String = raw
            .chars()
            .map(|c| {
                if c <= '\u{1f}'
                    || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                {
                    invalid = true;
                    '_'
                } else {
                    c
                }
            })
            .collect();
        if invalid {
            reasons.push(Reason::Characters);
        }
        let trimmed = cleaned.trim_end_matches([' ', '.']);
        if trimmed != cleaned {
            reasons.push(Reason::Trailing);
        }
        let usable = if trimmed.is_empty() { "item" } else { trimmed };
        let nfc = ComposingNormalizerBorrowed::new_nfc().normalize(usable);
        if nfc != usable {
            reasons.push(Reason::Unicode);
        }
        let mut value = nfc.into_owned();
        if reserved(&value) {
            value.insert(0, '_');
            reasons.push(Reason::Reserved);
        }
        if apple_double && value.starts_with("._") {
            value.replace_range(..1, "_");
            reasons.push(Reason::MetadataName);
        }
        if destination_bytes(&value, apple_double) > bytes
            || value.encode_utf16().count() > MAX_UTF16_UNITS
        {
            value = fit(&value, "", bytes, apple_double)?;
            reasons.push(Reason::Length);
        }
        Ok(Self { value, reasons })
    }

    pub(super) fn reserve(
        mut self,
        bytes: usize,
        apple_double: bool,
        used: &mut HashSet<String>,
        mut exists: impl FnMut(&OsStr) -> Result<bool, TransferError>,
    ) -> Result<Self, TransferError> {
        if reserve(&self.value, apple_double, used, &mut exists)? {
            return Ok(self);
        }
        for index in 2..=100_001 {
            let suffix = format!(" ({index})");
            let name = fit(&self.value, &suffix, bytes, apple_double)?;
            if reserve(&name, apple_double, used, &mut exists)? {
                self.value = name;
                self.reasons.push(Reason::Collision);
                return Ok(self);
            }
        }
        Err(text("Не удалось подобрать совместимое свободное имя").into())
    }
}

fn reserve(
    name: &str,
    apple_double: bool,
    used: &mut HashSet<String>,
    exists: &mut impl FnMut(&OsStr) -> Result<bool, TransferError>,
) -> Result<bool, TransferError> {
    let name = OsStr::new(name);
    let key = portable_key(name);
    if used.contains(&key) || exists(name)? {
        return Ok(false);
    }
    if apple_double {
        let companion = metadata_name(name);
        let key = portable_key(&companion);
        if used.contains(&key) || exists(&companion)? {
            return Ok(false);
        }
        used.insert(key);
    }
    used.insert(key);
    Ok(true)
}

pub(super) fn metadata_name(name: &OsStr) -> OsString {
    let mut companion = OsString::from("._");
    companion.push(name);
    companion
}

pub(super) fn destination_bytes(name: &str, apple_double: bool) -> usize {
    if apple_double {
        // Darwin can enumerate FAT names in decomposed form. Budget enough
        // bytes for either spelling to keep the destination verifiable.
        return DecomposingNormalizerBorrowed::new_nfd()
            .normalize(name)
            .len();
    }
    name.len()
}

pub(super) fn portable_key(name: &OsStr) -> String {
    // Use a conservative caseless comparison, including final sigma and the
    // two German sharp S forms. The volume still checks its own aliases.
    let upper: String = name
        .to_string_lossy()
        .trim_end_matches([' ', '.'])
        .chars()
        .flat_map(char::to_lowercase)
        .flat_map(char::to_uppercase)
        .collect();
    ComposingNormalizerBorrowed::new_nfc()
        .normalize(&upper)
        .into_owned()
}

fn reserved(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let stem = stem.to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|s| {
            matches!(
                s,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

fn fit(
    name: &str,
    suffix: &str,
    bytes: usize,
    apple_double: bool,
) -> Result<String, TransferError> {
    let bytes = bytes.min(MAX_UTF16_UNITS * 4);
    let units = MAX_UTF16_UNITS;
    if bytes <= suffix.len() || units <= suffix.len() {
        return Err(path_limit());
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let extension = prefix(
        extension,
        bytes - suffix.len() - 1,
        units - suffix.len() - 1,
        apple_double,
    )
    .trim_end_matches([' ', '.']);
    let stem_bytes = bytes - suffix.len() - destination_bytes(extension, apple_double);
    let stem_units = units - suffix.len() - extension.encode_utf16().count();
    let stem = prefix(stem, stem_bytes, stem_units, apple_double).trim_end_matches([' ', '.']);
    let stem = if stem.is_empty() { "_" } else { stem };
    let mut result = format!("{stem}{suffix}{extension}");
    if reserved(&result) {
        let rest = prefix(&result, bytes - 1, units - 1, apple_double).trim_end_matches([' ', '.']);
        result = format!("_{rest}");
    }
    Ok(result)
}

fn prefix(value: &str, bytes: usize, units: usize, apple_double: bool) -> &str {
    let (mut used_bytes, mut used_units) = (0, 0);
    for (offset, c) in value.char_indices() {
        used_bytes += destination_bytes(c.encode_utf8(&mut [0; 4]), apple_double);
        used_units += c.len_utf16();
        if used_bytes > bytes || used_units > units {
            return &value[..offset];
        }
    }
    value
}

pub(super) fn path_limit() -> TransferError {
    text("Путь назначения слишком длинный. Выберите папку ближе к корню.").into()
}
