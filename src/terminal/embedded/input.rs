use alacritty_terminal::term::TermMode;
use eframe::egui::{Key, Modifiers};

pub(super) fn key_bytes(key: Key, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    if mods.mac_cmd {
        return None;
    }
    let modifier =
        1 + usize::from(mods.shift) + 2 * usize::from(mods.alt) + 4 * usize::from(mods.ctrl);
    let cursor = match key {
        Key::ArrowUp => Some('A'),
        Key::ArrowDown => Some('B'),
        Key::ArrowRight => Some('C'),
        Key::ArrowLeft => Some('D'),
        Key::Home => Some('H'),
        Key::End => Some('F'),
        Key::F1 => Some('P'),
        Key::F2 => Some('Q'),
        Key::F3 => Some('R'),
        Key::F4 => Some('S'),
        _ => None,
    };
    if let Some(suffix) = cursor {
        let command = if modifier > 1 {
            format!("\x1b[1;{modifier}{suffix}")
        } else if matches!(key, Key::F1 | Key::F2 | Key::F3 | Key::F4)
            || mode.contains(TermMode::APP_CURSOR)
        {
            format!("\x1bO{suffix}")
        } else {
            format!("\x1b[{suffix}")
        };
        return Some(command.into_bytes());
    }
    let edit = match key {
        Key::Insert => 2,
        Key::Delete => 3,
        Key::PageUp => 5,
        Key::PageDown => 6,
        Key::F5 => 15,
        Key::F6 => 17,
        Key::F7 => 18,
        Key::F8 => 19,
        Key::F9 => 20,
        Key::F10 => 21,
        Key::F11 => 23,
        Key::F12 => 24,
        _ => 0,
    };
    if edit != 0 {
        return Some(
            if modifier == 1 {
                format!("\x1b[{edit}~")
            } else {
                format!("\x1b[{edit};{modifier}~")
            }
            .into_bytes(),
        );
    }
    if key == Key::Tab && mods.shift {
        return Some(b"\x1b[Z".to_vec());
    }
    let byte = match key {
        Key::Enter => b'\r',
        Key::Escape => 0x1b,
        Key::Tab => b'\t',
        Key::Backspace if mods.ctrl => 0x08,
        Key::Backspace => 0x7f,
        _ if mods.ctrl => control_byte(key)?,
        _ => return None,
    };
    Some(if mods.alt {
        vec![0x1b, byte]
    } else {
        vec![byte]
    })
}

fn control_byte(key: Key) -> Option<u8> {
    if let [letter @ b'A'..=b'Z'] = key.name().as_bytes() {
        return Some(letter & 0x1f);
    }
    Some(match key {
        Key::Space | Key::Num2 => 0,
        Key::OpenBracket | Key::Num3 => 0x1b,
        Key::Backslash | Key::Num4 => 0x1c,
        Key::CloseBracket | Key::Num5 => 0x1d,
        Key::Num6 => 0x1e,
        Key::Slash | Key::Minus | Key::Num7 => 0x1f,
        Key::Num8 => 0x7f,
        _ => return None,
    })
}

pub(super) fn paste(text: &str, mode: TermMode) -> Vec<u8> {
    let bracketed = mode.contains(TermMode::BRACKETED_PASTE);
    let mut bytes = Vec::with_capacity(text.len() + if bracketed { 12 } else { 0 });
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    let mut previous_cr = false;
    for byte in text.bytes() {
        match byte {
            b'\n' if previous_cr => {}
            b'\r' | b'\n' => bytes.push(if bracketed { b'\n' } else { b'\r' }),
            b'\t' | 0x20..=0x7e | 0x80..=0xff => bytes.push(byte),
            _ => {}
        }
        previous_cr = byte == b'\r';
    }
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    bytes
}
