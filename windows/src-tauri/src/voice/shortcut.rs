// The push-to-talk shortcut as text ("Ctrl+Alt+Space"): parsed, judged and written
// back in one canonical form. No OS call lives here, so every rule is tested on its
// own; `platform::voice` turns a `Shortcut` into a Windows hotkey.

use std::fmt;

/// What a fresh install uses. Not `Ctrl+Space`: editors use that for completion, and a
/// global hotkey would take it away from every one of them.
pub const DEFAULT: &str = "Ctrl+Alt+Space";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Space,
    /// `b'A'..=b'Z'`
    Letter(u8),
    /// `b'0'..=b'9'`
    Digit(u8),
    /// F1..=F24
    Function(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    ctrl: bool,
    alt: bool,
    shift: bool,
    win: bool,
    key: Key,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutError {
    Empty,
    /// An empty part, a repeated modifier, or two keys.
    Malformed,
    UnknownKey,
    NoKey,
    /// Shift alone is not a shortcut: it would fire while typing.
    NoModifier,
    /// A letter or digit with too little around it would break typing or editing
    /// in every program (Ctrl+C, Alt+A, or AltGr characters on some keyboards).
    TooCommon,
    /// Windows keeps this one for itself.
    Reserved,
}

impl ShortcutError {
    pub fn message(&self) -> &'static str {
        match self {
            ShortcutError::Empty => "Type a shortcut, for example Ctrl+Alt+Space.",
            ShortcutError::Malformed => "That is not a valid shortcut. Write it like Ctrl+Alt+Space.",
            ShortcutError::UnknownKey => "That key is not supported. Use a letter, a digit, Space or F1–F24.",
            ShortcutError::NoKey => "Add a key after the modifiers, for example Ctrl+Alt+Space.",
            ShortcutError::NoModifier => "Hold Ctrl, Alt or the Windows key as well.",
            ShortcutError::TooCommon => {
                "That combination would break typing in other programs. Add Ctrl+Shift, or use Space or an F key."
            }
            ShortcutError::Reserved => "Windows uses that shortcut itself. Pick another.",
        }
    }
}

// Windows' own numbering, written down so the rules can be tested on any machine.
const MOD_ALT: u32 = 0x1;
const MOD_CONTROL: u32 = 0x2;
const MOD_SHIFT: u32 = 0x4;
const MOD_WIN: u32 = 0x8;

impl Shortcut {
    pub fn parse(text: &str) -> Result<Shortcut, ShortcutError> {
        if text.trim().is_empty() {
            return Err(ShortcutError::Empty);
        }
        let (mut ctrl, mut alt, mut shift, mut win) = (false, false, false, false);
        let mut key: Option<Key> = None;
        for part in text.split('+') {
            let part = part.trim();
            if part.is_empty() {
                return Err(ShortcutError::Malformed);
            }
            let lower = part.to_ascii_lowercase();
            let flag = match lower.as_str() {
                "ctrl" | "control" => Some(&mut ctrl),
                "alt" => Some(&mut alt),
                "shift" => Some(&mut shift),
                "win" | "windows" | "super" | "meta" => Some(&mut win),
                _ => None,
            };
            if let Some(flag) = flag {
                if *flag {
                    return Err(ShortcutError::Malformed);
                }
                *flag = true;
                continue;
            }
            if key.is_some() {
                return Err(ShortcutError::Malformed);
            }
            key = Some(parse_key(&lower).ok_or(ShortcutError::UnknownKey)?);
        }
        let key = key.ok_or(ShortcutError::NoKey)?;
        let shortcut = Shortcut {
            ctrl,
            alt,
            shift,
            win,
            key,
        };
        shortcut.check()?;
        Ok(shortcut)
    }

    /// The rules for a shortcut that is registered for the whole desktop.
    fn check(&self) -> Result<(), ShortcutError> {
        if !(self.ctrl || self.alt || self.win) {
            return Err(ShortcutError::NoModifier);
        }
        let modifiers = [self.ctrl, self.alt, self.shift, self.win]
            .iter()
            .filter(|m| **m)
            .count();
        match self.key {
            Key::Letter(_) | Key::Digit(_) => {
                // Win+key is a shortcut by nature; Ctrl+Shift+key is deliberate. One
                // modifier, or Ctrl+Alt (which is AltGr on many keyboards), is not.
                let deliberate = self.win || (self.ctrl && self.shift) || modifiers >= 3;
                if !deliberate {
                    return Err(ShortcutError::TooCommon);
                }
            }
            Key::Space => {
                // Alt+Space is the window menu; Win+Space switches the input language.
                if (self.alt && !self.ctrl) || self.win {
                    return Err(ShortcutError::Reserved);
                }
            }
            // Alt+F4 closes the window; with Ctrl or Win as well it is free.
            Key::Function(4) if self.alt && !self.ctrl && !self.win => {
                return Err(ShortcutError::Reserved)
            }
            Key::Function(_) => {}
        }
        Ok(())
    }

    /// `MOD_*` for `RegisterHotKey` (without MOD_NOREPEAT, which the OS layer adds).
    pub fn modifier_flags(&self) -> u32 {
        (if self.alt { MOD_ALT } else { 0 })
            | (if self.ctrl { MOD_CONTROL } else { 0 })
            | (if self.shift { MOD_SHIFT } else { 0 })
            | (if self.win { MOD_WIN } else { 0 })
    }

    /// The Windows virtual-key code of the main key.
    pub fn virtual_key(&self) -> u32 {
        match self.key {
            Key::Space => 0x20,
            Key::Letter(c) | Key::Digit(c) => u32::from(c),
            Key::Function(n) => 0x70 + u32::from(n) - 1,
        }
    }

    /// Every virtual-key code whose release ends the push: the main key and each
    /// side of each modifier the shortcut asks for.
    pub fn release_keys(&self) -> Vec<u32> {
        let mut keys = vec![self.virtual_key()];
        if self.ctrl {
            keys.extend([0x11, 0xA2, 0xA3]);
        }
        if self.alt {
            keys.extend([0x12, 0xA4, 0xA5]);
        }
        if self.shift {
            keys.extend([0x10, 0xA0, 0xA1]);
        }
        if self.win {
            keys.extend([0x5B, 0x5C]);
        }
        keys
    }
}

fn parse_key(lower: &str) -> Option<Key> {
    if lower == "space" {
        return Some(Key::Space);
    }
    let mut chars = lower.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        return match first {
            'a'..='z' => Some(Key::Letter(first.to_ascii_uppercase() as u8)),
            '0'..='9' => Some(Key::Digit(first as u8)),
            _ => None,
        };
    }
    let number = lower.strip_prefix('f')?;
    if !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match number.parse::<u8>() {
        Ok(n @ 1..=24) => Some(Key::Function(n)),
        _ => None,
    }
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        if self.win {
            f.write_str("Win+")?;
        }
        match self.key {
            Key::Space => f.write_str("Space"),
            Key::Letter(c) | Key::Digit(c) => write!(f, "{}", c as char),
            Key::Function(n) => write!(f, "F{n}"),
        }
    }
}

/// The text a saved shortcut should have: canonical if it is valid, otherwise the
/// default. Used by settings validation.
pub fn canonical_or_default(text: &str) -> String {
    Shortcut::parse(text)
        .map(|s| s.to_string())
        .unwrap_or_else(|_| DEFAULT.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Shortcut {
        Shortcut::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
    }

    #[test]
    fn the_default_is_valid_and_is_its_own_canonical_form() {
        let s = ok(DEFAULT);
        assert_eq!(s.to_string(), DEFAULT);
        assert_eq!(s.virtual_key(), 0x20);
        assert_eq!(s.modifier_flags(), MOD_CONTROL | MOD_ALT);
    }

    #[test]
    fn spelling_case_spacing_and_order_do_not_matter() {
        for text in [
            "ctrl+alt+space",
            " Alt + Control + SPACE ",
            "CTRL+ALT+Space",
            "alt+ctrl+space",
        ] {
            assert_eq!(ok(text).to_string(), "Ctrl+Alt+Space", "{text}");
        }
        assert_eq!(ok("Windows+Shift+k").to_string(), "Shift+Win+K");
        assert_eq!(ok("super+f9").to_string(), "Win+F9");
    }

    #[test]
    fn custom_shortcuts_map_to_the_right_windows_codes() {
        let s = ok("Ctrl+Shift+K");
        assert_eq!(
            (s.virtual_key(), s.modifier_flags()),
            (0x4B, MOD_CONTROL | MOD_SHIFT)
        );
        let s = ok("Alt+F9");
        assert_eq!((s.virtual_key(), s.modifier_flags()), (0x78, MOD_ALT));
        let s = ok("Ctrl+F24");
        assert_eq!(s.virtual_key(), 0x87);
        let s = ok("Win+7");
        assert_eq!((s.virtual_key(), s.modifier_flags()), (0x37, MOD_WIN));
    }

    #[test]
    fn ctrl_space_is_allowed_when_the_user_chooses_it() {
        assert_eq!(ok("Ctrl+Space").to_string(), "Ctrl+Space");
    }

    #[test]
    fn invalid_text_is_refused_with_a_reason() {
        use ShortcutError::*;
        let cases = [
            ("", Empty),
            ("   ", Empty),
            ("Ctrl+Alt", NoKey),
            ("Ctrl+", Malformed),
            ("+Space", Malformed),
            ("Ctrl++Space", Malformed),
            ("Ctrl+Ctrl+Space", Malformed),
            ("Ctrl+A+B", Malformed),
            ("Ctrl+Alt+Banana", UnknownKey),
            ("Ctrl+Alt+F25", UnknownKey),
            ("Ctrl+Alt+F0", UnknownKey),
            ("Ctrl+Alt+Enter", UnknownKey),
            ("Space", NoModifier),
            ("Shift+Space", NoModifier),
            ("F9", NoModifier),
        ];
        for (text, expected) in cases {
            assert_eq!(Shortcut::parse(text), Err(expected.clone()), "{text:?}");
        }
    }

    #[test]
    fn combinations_that_would_break_typing_or_belong_to_windows_are_refused() {
        use ShortcutError::*;
        let cases = [
            ("Ctrl+C", TooCommon),
            ("Ctrl+V", TooCommon),
            ("Alt+A", TooCommon),
            ("Ctrl+Shift+Alt", NoKey),
            // AltGr on many keyboards: it types characters.
            ("Ctrl+Alt+E", TooCommon),
            ("Ctrl+Alt+4", TooCommon),
            ("Alt+Space", Reserved),
            ("Win+Space", Reserved),
            ("Alt+F4", Reserved),
        ];
        for (text, expected) in cases {
            assert_eq!(Shortcut::parse(text), Err(expected.clone()), "{text:?}");
        }
        // Deliberate ones are fine.
        for text in [
            "Ctrl+Shift+K",
            "Win+K",
            "Ctrl+Alt+Shift+E",
            "Ctrl+Alt+F4",
            "Ctrl+F9",
            "Alt+F9",
        ] {
            ok(text);
        }
    }

    #[test]
    fn every_error_has_a_message_that_says_what_to_do() {
        use ShortcutError::*;
        for e in [
            Empty, Malformed, UnknownKey, NoKey, NoModifier, TooCommon, Reserved,
        ] {
            assert!(e.message().len() > 20, "{e:?}");
        }
    }

    #[test]
    fn settings_get_the_canonical_text_or_the_default() {
        assert_eq!(canonical_or_default("alt+ctrl+space"), "Ctrl+Alt+Space");
        assert_eq!(canonical_or_default("nonsense"), DEFAULT);
        assert_eq!(canonical_or_default(""), DEFAULT);
        assert_eq!(canonical_or_default("Ctrl+C"), DEFAULT);
    }

    #[test]
    fn releasing_the_key_or_any_required_modifier_ends_the_push() {
        let keys = ok("Ctrl+Alt+Space").release_keys();
        for k in [0x20, 0x11, 0xA2, 0xA3, 0x12, 0xA4, 0xA5] {
            assert!(keys.contains(&k), "{k:#x}");
        }
        // A modifier the shortcut does not use must not end it.
        assert!(!keys.contains(&0x10) && !keys.contains(&0x5B));
    }
}
