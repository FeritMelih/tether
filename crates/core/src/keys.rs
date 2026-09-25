//! Keys by name, as a terminal would send them: `keys ["Down", "Enter"]` rather than escape
//! sequences a caller has to get right. Arrows and Home/End follow the program's cursor-key
//! mode, which is why this lives beside the screen model and not in a client. Pasting is
//! here too: bracketed when the program asked for it, with line breaks as a terminal sends
//! them.

/// The bytes for one named key; `None` for a name that is not a key.
pub fn encode(name: &str, app_cursor: bool) -> Option<Vec<u8>> {
    let mut rest = name;
    let (mut shift, mut alt, mut ctrl) = (false, false, false);
    // Modifier prefixes, but a bare "-" or "C-" as a whole name is not a prefix.
    loop {
        let lower = rest.to_ascii_lowercase();
        if rest.len() > 2 && lower.starts_with("s-") {
            shift = true;
        } else if rest.len() > 2 && lower.starts_with("m-") {
            alt = true;
        } else if rest.len() > 2 && lower.starts_with("c-") {
            ctrl = true;
        } else {
            break;
        }
        rest = &rest[2..];
    }
    let key = rest;
    let lower = key.to_ascii_lowercase();
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);

    // Cursor keys take modifiers as a parameter.
    let cursor = match lower.as_str() {
        "up" => Some(b'A'),
        "down" => Some(b'B'),
        "right" => Some(b'C'),
        "left" => Some(b'D'),
        "home" => Some(b'H'),
        "end" => Some(b'F'),
        _ => None,
    };
    if let Some(fin) = cursor {
        if modifier > 1 {
            return Some(format!("\x1b[1;{modifier}{}", fin as char).into_bytes());
        }
        return Some(if app_cursor {
            vec![0x1b, b'O', fin]
        } else {
            vec![0x1b, b'[', fin]
        });
    }

    let mut bytes: Vec<u8> = match lower.as_str() {
        "enter" | "return" => b"\r".to_vec(),
        "tab" if shift => {
            shift = false;
            b"\x1b[Z".to_vec()
        }
        "tab" => b"\t".to_vec(),
        "backtab" => b"\x1b[Z".to_vec(),
        "esc" | "escape" => b"\x1b".to_vec(),
        "backspace" | "bs" => b"\x7f".to_vec(),
        "space" if ctrl => {
            ctrl = false;
            vec![0]
        }
        "space" => b" ".to_vec(),
        "pageup" | "pgup" => b"\x1b[5~".to_vec(),
        "pagedown" | "pgdn" => b"\x1b[6~".to_vec(),
        "insert" | "ins" => b"\x1b[2~".to_vec(),
        "delete" | "del" => b"\x1b[3~".to_vec(),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => b"\x1b[15~".to_vec(),
        "f6" => b"\x1b[17~".to_vec(),
        "f7" => b"\x1b[18~".to_vec(),
        "f8" => b"\x1b[19~".to_vec(),
        "f9" => b"\x1b[20~".to_vec(),
        "f10" => b"\x1b[21~".to_vec(),
        "f11" => b"\x1b[23~".to_vec(),
        "f12" => b"\x1b[24~".to_vec(),
        _ => {
            let mut chars = key.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if ctrl {
                ctrl = false;
                vec![control_byte(c)?]
            } else {
                let mut b = [0u8; 4];
                c.encode_utf8(&mut b).as_bytes().to_vec()
            }
        }
    };
    if ctrl || shift {
        // A modifier this key has no encoding for.
        return None;
    }
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

fn control_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '/' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// Several keys, or the name that is not a key.
pub fn encode_all(names: &[String], app_cursor: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for n in names {
        out.extend(encode(n, app_cursor).ok_or_else(|| format!("not a key: {n:?}"))?);
    }
    Ok(out)
}

/// Text as a terminal pastes it: line breaks as CR, wrapped in the bracketed-paste markers
/// when `bracketed`, and any end marker inside the text taken out so it cannot end the paste.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text
        .replace("\r\n", "\r")
        .replace('\n', "\r")
        .replace("\x1b[201~", "");
    if !bracketed {
        return body.into_bytes();
    }
    let mut out = b"\x1b[200~".to_vec();
    out.extend(body.as_bytes());
    out.extend(b"\x1b[201~");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(n: &str) -> Vec<u8> {
        encode(n, false).unwrap_or_else(|| panic!("{n} is a key"))
    }

    #[test]
    fn names() {
        assert_eq!(k("Enter"), b"\r");
        assert_eq!(k("S-Tab"), b"\x1b[Z");
        assert_eq!(k("esc"), b"\x1b");
        assert_eq!(k("Down"), b"\x1b[B");
        assert_eq!(encode("Down", true).unwrap(), b"\x1bOB");
        assert_eq!(k("C-c"), [3]);
        assert_eq!(k("C-]"), [0x1d]);
        assert_eq!(k("C-Space"), [0]);
        assert_eq!(k("M-x"), b"\x1bx");
        assert_eq!(k("C-Up"), b"\x1b[1;5A");
        assert_eq!(k("S-M-Left"), b"\x1b[1;4D");
        assert_eq!(k("1"), b"1");
        assert_eq!(k("é"), "é".as_bytes());
        assert_eq!(k("F5"), b"\x1b[15~");
        assert_eq!(k("-"), b"-");
        assert_eq!(encode("Nope", false), None);
        assert_eq!(encode("C-F5", false), None);
        assert!(encode_all(&["Down".into(), "bogus".into()], false).is_err());
    }

    #[test]
    fn pastes() {
        assert_eq!(paste("a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(paste("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~");
    }
}
