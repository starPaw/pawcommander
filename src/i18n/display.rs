//! Locale-specific display of dates and quantities. Stored values and input formats stay typed.
use super::{language, Language};
use chrono::{DateTime, Local};

#[cfg(target_os = "macos")]
pub(crate) fn launch_location(location: &crate::vfs::Location) -> String {
    let label = match location {
        crate::vfs::Location::Sftp { endpoint, path } => {
            format!("{} · {}", endpoint.label(), path.display())
        }
        _ => location.display(),
    };
    label.escape_debug().to_string()
}

#[derive(Clone, Copy)]
pub(crate) enum DateStyle {
    Column,
    Minutes,
    Seconds,
    Zoned,
    ZoneName,
}

pub(crate) fn date(value: DateTime<Local>, style: DateStyle) -> String {
    value.format(date_pattern(language(), style)).to_string()
}

fn date_pattern(language: Language, style: DateStyle) -> &'static str {
    #[cfg(target_os = "macos")]
    match (language, style) {
        (Language::English, DateStyle::Column) => "%-m/%-d/%y %-I:%M %p",
        (Language::English, DateStyle::Minutes) => "%-m/%-d/%Y, %-I:%M %p",
        (Language::English, DateStyle::Seconds) => "%-m/%-d/%Y, %-I:%M:%S %p",
        (Language::English, DateStyle::Zoned) => "%-m/%-d/%Y, %-I:%M:%S %p %:z",
        (Language::English, DateStyle::ZoneName) => "%-m/%-d/%Y, %-I:%M:%S %p %Z",
        (Language::Russian, DateStyle::Column) => "%d.%m.%y %H:%M",
        (Language::Russian, DateStyle::Minutes) => "%d.%m.%Y, %H:%M",
        (Language::Russian, DateStyle::Seconds) => "%d.%m.%Y %H:%M:%S",
        (Language::Russian, DateStyle::Zoned) => "%d.%m.%Y %H:%M:%S %:z",
        (Language::Russian, DateStyle::ZoneName) => "%d.%m.%Y %H:%M:%S %Z",
        (Language::Czech, DateStyle::Column | DateStyle::Minutes) => "%-d. %-m. %Y %H:%M",
        (Language::Czech, DateStyle::Seconds) => "%-d. %-m. %Y %H:%M:%S",
        (Language::Czech, DateStyle::Zoned) => "%-d. %-m. %Y %H:%M:%S %:z",
        (Language::Czech, DateStyle::ZoneName) => "%-d. %-m. %Y %H:%M:%S %Z",
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = language;
        match style {
            DateStyle::Column => "%d.%m.%y %H:%M",
            DateStyle::Minutes => "%d.%m.%Y, %H:%M",
            DateStyle::Seconds => "%Y-%m-%d %H:%M:%S",
            DateStyle::Zoned => "%Y-%m-%d %H:%M:%S %:z",
            DateStyle::ZoneName => "%Y-%m-%d %H:%M:%S %Z",
        }
    }
}

pub(crate) fn integer(value: impl std::fmt::Display) -> String {
    localized_number(&value.to_string(), language())
}

pub(crate) fn decimal(value: f64, places: usize) -> String {
    localized_number(&format!("{value:.places$}"), language())
}

fn localized_number(ascii: &str, language: Language) -> String {
    if !cfg!(target_os = "macos") {
        return ascii.to_owned();
    }
    let (decimal, group) = if language == Language::English {
        ('.', ',')
    } else {
        (',', '\u{a0}')
    };
    let (whole, fraction) = ascii
        .split_once('.')
        .map_or((ascii, None), |(whole, fraction)| (whole, Some(fraction)));
    let mut formatted = String::with_capacity(ascii.len() + 4);
    let digits = whole.strip_prefix('-').unwrap_or(whole);
    if whole.starts_with('-') {
        formatted.push('-');
    }
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            formatted.push(group);
        }
        formatted.push(digit);
    }
    if let Some(fraction) = fraction {
        formatted.push(decimal);
        formatted.push_str(fraction);
    }
    formatted
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn czech_formats_dates_with_spaces_and_decimal_values_with_a_comma() {
        let moment = chrono::Utc.with_ymd_and_hms(2026, 11, 8, 14, 7, 9).unwrap();
        assert_eq!(
            moment
                .format(date_pattern(Language::Czech, DateStyle::Column))
                .to_string(),
            "8. 11. 2026 14:07"
        );
        assert_eq!(
            moment
                .format(date_pattern(Language::English, DateStyle::Minutes))
                .to_string(),
            "11/8/2026, 2:07 PM"
        );
        assert_eq!(
            localized_number("12345.67", Language::Czech),
            "12\u{a0}345,67"
        );
        assert_eq!(
            localized_number("12345.67", Language::Russian),
            "12\u{a0}345,67"
        );
        assert_eq!(localized_number("12345.67", Language::English), "12,345.67");
        assert_eq!(
            localized_number("-1234567", Language::Czech),
            "-1\u{a0}234\u{a0}567"
        );
    }
}
