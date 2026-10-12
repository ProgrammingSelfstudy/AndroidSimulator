//! macOS key code -> Android key code mapping.
use moniqi_scrcpy::keycode as ak;

/// macOS virtual key codes (`kVK_*`) for keys that don't produce text.
pub mod mac {
    pub const RETURN: u16 = 36;
    pub const TAB: u16 = 48;
    pub const DELETE: u16 = 51; // backspace
    pub const ESCAPE: u16 = 53;
    pub const FORWARD_DELETE: u16 = 117;
    pub const HOME: u16 = 115;
    pub const END: u16 = 119;
    pub const PAGE_UP: u16 = 116;
    pub const PAGE_DOWN: u16 = 121;
    pub const LEFT: u16 = 123;
    pub const RIGHT: u16 = 124;
    pub const DOWN: u16 = 125;
    pub const UP: u16 = 126;
    // letters used for Option+<key> device shortcuts
    pub const H: u16 = 4;
    pub const B: u16 = 11;
    pub const S: u16 = 1;
    pub const R: u16 = 15;
}

/// Android keycode for non-text keys, if any.
pub fn special_key(mac_code: u16) -> Option<u32> {
    Some(match mac_code {
        mac::RETURN => ak::ENTER,
        mac::TAB => ak::TAB,
        mac::DELETE => ak::DEL,
        mac::ESCAPE => ak::BACK,
        mac::FORWARD_DELETE => ak::FORWARD_DEL,
        mac::HOME => ak::MOVE_HOME,
        mac::END => ak::MOVE_END,
        mac::PAGE_UP => ak::PAGE_UP,
        mac::PAGE_DOWN => ak::PAGE_DOWN,
        mac::LEFT => ak::DPAD_LEFT,
        mac::RIGHT => ak::DPAD_RIGHT,
        mac::DOWN => ak::DPAD_DOWN,
        mac::UP => ak::DPAD_UP,
        _ => return None,
    })
}

/// What an Option+<key> chord means on the device.
#[derive(Debug, PartialEq)]
pub enum Shortcut {
    Key(u32),
    Rotate,
}

pub fn option_shortcut(mac_code: u16) -> Option<Shortcut> {
    match mac_code {
        mac::H => Some(Shortcut::Key(ak::HOME)),
        mac::B => Some(Shortcut::Key(ak::BACK)),
        mac::S => Some(Shortcut::Key(ak::APP_SWITCH)),
        mac::R => Some(Shortcut::Rotate),
        _ => None,
    }
}

/// Characters worth injecting as text: printable, no control codes. macOS reports function and
/// arrow keys as private-use characters (U+F700..), which must not be typed into the device.
pub fn printable(s: &str) -> Option<&str> {
    if s.is_empty() || s.chars().any(|c| c.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&c)) {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_specials() {
        assert_eq!(special_key(mac::ESCAPE), Some(ak::BACK));
        assert_eq!(special_key(mac::UP), Some(ak::DPAD_UP));
        assert_eq!(special_key(0), None); // 'a'
    }

    #[test]
    fn shortcuts() {
        assert_eq!(option_shortcut(mac::H), Some(Shortcut::Key(ak::HOME)));
        assert_eq!(option_shortcut(mac::R), Some(Shortcut::Rotate));
        assert_eq!(option_shortcut(mac::UP), None);
    }

    #[test]
    fn filters_non_text() {
        assert_eq!(printable("a"), Some("a"));
        assert_eq!(printable("é"), Some("é"));
        assert_eq!(printable("\u{F700}"), None); // up arrow
        assert_eq!(printable("\r"), None);
        assert_eq!(printable(""), None);
    }
}
