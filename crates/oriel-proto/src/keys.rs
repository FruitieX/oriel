//! Keys travel as USB HID keyboard usages (page 0x07): they name physical
//! keys, so the host's keyboard layout decides which character is typed.

pub const A: u16 = 0x04;
pub const Z: u16 = 0x1D;
pub const DIGIT1: u16 = 0x1E;
pub const DIGIT0: u16 = 0x27;
pub const ENTER: u16 = 0x28;
pub const ESCAPE: u16 = 0x29;
pub const BACKSPACE: u16 = 0x2A;
pub const TAB: u16 = 0x2B;
pub const SPACE: u16 = 0x2C;
pub const MINUS: u16 = 0x2D;
pub const EQUAL: u16 = 0x2E;
pub const BRACKET_LEFT: u16 = 0x2F;
pub const BRACKET_RIGHT: u16 = 0x30;
pub const BACKSLASH: u16 = 0x31;
pub const NON_US_HASH: u16 = 0x32;
pub const SEMICOLON: u16 = 0x33;
pub const QUOTE: u16 = 0x34;
pub const BACKQUOTE: u16 = 0x35;
pub const COMMA: u16 = 0x36;
pub const PERIOD: u16 = 0x37;
pub const SLASH: u16 = 0x38;
pub const CAPS_LOCK: u16 = 0x39;
pub const F1: u16 = 0x3A;
pub const F12: u16 = 0x45;
pub const PRINT_SCREEN: u16 = 0x46;
pub const SCROLL_LOCK: u16 = 0x47;
pub const PAUSE: u16 = 0x48;
pub const INSERT: u16 = 0x49;
pub const HOME: u16 = 0x4A;
pub const PAGE_UP: u16 = 0x4B;
pub const DELETE: u16 = 0x4C;
pub const END: u16 = 0x4D;
pub const PAGE_DOWN: u16 = 0x4E;
pub const ARROW_RIGHT: u16 = 0x4F;
pub const ARROW_LEFT: u16 = 0x50;
pub const ARROW_DOWN: u16 = 0x51;
pub const ARROW_UP: u16 = 0x52;
pub const NUM_LOCK: u16 = 0x53;
pub const NUMPAD_DIVIDE: u16 = 0x54;
pub const NUMPAD_MULTIPLY: u16 = 0x55;
pub const NUMPAD_SUBTRACT: u16 = 0x56;
pub const NUMPAD_ADD: u16 = 0x57;
pub const NUMPAD_ENTER: u16 = 0x58;
pub const NUMPAD1: u16 = 0x59;
pub const NUMPAD9: u16 = 0x61;
pub const NUMPAD0: u16 = 0x62;
pub const NUMPAD_DECIMAL: u16 = 0x63;
pub const NON_US_BACKSLASH: u16 = 0x64;
pub const CONTEXT_MENU: u16 = 0x65;
pub const NUMPAD_EQUAL: u16 = 0x67;
pub const F13: u16 = 0x68;
pub const F24: u16 = 0x73;
pub const INTL_RO: u16 = 0x87;
pub const KANA_MODE: u16 = 0x88;
pub const INTL_YEN: u16 = 0x89;
pub const CONVERT: u16 = 0x8A;
pub const NON_CONVERT: u16 = 0x8B;
pub const CONTROL_LEFT: u16 = 0xE0;
pub const SHIFT_LEFT: u16 = 0xE1;
pub const ALT_LEFT: u16 = 0xE2;
pub const META_LEFT: u16 = 0xE3;
pub const CONTROL_RIGHT: u16 = 0xE4;
pub const SHIFT_RIGHT: u16 = 0xE5;
pub const ALT_RIGHT: u16 = 0xE6;
pub const META_RIGHT: u16 = 0xE7;

/// Set-1 scancode flag for keys sent with an `E0` prefix.
pub const EXTENDED: u16 = 0xE000;

/// Map a HID usage to a PC set-1 scancode (as Windows `SendInput` expects),
/// with [`EXTENDED`] set for `E0`-prefixed keys. Pause has no plain
/// scancode and returns `None`; hosts send it as a virtual key instead.
pub fn hid_to_set1(usage: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];
    const F13_24: [u16; 12] = [
        0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x76,
    ];
    const NUMPAD1_9: [u16; 9] = [0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49];
    let e = EXTENDED;
    Some(match usage {
        A..=Z => LETTERS[(usage - A) as usize],
        // 1..9 → 0x02..0x0A, 0 → 0x0B
        DIGIT1..=DIGIT0 => usage - DIGIT1 + 0x02,
        ENTER => 0x1C,
        ESCAPE => 0x01,
        BACKSPACE => 0x0E,
        TAB => 0x0F,
        SPACE => 0x39,
        MINUS => 0x0C,
        EQUAL => 0x0D,
        BRACKET_LEFT => 0x1A,
        BRACKET_RIGHT => 0x1B,
        BACKSLASH | NON_US_HASH => 0x2B,
        SEMICOLON => 0x27,
        QUOTE => 0x28,
        BACKQUOTE => 0x29,
        COMMA => 0x33,
        PERIOD => 0x34,
        SLASH => 0x35,
        CAPS_LOCK => 0x3A,
        F1..=0x43 => usage - F1 + 0x3B, // F1..F10
        0x44 => 0x57,                   // F11
        F12 => 0x58,
        PRINT_SCREEN => e | 0x37,
        SCROLL_LOCK => 0x46,
        PAUSE => return None,
        INSERT => e | 0x52,
        HOME => e | 0x47,
        PAGE_UP => e | 0x49,
        DELETE => e | 0x53,
        END => e | 0x4F,
        PAGE_DOWN => e | 0x51,
        ARROW_RIGHT => e | 0x4D,
        ARROW_LEFT => e | 0x4B,
        ARROW_DOWN => e | 0x50,
        ARROW_UP => e | 0x48,
        // SendInput needs the extended flag for NumLock, unlike the wire
        // scancode.
        NUM_LOCK => e | 0x45,
        NUMPAD_DIVIDE => e | 0x35,
        NUMPAD_MULTIPLY => 0x37,
        NUMPAD_SUBTRACT => 0x4A,
        NUMPAD_ADD => 0x4E,
        NUMPAD_ENTER => e | 0x1C,
        NUMPAD1..=NUMPAD9 => NUMPAD1_9[(usage - NUMPAD1) as usize],
        NUMPAD0 => 0x52,
        NUMPAD_DECIMAL => 0x53,
        NON_US_BACKSLASH => 0x56,
        CONTEXT_MENU => e | 0x5D,
        NUMPAD_EQUAL => 0x59,
        F13..=F24 => F13_24[(usage - F13) as usize],
        INTL_RO => 0x73,
        KANA_MODE => 0x70,
        INTL_YEN => 0x7D,
        CONVERT => 0x79,
        NON_CONVERT => 0x7B,
        CONTROL_LEFT => 0x1D,
        SHIFT_LEFT => 0x2A,
        ALT_LEFT => 0x38,
        META_LEFT => e | 0x5B,
        CONTROL_RIGHT => e | 0x1D,
        SHIFT_RIGHT => 0x36,
        ALT_RIGHT => e | 0x38,
        META_RIGHT => e | 0x5C,
        _ => return None,
    })
}

/// Map a HID usage to a Linux evdev key code (`KEY_*`).
pub fn hid_to_evdev(usage: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const NUMPAD1_9: [u16; 9] = [79, 80, 81, 75, 76, 77, 71, 72, 73];
    Some(match usage {
        A..=Z => LETTERS[(usage - A) as usize],
        DIGIT1..=DIGIT0 => usage - DIGIT1 + 2, // KEY_1 = 2 ... KEY_0 = 11
        ENTER => 28,
        ESCAPE => 1,
        BACKSPACE => 14,
        TAB => 15,
        SPACE => 57,
        MINUS => 12,
        EQUAL => 13,
        BRACKET_LEFT => 26,
        BRACKET_RIGHT => 27,
        BACKSLASH | NON_US_HASH => 43,
        SEMICOLON => 39,
        QUOTE => 40,
        BACKQUOTE => 41,
        COMMA => 51,
        PERIOD => 52,
        SLASH => 53,
        CAPS_LOCK => 58,
        F1..=0x43 => usage - F1 + 59, // F1..F10 = 59..68
        0x44 => 87,                   // F11
        F12 => 88,
        PRINT_SCREEN => 99,
        SCROLL_LOCK => 70,
        PAUSE => 119,
        INSERT => 110,
        HOME => 102,
        PAGE_UP => 104,
        DELETE => 111,
        END => 107,
        PAGE_DOWN => 109,
        ARROW_RIGHT => 106,
        ARROW_LEFT => 105,
        ARROW_DOWN => 108,
        ARROW_UP => 103,
        NUM_LOCK => 69,
        NUMPAD_DIVIDE => 98,
        NUMPAD_MULTIPLY => 55,
        NUMPAD_SUBTRACT => 74,
        NUMPAD_ADD => 78,
        NUMPAD_ENTER => 96,
        NUMPAD1..=NUMPAD9 => NUMPAD1_9[(usage - NUMPAD1) as usize],
        NUMPAD0 => 82,
        NUMPAD_DECIMAL => 83,
        NON_US_BACKSLASH => 86,
        CONTEXT_MENU => 127,
        NUMPAD_EQUAL => 117,
        F13..=F24 => usage - F13 + 183,
        INTL_RO => 89,
        KANA_MODE => 93,
        INTL_YEN => 124,
        CONVERT => 92,
        NON_CONVERT => 94,
        CONTROL_LEFT => 29,
        SHIFT_LEFT => 42,
        ALT_LEFT => 56,
        META_LEFT => 125,
        CONTROL_RIGHT => 97,
        SHIFT_RIGHT => 54,
        ALT_RIGHT => 100,
        META_RIGHT => 126,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_checks() {
        assert_eq!(hid_to_set1(A), Some(0x1E));
        assert_eq!(hid_to_set1(Z), Some(0x2C));
        assert_eq!(hid_to_set1(DIGIT1), Some(0x02));
        assert_eq!(hid_to_set1(DIGIT0), Some(0x0B));
        assert_eq!(hid_to_set1(F1 + 9), Some(0x44)); // F10
        assert_eq!(hid_to_set1(F12), Some(0x58));
        assert_eq!(hid_to_set1(ARROW_UP), Some(EXTENDED | 0x48));
        assert_eq!(hid_to_set1(NUMPAD1 + 4), Some(0x4C)); // numpad 5
        assert_eq!(hid_to_set1(F24), Some(0x76));
        assert_eq!(hid_to_set1(PAUSE), None);
    }

    #[test]
    fn evdev_matches_set1_for_plain_keys() {
        // Classic evdev codes equal set-1 scancodes for non-extended keys.
        for usage in
            (A..=Z)
                .chain(DIGIT1..=DIGIT0)
                .chain([ENTER, SPACE, F1, CONTROL_LEFT, SHIFT_RIGHT])
        {
            let set1 = hid_to_set1(usage).unwrap();
            assert_eq!(set1 & EXTENDED, 0);
            assert_eq!(hid_to_evdev(usage), Some(set1), "usage {usage:#x}");
        }
        assert_eq!(hid_to_evdev(ARROW_UP), Some(103));
        assert_eq!(hid_to_evdev(META_LEFT), Some(125));
    }
}
