//! winit physical keys → USB HID usages.

use winit::keyboard::KeyCode;

pub fn hid_usage(key: KeyCode) -> Option<u16> {
    use KeyCode::*;
    Some(match key {
        KeyA => 0x04,
        KeyB => 0x05,
        KeyC => 0x06,
        KeyD => 0x07,
        KeyE => 0x08,
        KeyF => 0x09,
        KeyG => 0x0A,
        KeyH => 0x0B,
        KeyI => 0x0C,
        KeyJ => 0x0D,
        KeyK => 0x0E,
        KeyL => 0x0F,
        KeyM => 0x10,
        KeyN => 0x11,
        KeyO => 0x12,
        KeyP => 0x13,
        KeyQ => 0x14,
        KeyR => 0x15,
        KeyS => 0x16,
        KeyT => 0x17,
        KeyU => 0x18,
        KeyV => 0x19,
        KeyW => 0x1A,
        KeyX => 0x1B,
        KeyY => 0x1C,
        KeyZ => 0x1D,
        Digit1 => 0x1E,
        Digit2 => 0x1F,
        Digit3 => 0x20,
        Digit4 => 0x21,
        Digit5 => 0x22,
        Digit6 => 0x23,
        Digit7 => 0x24,
        Digit8 => 0x25,
        Digit9 => 0x26,
        Digit0 => 0x27,
        Enter => 0x28,
        Escape => 0x29,
        Backspace => 0x2A,
        Tab => 0x2B,
        Space => 0x2C,
        Minus => 0x2D,
        Equal => 0x2E,
        BracketLeft => 0x2F,
        BracketRight => 0x30,
        Backslash => 0x31,
        Semicolon => 0x33,
        Quote => 0x34,
        Backquote => 0x35,
        Comma => 0x36,
        Period => 0x37,
        Slash => 0x38,
        CapsLock => 0x39,
        F1 => 0x3A,
        F2 => 0x3B,
        F3 => 0x3C,
        F4 => 0x3D,
        F5 => 0x3E,
        F6 => 0x3F,
        F7 => 0x40,
        F8 => 0x41,
        F9 => 0x42,
        F10 => 0x43,
        F11 => 0x44,
        F12 => 0x45,
        PrintScreen => 0x46,
        ScrollLock => 0x47,
        Pause => 0x48,
        Insert => 0x49,
        Home => 0x4A,
        PageUp => 0x4B,
        Delete => 0x4C,
        End => 0x4D,
        PageDown => 0x4E,
        ArrowRight => 0x4F,
        ArrowLeft => 0x50,
        ArrowDown => 0x51,
        ArrowUp => 0x52,
        NumLock => 0x53,
        NumpadDivide => 0x54,
        NumpadMultiply => 0x55,
        NumpadSubtract => 0x56,
        NumpadAdd => 0x57,
        NumpadEnter => 0x58,
        Numpad1 => 0x59,
        Numpad2 => 0x5A,
        Numpad3 => 0x5B,
        Numpad4 => 0x5C,
        Numpad5 => 0x5D,
        Numpad6 => 0x5E,
        Numpad7 => 0x5F,
        Numpad8 => 0x60,
        Numpad9 => 0x61,
        Numpad0 => 0x62,
        NumpadDecimal => 0x63,
        IntlBackslash => 0x64,
        ContextMenu => 0x65,
        NumpadEqual => 0x67,
        F13 => 0x68,
        F14 => 0x69,
        F15 => 0x6A,
        F16 => 0x6B,
        F17 => 0x6C,
        F18 => 0x6D,
        F19 => 0x6E,
        F20 => 0x6F,
        F21 => 0x70,
        F22 => 0x71,
        F23 => 0x72,
        F24 => 0x73,
        IntlRo => 0x87,
        KanaMode => 0x88,
        IntlYen => 0x89,
        Convert => 0x8A,
        NonConvert => 0x8B,
        ControlLeft => 0xE0,
        ShiftLeft => 0xE1,
        AltLeft => 0xE2,
        SuperLeft => 0xE3,
        ControlRight => 0xE4,
        ShiftRight => 0xE5,
        AltRight => 0xE6,
        SuperRight => 0xE7,
        _ => return None,
    })
}

/// A key combination such as `Ctrl+Alt+Shift+M`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
    pub key: KeyCode,
}

impl Shortcut {
    pub fn parse(s: &str) -> Option<Self> {
        let mut sc = Shortcut {
            ctrl: false,
            alt: false,
            shift: false,
            logo: false,
            key: KeyCode::F24,
        };
        let mut key = None;
        for part in s.split('+').map(str::trim) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => sc.ctrl = true,
                "alt" | "option" => sc.alt = true,
                "shift" => sc.shift = true,
                "super" | "meta" | "win" | "cmd" | "logo" => sc.logo = true,
                other => key = Some(parse_key(other)?),
            }
        }
        sc.key = key?;
        Some(sc)
    }

    /// Match against HID usages: `usage` was just pressed while `held` are
    /// down.
    pub fn matches_hid(&self, held: &std::collections::HashSet<u16>, usage: u16) -> bool {
        use oriel_proto::keys::*;
        let any = |a, b| held.contains(&a) || held.contains(&b);
        hid_usage(self.key) == Some(usage)
            && any(CONTROL_LEFT, CONTROL_RIGHT) == self.ctrl
            && any(ALT_LEFT, ALT_RIGHT) == self.alt
            && any(SHIFT_LEFT, SHIFT_RIGHT) == self.shift
            && any(META_LEFT, META_RIGHT) == self.logo
    }

    pub fn matches(&self, mods: winit::keyboard::ModifiersState, key: KeyCode) -> bool {
        key == self.key
            && mods.control_key() == self.ctrl
            && mods.alt_key() == self.alt
            && mods.shift_key() == self.shift
            && mods.super_key() == self.logo
    }
}

impl std::fmt::Display for Shortcut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let logo = if cfg!(target_os = "macos") {
            "Cmd+"
        } else {
            "Super+"
        };
        let key = format!("{:?}", self.key);
        let key = key
            .strip_prefix("Key")
            .or_else(|| key.strip_prefix("Digit"))
            .unwrap_or(&key);
        write!(
            f,
            "{}{}{}{}{key}",
            if self.ctrl { "Ctrl+" } else { "" },
            if self.alt { "Alt+" } else { "" },
            if self.shift { "Shift+" } else { "" },
            if self.logo { logo } else { "" },
        )
    }
}

fn parse_key(name: &str) -> Option<KeyCode> {
    use KeyCode::*;
    let upper = name.to_ascii_uppercase();
    if upper.len() == 1 {
        let c = upper.as_bytes()[0];
        const LETTERS: [KeyCode; 26] = [
            KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN,
            KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
        ];
        const DIGITS: [KeyCode; 10] = [
            Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
        ];
        return match c {
            b'A'..=b'Z' => Some(LETTERS[(c - b'A') as usize]),
            b'0'..=b'9' => Some(DIGITS[(c - b'0') as usize]),
            _ => None,
        };
    }
    if let Some(n) = upper.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
        const FN: [KeyCode; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
        return FN.get(n.checked_sub(1)? as usize).copied();
    }
    Some(match upper.as_str() {
        "ESC" | "ESCAPE" => Escape,
        "HOME" => Home,
        "END" => End,
        "INSERT" => Insert,
        "DELETE" => Delete,
        "PAUSE" => Pause,
        "SPACE" => Space,
        "TAB" => Tab,
        "ENTER" => Enter,
        "BACKSPACE" => Backspace,
        "SCROLLLOCK" => ScrollLock,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_roundtrip() {
        let s = Shortcut::parse("Ctrl+Alt+Shift+M").unwrap();
        assert!(s.ctrl && s.alt && s.shift && !s.logo);
        assert_eq!(s.key, KeyCode::KeyM);
        assert_eq!(s.to_string(), "Ctrl+Alt+Shift+M");
        assert_eq!(Shortcut::parse("ctrl + f12").unwrap().key, KeyCode::F12);
        assert!(Shortcut::parse("Ctrl+Alt").is_none());
    }

    #[test]
    fn hid_matches_proto_table() {
        // Every mapped key should also map to a Windows scancode (except
        // Pause, which the host sends as a virtual key).
        for key in [
            KeyCode::KeyA,
            KeyCode::Digit0,
            KeyCode::F24,
            KeyCode::NumpadEnter,
            KeyCode::SuperRight,
        ] {
            let usage = hid_usage(key).unwrap();
            assert!(oriel_proto::keys::hid_to_set1(usage).is_some(), "{key:?}");
        }
    }
}
