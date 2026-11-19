mod view;
mod worker;

pub(super) use view::Document;
#[cfg(test)]
pub(crate) use worker::check_sftp_hex;

const BYTES_PER_ROW: usize = 16;
const PAGE_BYTES: u64 = 4096;
const PATTERN_LIMIT: usize = 256;

fn parse_offset(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    digits
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit())
        .then(|| u64::from_str_radix(digits, 16).ok())
        .flatten()
}

fn parse_pattern(text: &str) -> Option<Vec<u8>> {
    let mut digits = text.bytes().filter(|byte| !byte.is_ascii_whitespace());
    let mut result = Vec::new();
    while let Some(high) = digits.next() {
        if result.len() == PATTERN_LIMIT {
            return None;
        }
        let high = char::from(high).to_digit(16)?;
        let low = char::from(digits.next()?).to_digit(16)?;
        result.push((high * 16 + low) as u8);
    }
    (!result.is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_offsets_and_complete_byte_pairs() {
        assert_eq!(parse_offset(" 0x100000010 "), Some(0x1_0000_0010));
        assert_eq!(parse_offset("ffffffffffffffff"), Some(u64::MAX));
        for invalid in ["", "0x", "+20", " 8 0", "10000000000000000"] {
            assert_eq!(parse_offset(invalid), None);
        }
        assert_eq!(
            parse_pattern("de AD\tbe\nef"),
            Some(vec![0xde, 0xad, 0xbe, 0xef])
        );
        assert_eq!(
            parse_pattern("deADbeef"),
            Some(vec![0xde, 0xad, 0xbe, 0xef])
        );
        for invalid in ["   ", "ADC", "01 _2", "0x41", "Ж0"] {
            assert_eq!(parse_pattern(invalid), None);
        }
        assert_eq!(
            parse_pattern(&"00".repeat(PATTERN_LIMIT)).unwrap().len(),
            PATTERN_LIMIT
        );
        assert_eq!(parse_pattern(&"ab".repeat(PATTERN_LIMIT + 1)), None);
    }
}
