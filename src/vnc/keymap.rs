//! Which physical key types a character on the remote machine.
//!
//! A terminal reports characters, and RFB names keys by keysym. A QEMU-based
//! server (OpenStack, OCI) turns a keysym into a key press through its own
//! keymap, US English unless configured otherwise, so a guest with a German
//! console layout gets `y` for `z`, and most symbols land on the wrong key.
//! QEMU's extended key event carries the key's scancode as well; with it, the
//! guest receives the physical key that types the character in *its* layout.
//! This module knows those keys for the layouts it supports.

use std::path::Path;

/// A guest keyboard layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Us,
    De,
}

impl Layout {
    /// From a configured name (`us`, `de`, or a console keymap name such as
    /// `de-latin1`). `None` for a layout that is not supported.
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        let base = name.split(['-', '_', '.']).next().unwrap_or_default();
        match base {
            "us" | "en" => Some(Self::Us),
            "de" => Some(Self::De),
            _ => None,
        }
    }

    /// The local console's layout (`KEYMAP=` in `/etc/vconsole.conf`), on the
    /// assumption that the machines one looks after are set up the same way.
    /// US English when it is not set or not supported.
    pub fn detect() -> Self {
        Self::from_vconsole(Path::new("/etc/vconsole.conf")).unwrap_or(Self::Us)
    }

    fn from_vconsole(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("KEYMAP="))
            .next_back()
            .and_then(|v| Self::parse(v.trim_matches(['"', '\''])))
    }
}

/// How a character is typed: the key, and what is held with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    /// XT scancode as QEMU numbers it (`0xe0`-prefixed keys have bit 7 set).
    pub scancode: u16,
    pub shift: bool,
    pub altgr: bool,
    /// A dead key: the guest waits for the next key, so a space follows to
    /// produce the character itself.
    pub dead: bool,
}

/// One key: its scancode and the characters it types plain, with Shift and
/// with AltGr (`'\0'` for none).
type Row = (u16, char, char, char);

const SPACE: Row = (0x39, ' ', ' ', '\0');

const LETTERS_US: [(u16, char); 26] = [
    (0x1e, 'a'),
    (0x30, 'b'),
    (0x2e, 'c'),
    (0x20, 'd'),
    (0x12, 'e'),
    (0x21, 'f'),
    (0x22, 'g'),
    (0x23, 'h'),
    (0x17, 'i'),
    (0x24, 'j'),
    (0x25, 'k'),
    (0x26, 'l'),
    (0x32, 'm'),
    (0x31, 'n'),
    (0x18, 'o'),
    (0x19, 'p'),
    (0x10, 'q'),
    (0x13, 'r'),
    (0x1f, 's'),
    (0x14, 't'),
    (0x16, 'u'),
    (0x2f, 'v'),
    (0x11, 'w'),
    (0x2d, 'x'),
    (0x15, 'y'),
    (0x2c, 'z'),
];

const US: &[Row] = &[
    (0x29, '`', '~', '\0'),
    (0x02, '1', '!', '\0'),
    (0x03, '2', '@', '\0'),
    (0x04, '3', '#', '\0'),
    (0x05, '4', '$', '\0'),
    (0x06, '5', '%', '\0'),
    (0x07, '6', '^', '\0'),
    (0x08, '7', '&', '\0'),
    (0x09, '8', '*', '\0'),
    (0x0a, '9', '(', '\0'),
    (0x0b, '0', ')', '\0'),
    (0x0c, '-', '_', '\0'),
    (0x0d, '=', '+', '\0'),
    (0x1a, '[', '{', '\0'),
    (0x1b, ']', '}', '\0'),
    (0x2b, '\\', '|', '\0'),
    (0x27, ';', ':', '\0'),
    (0x28, '\'', '"', '\0'),
    (0x33, ',', '<', '\0'),
    (0x34, '.', '>', '\0'),
    (0x35, '/', '?', '\0'),
    SPACE,
];

/// German (QWERTZ), as the Linux console's `de` keymap and XKB `de`.
const DE: &[Row] = &[
    (0x29, '^', '°', '\0'),
    (0x02, '1', '!', '\0'),
    (0x03, '2', '"', '²'),
    (0x04, '3', '§', '³'),
    (0x05, '4', '$', '\0'),
    (0x06, '5', '%', '\0'),
    (0x07, '6', '&', '\0'),
    (0x08, '7', '/', '{'),
    (0x09, '8', '(', '['),
    (0x0a, '9', ')', ']'),
    (0x0b, '0', '=', '}'),
    (0x0c, 'ß', '?', '\\'),
    (0x0d, '´', '`', '\0'),
    (0x1a, 'ü', 'Ü', '\0'),
    (0x1b, '+', '*', '~'),
    (0x27, 'ö', 'Ö', '\0'),
    (0x28, 'ä', 'Ä', '\0'),
    (0x2b, '#', '\'', '\0'),
    (0x56, '<', '>', '|'),
    (0x33, ',', ';', '\0'),
    (0x34, '.', ':', '\0'),
    (0x35, '-', '_', '\0'),
    SPACE,
];

/// German keys that wait for the next key.
const DE_DEAD: &[char] = &['^', '´', '`'];

/// The physical key, on the US keyboard, of a German letter.
fn de_letter(c: char) -> char {
    match c {
        'y' => 'z',
        'z' => 'y',
        c => c,
    }
}

/// How `c` is typed on a guest with `layout`. `None` for a character the
/// layout does not have (or that is not a printable character).
pub fn stroke(layout: Layout, c: char) -> Option<Stroke> {
    let lower = c.to_lowercase().next().filter(|l| l.is_ascii_lowercase());
    if let Some(l) = lower {
        let key = if layout == Layout::De {
            de_letter(l)
        } else {
            l
        };
        let scancode = LETTERS_US.iter().find(|(_, k)| *k == key)?.0;
        return Some(Stroke {
            scancode,
            shift: c.is_uppercase(),
            altgr: false,
            dead: false,
        });
    }
    let (rows, dead): (&[Row], &[char]) = match layout {
        Layout::Us => (US, &[]),
        Layout::De => (DE, DE_DEAD),
    };
    let altgr_letters: &[(char, char)] = match layout {
        Layout::De => &[('@', 'q'), ('€', 'e'), ('µ', 'm')],
        Layout::Us => &[],
    };
    if let Some((_, letter)) = altgr_letters.iter().find(|(ch, _)| *ch == c) {
        let key = de_letter(*letter);
        let scancode = LETTERS_US.iter().find(|(_, k)| *k == key)?.0;
        return Some(Stroke {
            scancode,
            shift: false,
            altgr: true,
            dead: false,
        });
    }
    rows.iter().find_map(|&(scancode, plain, shifted, altgr)| {
        let (shift, altgr) = if c == plain {
            (false, false)
        } else if c == shifted {
            (true, false)
        } else if altgr != '\0' && c == altgr {
            (false, true)
        } else {
            return None;
        };
        Some(Stroke {
            scancode,
            shift,
            altgr,
            dead: dead.contains(&c),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(scancode: u16, shift: bool, altgr: bool) -> Option<Stroke> {
        Some(Stroke {
            scancode,
            shift,
            altgr,
            dead: false,
        })
    }

    #[test]
    fn letters_are_on_the_layouts_keys() {
        assert_eq!(stroke(Layout::Us, 'z'), s(0x2c, false, false));
        assert_eq!(stroke(Layout::Us, 'Y'), s(0x15, true, false));
        // QWERTZ: z and y trade places.
        assert_eq!(stroke(Layout::De, 'z'), s(0x15, false, false));
        assert_eq!(stroke(Layout::De, 'Y'), s(0x2c, true, false));
        assert_eq!(stroke(Layout::De, 'a'), s(0x1e, false, false));
    }

    #[test]
    fn german_symbols_use_shift_and_altgr() {
        assert_eq!(stroke(Layout::De, '@'), s(0x10, false, true));
        assert_eq!(stroke(Layout::De, '/'), s(0x08, true, false));
        assert_eq!(stroke(Layout::De, '-'), s(0x35, false, false));
        assert_eq!(stroke(Layout::De, '|'), s(0x56, false, true));
        assert_eq!(stroke(Layout::De, '\\'), s(0x0c, false, true));
        assert_eq!(stroke(Layout::De, 'ö'), s(0x27, false, false));
        assert_eq!(stroke(Layout::De, '{'), s(0x08, false, true));
        assert_eq!(stroke(Layout::De, '~'), s(0x1b, false, true));
        assert_eq!(stroke(Layout::De, ' '), s(0x39, false, false));
        assert_eq!(stroke(Layout::Us, '@'), s(0x03, true, false));
        assert_eq!(stroke(Layout::Us, '/'), s(0x35, false, false));
    }

    #[test]
    fn dead_keys_are_marked() {
        let caret = stroke(Layout::De, '^').unwrap();
        assert!(caret.dead && caret.scancode == 0x29);
        assert!(!stroke(Layout::Us, '^').unwrap().dead);
    }

    #[test]
    fn characters_a_layout_lacks_have_no_stroke() {
        assert_eq!(stroke(Layout::Us, 'ö'), None);
        assert_eq!(stroke(Layout::De, 'é'), None);
        assert_eq!(stroke(Layout::De, '\n'), None);
    }

    #[test]
    fn layout_names_and_the_console_keymap() {
        assert_eq!(Layout::parse("de"), Some(Layout::De));
        assert_eq!(Layout::parse("de-latin1-nodeadkeys"), Some(Layout::De));
        assert_eq!(Layout::parse("US"), Some(Layout::Us));
        assert_eq!(Layout::parse("fr"), None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vconsole.conf");
        std::fs::write(&path, "FONT=x\nKEYMAP=\"de\"\n").unwrap();
        assert_eq!(Layout::from_vconsole(&path), Some(Layout::De));
        std::fs::write(&path, "FONT=x\n").unwrap();
        assert_eq!(Layout::from_vconsole(&path), None);
    }
}
