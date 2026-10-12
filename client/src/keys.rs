//! Keyboard translation from egui to X11 keysyms, which is what RFB key events carry.

use egui::Key;

pub const SHIFT_L: u32 = 0xffe1;
pub const CONTROL_L: u32 = 0xffe3;
pub const ALT_L: u32 = 0xffe9;
pub const DELETE: u32 = 0xffff;
pub const INSERT: u32 = 0xff63;

/// Keys that type a character. Without Ctrl/Alt/Cmd these are sent from the
/// text egui reports (which follows the local keyboard layout); with them, from
/// the key itself so shortcuts like Ctrl+C work.
pub fn is_text_key(key: Key) -> bool {
    use Key::*;
    matches!(
        key,
        Space
            | Colon
            | Comma
            | Backslash
            | Slash
            | Pipe
            | Questionmark
            | Exclamationmark
            | OpenBracket
            | CloseBracket
            | OpenCurlyBracket
            | CloseCurlyBracket
            | Backtick
            | Minus
            | Period
            | Plus
            | Equals
            | Semicolon
            | Quote
            | IntlBackslash
            | Num0
            | Num1
            | Num2
            | Num3
            | Num4
            | Num5
            | Num6
            | Num7
            | Num8
            | Num9
    ) || letter(key).is_some()
}

fn letter(key: Key) -> Option<u8> {
    use Key::*;
    let all = [A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q, R, S, T, U, V, W, X, Y, Z];
    all.iter().position(|&k| k == key).map(|i| b'a' + i as u8)
}

/// The keysym for a key event. `shift` picks upper-case letters, matching what
/// a local X server would report for Ctrl+Shift+T and similar.
pub fn keysym(key: Key, shift: bool) -> Option<u32> {
    use Key::*;
    if let Some(c) = letter(key) {
        return Some(if shift { c.to_ascii_uppercase() } else { c } as u32);
    }
    let f = |n: u32| 0xffbe + n - 1;
    Some(match key {
        ArrowLeft => 0xff51,
        ArrowUp => 0xff52,
        ArrowRight => 0xff53,
        ArrowDown => 0xff54,
        Escape => 0xff1b,
        Tab => 0xff09,
        Backspace => 0xff08,
        Enter => 0xff0d,
        Space => 0x20,
        Insert => INSERT,
        Delete => DELETE,
        Home => 0xff50,
        End => 0xff57,
        PageUp => 0xff55,
        PageDown => 0xff56,
        Copy => 0x1008ff57,
        Cut => 0x1008ff58,
        Paste => 0x1008ff6d,
        BrowserBack => 0x1008ff26,
        Colon => ':' as u32,
        Comma => ',' as u32,
        Backslash | IntlBackslash => '\\' as u32,
        Slash => '/' as u32,
        Pipe => '|' as u32,
        Questionmark => '?' as u32,
        Exclamationmark => '!' as u32,
        OpenBracket => '[' as u32,
        CloseBracket => ']' as u32,
        OpenCurlyBracket => '{' as u32,
        CloseCurlyBracket => '}' as u32,
        Backtick => '`' as u32,
        Minus => '-' as u32,
        Period => '.' as u32,
        Plus => '+' as u32,
        Equals => '=' as u32,
        Semicolon => ';' as u32,
        Quote => '\'' as u32,
        Num0 => '0' as u32,
        Num1 => '1' as u32,
        Num2 => '2' as u32,
        Num3 => '3' as u32,
        Num4 => '4' as u32,
        Num5 => '5' as u32,
        Num6 => '6' as u32,
        Num7 => '7' as u32,
        Num8 => '8' as u32,
        Num9 => '9' as u32,
        F1 => f(1),
        F2 => f(2),
        F3 => f(3),
        F4 => f(4),
        F5 => f(5),
        F6 => f(6),
        F7 => f(7),
        F8 => f(8),
        F9 => f(9),
        F10 => f(10),
        F11 => f(11),
        F12 => f(12),
        F13 => f(13),
        F14 => f(14),
        F15 => f(15),
        F16 => f(16),
        F17 => f(17),
        F18 => f(18),
        F19 => f(19),
        F20 => f(20),
        F21 => f(21),
        F22 => f(22),
        F23 => f(23),
        F24 => f(24),
        F25 => f(25),
        F26 => f(26),
        F27 => f(27),
        F28 => f(28),
        F29 => f(29),
        F30 => f(30),
        F31 => f(31),
        F32 => f(32),
        F33 => f(33),
        F34 => f(34),
        F35 => f(35),
        ShiftLeft => SHIFT_L,
        ShiftRight => 0xffe2,
        ControlLeft => CONTROL_L,
        ControlRight => 0xffe4,
        AltLeft => ALT_L,
        AltRight => 0xffea,
        SuperLeft => 0xffeb,
        SuperRight => 0xffec,
        _ => return None,
    })
}

/// The keysym for a typed character: Latin-1 maps directly, everything else
/// uses the Unicode keysym range.
pub fn char_keysym(c: char) -> u32 {
    let u = c as u32;
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        _ if (0x20..=0x7e).contains(&u) || (0xa0..=0xff).contains(&u) => u,
        _ => 0x0100_0000 | u,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_follow_shift() {
        assert_eq!(keysym(Key::A, false), Some('a' as u32));
        assert_eq!(keysym(Key::T, true), Some('T' as u32));
    }

    #[test]
    fn special_keys() {
        assert_eq!(keysym(Key::Enter, false), Some(0xff0d));
        assert_eq!(keysym(Key::F1, false), Some(0xffbe));
        assert_eq!(keysym(Key::F12, false), Some(0xffc9));
        assert_eq!(keysym(Key::F35, false), Some(0xffe0));
        assert_eq!(keysym(Key::ControlRight, false), Some(0xffe4));
    }

    #[test]
    fn characters() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('€'), 0x0100_20ac);
        assert_eq!(char_keysym('\n'), 0xff0d);
    }

    #[test]
    fn text_keys() {
        assert!(is_text_key(Key::Q));
        assert!(is_text_key(Key::Num5));
        assert!(is_text_key(Key::Space));
        assert!(!is_text_key(Key::Enter));
        assert!(!is_text_key(Key::F5));
        assert!(!is_text_key(Key::ShiftLeft));
    }
}
