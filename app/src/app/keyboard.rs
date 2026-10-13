use slint::platform::Key;
pub fn encode(text: &str, control: bool, shift: bool, alt: bool, mode: u32) -> Vec<u8> {
    let mut chars = text.chars();
    let Some(ch) = chars.next() else {
        return Vec::new();
    };
    let key = |key: Key| ch == char::from(key);
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(control);
    let arrow = if key(Key::UpArrow) {
        Some('A')
    } else if key(Key::DownArrow) {
        Some('B')
    } else if key(Key::RightArrow) {
        Some('C')
    } else if key(Key::LeftArrow) {
        Some('D')
    } else if key(Key::Home) {
        Some('H')
    } else if key(Key::End) {
        Some('F')
    } else {
        None
    };
    let mut bytes = if let Some(code) = arrow {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{code}").into_bytes()
        } else if mode & 2 != 0 {
            format!("\x1bO{code}").into_bytes()
        } else {
            format!("\x1b[{code}").into_bytes()
        }
    } else if ('\u{f704}'..='\u{f707}').contains(&ch) {
        let code = char::from(b'P' + (ch as u32 - 0xf704) as u8);
        if modifier > 1 {
            format!("\x1b[1;{modifier}{code}").into_bytes()
        } else {
            format!("\x1bO{code}").into_bytes()
        }
    } else if ('\u{f708}'..='\u{f70f}').contains(&ch) {
        let code = [15, 17, 18, 19, 20, 21, 23, 24][(ch as u32 - 0xf708) as usize];
        if modifier > 1 {
            format!("\x1b[{code};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    } else if key(Key::Return) {
        vec![b'\r']
    } else if key(Key::Backspace) {
        vec![127]
    } else if key(Key::Tab) {
        if shift {
            b"\x1b[Z".to_vec()
        } else {
            vec![b'\t']
        }
    } else if key(Key::Escape) {
        vec![27]
    } else if key(Key::Delete) {
        b"\x1b[3~".to_vec()
    } else if key(Key::Insert) {
        b"\x1b[2~".to_vec()
    } else if key(Key::PageUp) {
        b"\x1b[5~".to_vec()
    } else if key(Key::PageDown) {
        b"\x1b[6~".to_vec()
    } else if control && !alt && ch.is_ascii() {
        let c = ch.to_ascii_uppercase() as u8;
        if (b'@'..=b'_').contains(&c) {
            vec![c & 31]
        } else if ch == ' ' {
            vec![0]
        } else {
            Vec::new()
        }
    } else if ('\u{f700}'..='\u{f8ff}').contains(&ch) || ch.is_control() {
        Vec::new()
    } else {
        text.as_bytes().to_vec()
    };
    if alt
        && !control
        && arrow.is_none()
        && !(('\u{f704}'..='\u{f70f}').contains(&ch))
        && !bytes.is_empty()
    {
        bytes.insert(0, 27);
    }
    bytes
}
