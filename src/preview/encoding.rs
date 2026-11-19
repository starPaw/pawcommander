use encoding_rs::{Decoder, DecoderResult, Encoding};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Codec {
    #[default]
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1250,
    Windows1251,
    Windows1252,
    Koi8R,
    Cp866,
    Iso8859_2,
    Iso8859_5,
}

impl Codec {
    pub(super) const ALL: [Self; 10] = [
        Self::Utf8,
        Self::Utf16Le,
        Self::Utf16Be,
        Self::Windows1250,
        Self::Windows1251,
        Self::Windows1252,
        Self::Koi8R,
        Self::Cp866,
        Self::Iso8859_2,
        Self::Iso8859_5,
    ];

    pub(super) fn choose(manual: Option<Self>, prefix: &[u8]) -> Self {
        manual.unwrap_or(match Encoding::for_bom(prefix) {
            Some((codec, _)) if codec == encoding_rs::UTF_16LE => Self::Utf16Le,
            Some((codec, _)) if codec == encoding_rs::UTF_16BE => Self::Utf16Be,
            _ => Self::Utf8,
        })
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Cp866 => "CP866",
            _ => self.engine().name(),
        }
    }

    fn engine(self) -> &'static Encoding {
        match self {
            Self::Utf8 => encoding_rs::UTF_8,
            Self::Utf16Le => encoding_rs::UTF_16LE,
            Self::Utf16Be => encoding_rs::UTF_16BE,
            Self::Windows1250 => encoding_rs::WINDOWS_1250,
            Self::Windows1251 => encoding_rs::WINDOWS_1251,
            Self::Windows1252 => encoding_rs::WINDOWS_1252,
            Self::Koi8R => encoding_rs::KOI8_R,
            Self::Cp866 => encoding_rs::IBM866,
            Self::Iso8859_2 => encoding_rs::ISO_8859_2,
            Self::Iso8859_5 => encoding_rs::ISO_8859_5,
        }
    }

    pub(super) fn bom(self, input: &[u8]) -> usize {
        Encoding::for_bom(input)
            .filter(|(codec, _)| *codec == self.engine())
            .map_or(0, |(_, size)| size)
    }

    pub(super) fn width(self) -> usize {
        if matches!(self, Self::Utf16Le | Self::Utf16Be) {
            2
        } else {
            1
        }
    }

    pub(super) fn unit(self, bytes: &[u8]) -> u16 {
        let word = [bytes[0], bytes[1]];
        if self == Self::Utf16Be {
            u16::from_be_bytes(word)
        } else {
            u16::from_le_bytes(word)
        }
    }

    pub(super) fn raw_len(self, text: &str) -> usize {
        match self {
            Self::Utf8 => text.len(),
            Self::Utf16Le | Self::Utf16Be => text.encode_utf16().count() * 2,
            _ => text.chars().count(),
        }
    }

    /// Positions after line breaks in a slice that starts on a code unit.
    pub(super) fn line_breaks(self, bytes: &[u8]) -> impl DoubleEndedIterator<Item = usize> {
        memchr::memchr_iter(b'\n', bytes).filter_map(move |index| match self {
            Self::Utf16Le => {
                (index.is_multiple_of(2) && bytes.get(index + 1) == Some(&0)).then_some(index + 2)
            }
            Self::Utf16Be => {
                (!index.is_multiple_of(2) && bytes[index - 1] == 0).then_some(index + 1)
            }
            _ => Some(index + 1),
        })
    }

    pub(super) fn decoder(self) -> Decoder {
        self.engine().new_decoder_without_bom_handling()
    }

    pub(super) fn decode(self, bytes: &[u8]) -> Option<Cow<'_, str>> {
        self.engine()
            .decode_without_bom_handling_and_without_replacement(bytes)
    }

    pub(super) fn prefix(self, bytes: &[u8], truncated: bool) -> Option<String> {
        let bytes = &bytes[self.bom(bytes)..];
        if !truncated {
            return self.decode(bytes).map(Cow::into_owned);
        }
        let mut decoder = self.decoder();
        let mut text =
            String::with_capacity(decoder.max_utf8_buffer_length_without_replacement(bytes.len())?);
        let (status, read) = decoder.decode_to_string_without_replacement(bytes, &mut text, false);
        (status == DecoderResult::InputEmpty && read == bytes.len()).then_some(text)
    }

    pub(super) fn encode(self, text: &str) -> Option<Cow<'_, [u8]>> {
        match self {
            Self::Utf8 => Some(Cow::Borrowed(text.as_bytes())),
            Self::Utf16Le => Some(Cow::Owned(
                text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            )),
            Self::Utf16Be => Some(Cow::Owned(
                text.encode_utf16().flat_map(u16::to_be_bytes).collect(),
            )),
            _ => {
                let (bytes, _, errors) = self.engine().encode(text);
                (!errors).then_some(bytes)
            }
        }
    }

    pub(super) fn error(self) -> String {
        crate::tr_format!(
            "Не удалось показать текст в {}. Выберите кодировку.",
            "Could not show text in {}. Choose an encoding.",
            self.name()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_encodings_decode_known_byte_sequences() {
        let fixtures = [
            (
                Codec::Windows1250,
                b"\x50\xf8\xed\x6c\x69\x9a".as_slice(),
                "Příliš",
            ),
            (Codec::Windows1251, b"\xcf\xf0\xe8\xe2\xe5\xf2", "Привет"),
            (Codec::Windows1252, b"Caf\xe9 \x80", "Café €"),
            (Codec::Koi8R, b"\xf0\xd2\xc9\xd7\xc5\xd4", "Привет"),
            (Codec::Cp866, b"\x8f\xe0\xa8\xa2\xa5\xe2", "Привет"),
            (Codec::Iso8859_2, b"\xa3\xf3\x64\xbc", "Łódź"),
            (Codec::Iso8859_5, b"\xbf\xe0\xd8\xd2\xd5\xe2", "Привет"),
        ];
        for (encoding, bytes, expected) in fixtures {
            assert_eq!(encoding.prefix(bytes, false).as_deref(), Some(expected));
            assert_eq!(&*encoding.encode(expected).unwrap(), bytes);
        }
    }

    #[test]
    fn detects_both_utf16_boms_and_preserves_explicit_choice_and_partial_prefix() {
        let little = b"\xff\xfeA\x00\x3d\xd8";
        let big = b"\xfe\xff\x00A\xd8\x3d\xde\x42";
        assert_eq!(Codec::choose(None, little), Codec::Utf16Le);
        assert_eq!(Codec::choose(None, big), Codec::Utf16Be);
        assert_eq!(
            Codec::choose(Some(Codec::Windows1251), little),
            Codec::Windows1251
        );
        assert_eq!(Codec::Utf16Le.prefix(little, true).as_deref(), Some("A"));
        assert!(Codec::Utf16Le.prefix(little, false).is_none());
        assert_eq!(Codec::Utf16Be.prefix(big, false).as_deref(), Some("A🙂"));
    }
}
