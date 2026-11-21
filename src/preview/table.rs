mod records;
mod view;
mod worker;

use super::encoding::Codec;
pub(super) use view::Document;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Separator {
    Comma,
    Semicolon,
    Tab,
}

impl Separator {
    pub(super) fn from_path(path: &std::path::Path) -> Option<Self> {
        let extension = path.extension()?;
        if extension.eq_ignore_ascii_case("csv") {
            Some(Self::Comma)
        } else if extension.eq_ignore_ascii_case("tsv") {
            Some(Self::Tab)
        } else {
            None
        }
    }

    const ALL: [Self; 3] = [Self::Comma, Self::Semicolon, Self::Tab];

    fn byte(self) -> u8 {
        match self {
            Self::Comma => b',',
            Self::Semicolon => b';',
            Self::Tab => b'\t',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Comma => crate::i18n::text!("Запятая"),
            Self::Semicolon => crate::i18n::text!("Точка с запятой"),
            Self::Tab => crate::i18n::text!("Табуляция"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Position {
    byte: u64,
    records: u64,
}

struct Row {
    number: u64,
    cells: Vec<String>,
}
