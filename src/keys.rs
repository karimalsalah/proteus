//! Key events as bytes. Pure: no window-system types, so every rule here is
//! a row in a table that a test checks.
//!
//! Two dialects share one description of a key:
//!  - legacy, what a program that asked for nothing expects (xterm's bytes);
//!  - the kitty keyboard protocol, switched on a flag at a time by the
//!    program (`CSI > flags u`), which can tell Shift+Enter from Enter,
//!    Ctrl+I from Tab, and a key going down from a key coming up.
//!
//! Reference: https://sw.kovidgoyal.net/kitty/keyboard-protocol/

/// Progressive enhancement flags, as the program sets them.
pub const DISAMBIGUATE: u8 = 1;
pub const EVENT_TYPES: u8 = 2;
pub const ALTERNATE_KEYS: u8 = 4;
pub const ALL_KEYS: u8 = 8;
pub const ASSOCIATED_TEXT: u8 = 16;

/// Modifier bits, as the protocol numbers them.
pub const SHIFT: u8 = 1;
pub const ALT: u8 = 2;
pub const CTRL: u8 = 4;
pub const SUPER: u8 = 8;
#[allow(dead_code)] // no such keys on a PC keyboard; the encoder still carries them
pub const HYPER: u8 = 16;
#[allow(dead_code)]
pub const META: u8 = 32;
pub const CAPS_LOCK: u8 = 64;
pub const NUM_LOCK: u8 = 128;
const LOCKS: u8 = CAPS_LOCK | NUM_LOCK;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Press,
    Repeat,
    Release,
}

/// Keys that are not text. The list is the protocol's, so it is complete
/// whether or not a given keyboard (or window system) can produce each one.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fk {
    Escape,
    Enter,
    Tab,
    Backspace,
    Insert,
    Delete,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    /// F1 to F35.
    F(u8),
    /// Keypad digit 0 to 9.
    Kp(u8),
    KpDecimal,
    KpDivide,
    KpMultiply,
    KpSubtract,
    KpAdd,
    KpEnter,
    KpEqual,
    KpSeparator,
    KpLeft,
    KpRight,
    KpUp,
    KpDown,
    KpPageUp,
    KpPageDown,
    KpHome,
    KpEnd,
    KpInsert,
    KpDelete,
    KpBegin,
    MediaPlay,
    MediaPause,
    MediaPlayPause,
    MediaReverse,
    MediaStop,
    MediaFastForward,
    MediaRewind,
    MediaTrackNext,
    MediaTrackPrevious,
    MediaRecord,
    LowerVolume,
    RaiseVolume,
    MuteVolume,
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    LeftHyper,
    LeftMeta,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
    RightHyper,
    RightMeta,
    IsoLevel3Shift,
    IsoLevel5Shift,
}

impl Fk {
    /// The key's number and the final byte of its escape code.
    pub fn code(self) -> (u32, u8) {
        use Fk::*;
        match self {
            Escape => (27, b'u'),
            Enter => (13, b'u'),
            Tab => (9, b'u'),
            Backspace => (127, b'u'),
            Insert => (2, b'~'),
            Delete => (3, b'~'),
            Left => (1, b'D'),
            Right => (1, b'C'),
            Up => (1, b'A'),
            Down => (1, b'B'),
            PageUp => (5, b'~'),
            PageDown => (6, b'~'),
            Home => (1, b'H'),
            End => (1, b'F'),
            CapsLock => (57358, b'u'),
            ScrollLock => (57359, b'u'),
            NumLock => (57360, b'u'),
            PrintScreen => (57361, b'u'),
            Pause => (57362, b'u'),
            Menu => (57363, b'u'),
            F(1) => (1, b'P'),
            F(2) => (1, b'Q'),
            F(3) => (13, b'~'),
            F(4) => (1, b'S'),
            F(n @ 5..=12) => ([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5], b'~'),
            F(n) => (57376 + (n.clamp(13, 35) as u32 - 13), b'u'),
            Kp(n) => (57399 + n.min(9) as u32, b'u'),
            KpDecimal => (57409, b'u'),
            KpDivide => (57410, b'u'),
            KpMultiply => (57411, b'u'),
            KpSubtract => (57412, b'u'),
            KpAdd => (57413, b'u'),
            KpEnter => (57414, b'u'),
            KpEqual => (57415, b'u'),
            KpSeparator => (57416, b'u'),
            KpLeft => (57417, b'u'),
            KpRight => (57418, b'u'),
            KpUp => (57419, b'u'),
            KpDown => (57420, b'u'),
            KpPageUp => (57421, b'u'),
            KpPageDown => (57422, b'u'),
            KpHome => (57423, b'u'),
            KpEnd => (57424, b'u'),
            KpInsert => (57425, b'u'),
            KpDelete => (57426, b'u'),
            KpBegin => (1, b'E'),
            MediaPlay => (57428, b'u'),
            MediaPause => (57429, b'u'),
            MediaPlayPause => (57430, b'u'),
            MediaReverse => (57431, b'u'),
            MediaStop => (57432, b'u'),
            MediaFastForward => (57433, b'u'),
            MediaRewind => (57434, b'u'),
            MediaTrackNext => (57435, b'u'),
            MediaTrackPrevious => (57436, b'u'),
            MediaRecord => (57437, b'u'),
            LowerVolume => (57438, b'u'),
            RaiseVolume => (57439, b'u'),
            MuteVolume => (57440, b'u'),
            LeftShift => (57441, b'u'),
            LeftControl => (57442, b'u'),
            LeftAlt => (57443, b'u'),
            LeftSuper => (57444, b'u'),
            LeftHyper => (57445, b'u'),
            LeftMeta => (57446, b'u'),
            RightShift => (57447, b'u'),
            RightControl => (57448, b'u'),
            RightAlt => (57449, b'u'),
            RightSuper => (57450, b'u'),
            RightHyper => (57451, b'u'),
            RightMeta => (57452, b'u'),
            IsoLevel3Shift => (57453, b'u'),
            IsoLevel5Shift => (57454, b'u'),
        }
    }

    pub fn is_modifier(self) -> bool {
        (57441..=57454).contains(&self.code().0) || matches!(self, Fk::CapsLock | Fk::ScrollLock | Fk::NumLock)
    }

    /// The key a keypad key stands for when the program cannot tell them apart.
    fn off_keypad(self) -> Code {
        use Fk::*;
        Code::Fn(match self {
            Kp(n) => return Code::Char((b'0' + n.min(9)) as char),
            KpDecimal => return Code::Char('.'),
            KpDivide => return Code::Char('/'),
            KpMultiply => return Code::Char('*'),
            KpSubtract => return Code::Char('-'),
            KpAdd => return Code::Char('+'),
            KpEqual => return Code::Char('='),
            KpSeparator => return Code::Char(','),
            KpEnter => Enter,
            KpLeft => Left,
            KpRight => Right,
            KpUp => Up,
            KpDown => Down,
            KpPageUp => PageUp,
            KpPageDown => PageDown,
            KpHome => Home,
            KpEnd => End,
            KpInsert => Insert,
            KpDelete => Delete,
            other => other,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Code {
    /// A text key, named by the character it types with no modifiers held
    /// (lower case). `'\0'` is text that came from no particular key.
    Char(char),
    Fn(Fk),
}

/// One key event, described without reference to any window system.
#[derive(Clone, Copy, Debug)]
pub struct Key<'a> {
    pub code: Code,
    /// What the key types with Shift held, when Shift is held.
    pub shifted: Option<char>,
    /// What the same physical key types on a US layout.
    pub base: Option<char>,
    /// The text this press produces; empty if none. Never control characters.
    pub text: &'a str,
    pub mods: u8,
    pub action: Action,
}

/// Control codes: what Ctrl turns a key into for a program that asked for
/// nothing. `None` means Ctrl leaves the key as it is.
pub fn ctrl_code(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        '@' | '2' | ' ' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' | '~' => 0x1e,
        // Ctrl+- is how Ctrl+_ (undo, in readline and emacs) is typed on a PC.
        '_' | '-' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

fn push_char(v: &mut Vec<u8>, c: char) {
    let mut b = [0u8; 4];
    v.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
}

/// Encode one key event for a program that has set `flags` (0 = legacy).
/// `app_cursor` is DECCKM. An empty result means "send nothing".
pub fn encode(key: &Key, flags: u8, app_cursor: bool) -> Vec<u8> {
    let (disambiguate, events, all) = (flags & DISAMBIGUATE != 0, flags & EVENT_TYPES != 0, flags & ALL_KEYS != 0);
    if key.action == Action::Release && !events {
        return Vec::new();
    }
    let mut k = *key;
    if k.text.chars().any(|c| c.is_control()) {
        k.text = "";
    }
    // AltGr arrives as Ctrl+Alt carrying its own text; that is typing, not a chord.
    if k.mods & (CTRL | ALT) == CTRL | ALT && !k.text.is_empty() {
        if let Code::Char(c) = k.code {
            let plain = if k.mods & SHIFT != 0 { k.shifted.unwrap_or(c) } else { c };
            if !k.text.chars().eq(std::iter::once(plain)) {
                k.mods &= !(CTRL | ALT);
            }
        }
    }
    // Without "disambiguate" or "all keys" the keypad is not a separate place.
    if !disambiguate && !all {
        if let Code::Fn(f) = k.code {
            k.code = f.off_keypad();
        }
    }
    if !disambiguate && !events && !all {
        return legacy(&k, app_cursor);
    }

    let held = k.mods & !LOCKS;
    let release = k.action == Action::Release;
    // A press types its text only with nothing but Shift held. Under Ctrl,
    // Alt or Super the window system may still say what the key would have
    // typed (Windows reports "the text without Ctrl", and Alt+a carries "a");
    // that is the chord's key, not text the chord produced.
    let typing = !k.text.is_empty() && held & !SHIFT == 0 && !release;
    // Text stays text unless the program asked for every key as an escape code.
    if !all && typing {
        return k.text.as_bytes().to_vec();
    }
    let embed = flags & ASSOCIATED_TEXT != 0 && typing;
    let text = if embed { k.text } else { "" };
    match k.code {
        Code::Fn(f) => {
            if f.is_modifier() && !all {
                return Vec::new();
            }
            // Enter, Tab and Backspace stay as they were so that a shell is
            // still usable after a program dies without restoring the mode.
            if !all && matches!(f, Fk::Enter | Fk::Tab | Fk::Backspace) {
                if release {
                    return Vec::new();
                }
                if held == 0 {
                    return vec![match f {
                        Fk::Enter => b'\r',
                        Fk::Tab => b'\t',
                        _ => 0x7f,
                    }];
                }
            }
            if !all && !disambiguate && f == Fk::Escape && held == 0 && !release {
                return vec![0x1b];
            }
            let (n, fin) = f.code();
            csi(n, None, None, k.mods, k.action, events, text, fin)
        }
        Code::Char('\0') => {
            if embed {
                csi(0, None, None, 0, Action::Press, false, text, b'u')
            } else {
                k.text.as_bytes().to_vec()
            }
        }
        Code::Char(c) => {
            let alternates = flags & ALTERNATE_KEYS != 0;
            let shifted = if alternates && k.mods & SHIFT != 0 { k.shifted.filter(|&s| s != c) } else { None };
            let base = if alternates { k.base.filter(|&b| b != c) } else { None };
            let plain = !(events && k.action != Action::Press) && shifted.is_none() && base.is_none() && !embed;
            if plain && !disambiguate && !all {
                return legacy(&k, app_cursor);
            }
            if plain && !all && held & !SHIFT == 0 {
                // A character key whose press carried no text is still that character.
                let mut out = Vec::new();
                push_char(&mut out, if held & SHIFT != 0 { k.shifted.unwrap_or(c) } else { c });
                return out;
            }
            // Lock keys are not reported on text keys unless every key is an escape code.
            let mods = if all { k.mods } else { held };
            csi(c as u32, shifted, base, mods, k.action, events, text, b'u')
        }
    }
}

/// `CSI number[:shifted[:base]] [; modifiers[:event] [; text]] final`.
#[allow(clippy::too_many_arguments)]
fn csi(n: u32, shifted: Option<char>, base: Option<char>, mods: u8, action: Action, events: bool, text: &str, fin: u8) -> Vec<u8> {
    use std::fmt::Write;
    let event = match action {
        Action::Repeat if events => 2,
        Action::Release if events => 3,
        _ => 0,
    };
    let alternates = shifted.is_some() || base.is_some();
    let second = mods != 0 || event != 0;
    let third = !text.is_empty();
    let mut s = String::from("\x1b[");
    // `CSI 1 A` is written `CSI A`: the 1 appears only to hold the place.
    if !(n == 1 && fin.is_ascii_uppercase() && !alternates && !second && !third) {
        let _ = write!(s, "{n}");
    }
    if alternates {
        s.push(':');
        if let Some(c) = shifted {
            let _ = write!(s, "{}", c as u32);
        }
        if let Some(c) = base {
            let _ = write!(s, ":{}", c as u32);
        }
    }
    if second || third {
        s.push(';');
        if second {
            let _ = write!(s, "{}", 1 + mods as u32);
        }
    }
    if event != 0 {
        let _ = write!(s, ":{event}");
    }
    if third {
        s.push(';');
        for (i, c) in text.chars().enumerate() {
            if i > 0 {
                s.push(':');
            }
            let _ = write!(s, "{}", c as u32);
        }
    }
    s.push(fin as char);
    s.into_bytes()
}

/// What a program that asked for nothing is sent. This is xterm's encoding
/// and the protocol's own "legacy" tables, with two deliberate differences:
/// Shift+Enter sends ESC CR (agent interfaces read it as "new line, do not
/// submit"; plain CR would submit), and Ctrl+Shift+key sends the same control
/// code as Ctrl+key (a program that has not asked for the protocol cannot
/// read a `CSI u`).
fn legacy(k: &Key, app_cursor: bool) -> Vec<u8> {
    let (shift, alt, ctrl) = (k.mods & SHIFT != 0, k.mods & ALT != 0, k.mods & CTRL != 0);
    let m = 1 + (k.mods & (SHIFT | ALT | CTRL));
    let esc = |alt: bool, rest: &[u8]| {
        let mut v = Vec::with_capacity(rest.len() + 1);
        if alt {
            v.push(0x1b);
        }
        v.extend_from_slice(rest);
        v
    };
    match k.code {
        Code::Char(c) => {
            let typed = if shift { k.shifted.unwrap_or(c) } else { c };
            let mut out = Vec::new();
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                // A key that types a letter outside ASCII (Cyrillic, Greek, an
                // accented letter) has no control code of its own: Ctrl acts
                // on the key in that position on a US keyboard, so Ctrl+C
                // interrupts whatever layout is active.
                let key = if typed.is_ascii() { typed } else { k.base.filter(char::is_ascii).unwrap_or(typed) };
                match ctrl_code(key) {
                    Some(b) => out.push(b),
                    // Ctrl leaves a key it has no code for as it is.
                    None if c != '\0' => push_char(&mut out, key),
                    None => out.extend_from_slice(k.text.as_bytes()),
                }
            } else if !k.text.is_empty() {
                out.extend_from_slice(k.text.as_bytes());
            } else if c != '\0' {
                push_char(&mut out, typed);
            }
            if out == [0x1b] && c == '\0' {
                out.clear();
            }
            out
        }
        Code::Fn(f) => {
            use Fk::*;
            let (n, fin) = f.code();
            match f {
                Enter => esc(shift || alt, b"\r"),
                Escape => esc(alt, b"\x1b"),
                Backspace => esc(alt, if ctrl { b"\x08" } else { b"\x7f" }),
                Tab => esc(alt, if shift { b"\x1b[Z" } else { b"\t" }),
                Left | Right | Up | Down | Home | End | KpBegin => {
                    if m > 1 {
                        format!("\x1b[1;{m}{}", fin as char).into_bytes()
                    } else if app_cursor {
                        vec![0x1b, b'O', fin]
                    } else {
                        vec![0x1b, b'[', fin]
                    }
                }
                F(n @ 1..=4) => {
                    let fin = b"PQRS"[n as usize - 1];
                    if m > 1 {
                        format!("\x1b[1;{m}{}", fin as char).into_bytes()
                    } else {
                        vec![0x1b, b'O', fin]
                    }
                }
                Insert | Delete | PageUp | PageDown | F(5..=12) | Menu => {
                    let n = if f == Menu { 29 } else { n };
                    if m > 1 {
                        format!("\x1b[{n};{m}~").into_bytes()
                    } else {
                        format!("\x1b[{n}~").into_bytes()
                    }
                }
                _ => Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: u8 = 0;

    fn text_key(c: char, shifted: char, mods: u8, action: Action) -> Key<'static> {
        // What a window system reports: text only when no chord is held.
        let typed = if mods & SHIFT != 0 { shifted } else { c };
        let text: &'static str =
            if mods & !(SHIFT | LOCKS) == 0 && action != Action::Release { Box::leak(typed.to_string().into_boxed_str()) } else { "" };
        Key { code: Code::Char(c), shifted: (mods & SHIFT != 0).then_some(shifted), base: None, text, mods, action }
    }

    fn fk(f: Fk, mods: u8, action: Action) -> Key<'static> {
        Key { code: Code::Fn(f), shifted: None, base: None, text: "", mods, action }
    }

    fn show(b: &[u8]) -> String {
        b.iter().map(|&c| if (0x20..0x7f).contains(&c) { (c as char).to_string() } else { format!("\\x{c:02x}") }).collect()
    }

    /// Runs a table; returns how many rows there were. Panics on the first
    /// row that does not match, naming it.
    fn check(rows: &[(&str, Key, u8, bool, &[u8])]) -> usize {
        for (name, key, flags, app_cursor, want) in rows {
            let got = encode(key, *flags, *app_cursor);
            assert_eq!(show(&got), show(want), "{name} (flags {flags})");
        }
        rows.len()
    }

    use Action::{Press, Release, Repeat};

    /// The specification's description of the protocol proper (the
    /// progressive enhancements): every key-to-bytes pair it writes out, the
    /// pairs that follow directly from a rule it states, and its whole table
    /// of functional key numbers.
    #[test]
    fn kitty_specification_examples() {
        let d = DISAMBIGUATE;
        let all_text = ALL_KEYS | ASSOCIATED_TEXT;
        let a_ring = Key { code: Code::Char('\0'), shifted: None, base: None, text: "\u{e5}", mods: NONE, action: Press };
        let cyrillic_c = Key { code: Code::Char('\u{441}'), shifted: None, base: Some('c'), text: "", mods: CTRL, action: Press };
        let mut n = check(&[
            // "Key codes": the code is always the lower-case, unshifted one.
            ("ctrl+shift+a", text_key('a', 'A', CTRL | SHIFT, Press), d, false, b"\x1b[97;6u"),
            // "Text as code points".
            ("shift+a with its text", text_key('a', 'A', SHIFT, Press), all_text, false, b"\x1b[97;2;65u"),
            ("text from no key", a_ring, all_text, false, b"\x1b[0;;229u"),
            // "Disambiguate escape codes".
            ("esc", fk(Fk::Escape, NONE, Press), d, false, b"\x1b[27u"),
            ("ctrl+c is a key, not a signal", text_key('c', 'C', CTRL, Press), d, false, b"\x1b[99;5u"),
            ("alt+a", text_key('a', 'A', ALT, Press), d, false, b"\x1b[97;3u"),
            ("ctrl+alt+a", text_key('a', 'A', CTRL | ALT, Press), d, false, b"\x1b[97;7u"),
            ("shift+alt+a", text_key('a', 'A', SHIFT | ALT, Press), d, false, b"\x1b[97;4u"),
            ("enter stays", fk(Fk::Enter, NONE, Press), d, false, b"\r"),
            ("tab stays", fk(Fk::Tab, NONE, Press), d, false, b"\t"),
            ("backspace stays", fk(Fk::Backspace, NONE, Press), d, false, b"\x7f"),
            ("plain text stays", text_key('a', 'A', NONE, Press), d, false, b"a"),
            ("shifted text stays", text_key('a', 'A', SHIFT, Press), d, false, b"A"),
            // The errata to fixterms, and the "legacy text keys" last column.
            ("ctrl+shift+tab", fk(Fk::Tab, CTRL | SHIFT, Press), d, false, b"\x1b[9;6u"),
            ("ctrl+shift+i", text_key('i', 'I', CTRL | SHIFT, Press), d, false, b"\x1b[105;6u"),
            ("ctrl+shift+3", text_key('3', '#', CTRL | SHIFT, Press), d, false, b"\x1b[51;6u"),
            ("ctrl+shift+;", text_key(';', ':', CTRL | SHIFT, Press), d, false, b"\x1b[59;6u"),
            // "Modifiers": ctrl is 4, so ctrl+F1 carries 5.
            ("ctrl+f1", fk(Fk::F(1), CTRL, Press), d, false, b"\x1b[1;5P"),
            // "Report all keys as escape codes".
            ("enter, all keys", fk(Fk::Enter, NONE, Press), ALL_KEYS, false, b"\x1b[13u"),
            ("tab, all keys", fk(Fk::Tab, NONE, Press), ALL_KEYS, false, b"\x1b[9u"),
            ("backspace, all keys", fk(Fk::Backspace, NONE, Press), ALL_KEYS, false, b"\x1b[127u"),
            ("a, all keys", text_key('a', 'A', NONE, Press), ALL_KEYS, false, b"\x1b[97u"),
            ("left shift goes down", fk(Fk::LeftShift, SHIFT, Press), ALL_KEYS, false, b"\x1b[57441;2u"),
            ("left ctrl goes down", fk(Fk::LeftControl, CTRL, Press), ALL_KEYS, false, b"\x1b[57442;5u"),
            ("left ctrl comes up", fk(Fk::LeftControl, NONE, Release), ALL_KEYS | EVENT_TYPES, false, b"\x1b[57442;1:3u"),
            ("left ctrl comes up, right still held", fk(Fk::LeftControl, CTRL, Release), ALL_KEYS | EVENT_TYPES, false, b"\x1b[57442;5:3u"),
            // "Event types".
            ("repeat", text_key('a', 'A', CTRL, Repeat), d | EVENT_TYPES, false, b"\x1b[97;5:2u"),
            ("release", text_key('a', 'A', NONE, Release), d | EVENT_TYPES, false, b"\x1b[97;1:3u"),
            ("release of a cursor key", fk(Fk::Up, NONE, Release), d | EVENT_TYPES, false, b"\x1b[1;1:3A"),
            ("no release for enter", fk(Fk::Enter, NONE, Release), d | EVENT_TYPES, false, b""),
            ("no release for tab", fk(Fk::Tab, NONE, Release), d | EVENT_TYPES, false, b""),
            ("no release for backspace", fk(Fk::Backspace, NONE, Release), d | EVENT_TYPES, false, b""),
            ("enter released, all keys", fk(Fk::Enter, NONE, Release), ALL_KEYS | EVENT_TYPES, false, b"\x1b[13;1:3u"),
            ("releases are not sent unless asked for", text_key('a', 'A', NONE, Release), d, false, b""),
            // "Report alternate keys".
            ("shifted alternate", text_key('a', 'A', CTRL | SHIFT, Press), d | ALTERNATE_KEYS, false, b"\x1b[97:65;6u"),
            ("base layout only", cyrillic_c, d | ALTERNATE_KEYS, false, b"\x1b[1089::99;5u"),
            // Lock keys: not on text keys, present when every key is an escape code.
            ("caps lock does not turn text into a code", text_key('a', 'A', CAPS_LOCK, Press), d, false, b"a"),
            ("ctrl+a under num lock", text_key('a', 'A', CTRL | NUM_LOCK, Press), d, false, b"\x1b[97;5u"),
            ("ctrl+a under num lock, all keys", text_key('a', 'A', CTRL | NUM_LOCK, Press), ALL_KEYS, false, b"\x1b[97;133u"),
        ]);

        // The functional key table: number and final byte for every key in it.
        use Fk::*;
        let table: &[(Fk, &[u8])] = &[
            (Escape, b"\x1b[27u"), (Insert, b"\x1b[2~"), (Delete, b"\x1b[3~"), (Left, b"\x1b[D"), (Right, b"\x1b[C"),
            (Up, b"\x1b[A"), (Down, b"\x1b[B"), (PageUp, b"\x1b[5~"), (PageDown, b"\x1b[6~"), (Home, b"\x1b[H"), (End, b"\x1b[F"),
            (PrintScreen, b"\x1b[57361u"), (Pause, b"\x1b[57362u"), (Menu, b"\x1b[57363u"),
            (F(1), b"\x1b[P"), (F(2), b"\x1b[Q"), (F(3), b"\x1b[13~"), (F(4), b"\x1b[S"), (F(5), b"\x1b[15~"), (F(6), b"\x1b[17~"),
            (F(7), b"\x1b[18~"), (F(8), b"\x1b[19~"), (F(9), b"\x1b[20~"), (F(10), b"\x1b[21~"), (F(11), b"\x1b[23~"), (F(12), b"\x1b[24~"),
            (F(13), b"\x1b[57376u"), (F(24), b"\x1b[57387u"), (F(35), b"\x1b[57398u"),
            (KpDecimal, b"\x1b[57409u"), (KpDivide, b"\x1b[57410u"), (KpMultiply, b"\x1b[57411u"), (KpSubtract, b"\x1b[57412u"),
            (KpAdd, b"\x1b[57413u"), (KpEnter, b"\x1b[57414u"), (KpEqual, b"\x1b[57415u"), (KpSeparator, b"\x1b[57416u"),
            (KpLeft, b"\x1b[57417u"), (KpRight, b"\x1b[57418u"), (KpUp, b"\x1b[57419u"), (KpDown, b"\x1b[57420u"),
            (KpPageUp, b"\x1b[57421u"), (KpPageDown, b"\x1b[57422u"), (KpHome, b"\x1b[57423u"), (KpEnd, b"\x1b[57424u"),
            (KpInsert, b"\x1b[57425u"), (KpDelete, b"\x1b[57426u"), (KpBegin, b"\x1b[E"),
            (MediaPlay, b"\x1b[57428u"), (MediaPause, b"\x1b[57429u"), (MediaPlayPause, b"\x1b[57430u"), (MediaReverse, b"\x1b[57431u"),
            (MediaStop, b"\x1b[57432u"), (MediaFastForward, b"\x1b[57433u"), (MediaRewind, b"\x1b[57434u"),
            (MediaTrackNext, b"\x1b[57435u"), (MediaTrackPrevious, b"\x1b[57436u"), (MediaRecord, b"\x1b[57437u"),
            (LowerVolume, b"\x1b[57438u"), (RaiseVolume, b"\x1b[57439u"), (MuteVolume, b"\x1b[57440u"),
        ];
        for (f, want) in table {
            assert_eq!(show(&encode(&fk(*f, NONE, Press), d, false)), show(want), "{f:?}");
            n += 1;
        }
        for digit in 0..=9u8 {
            let want = format!("\x1b[{}u", 57399 + digit as u32);
            assert_eq!(encode(&fk(Kp(digit), NONE, Press), d, false), want.as_bytes(), "keypad {digit}");
            n += 1;
        }
        // Modifier and lock keys exist only when every key is an escape code.
        let modifiers: &[(Fk, u32)] = &[
            (CapsLock, 57358), (ScrollLock, 57359), (NumLock, 57360), (LeftShift, 57441), (LeftControl, 57442), (LeftAlt, 57443),
            (LeftSuper, 57444), (LeftHyper, 57445), (LeftMeta, 57446), (RightShift, 57447), (RightControl, 57448), (RightAlt, 57449),
            (RightSuper, 57450), (RightHyper, 57451), (RightMeta, 57452), (IsoLevel3Shift, 57453), (IsoLevel5Shift, 57454),
        ];
        for (f, num) in modifiers {
            assert_eq!(encode(&fk(*f, NONE, Press), ALL_KEYS, false), format!("\x1b[{num}u").as_bytes(), "{f:?}");
            assert!(encode(&fk(*f, NONE, Press), d, false).is_empty(), "{f:?} is silent without 'all keys'");
            n += 1;
        }
        println!("kitty specification, protocol examples: {n} of {n} pass");
    }

    /// The specification's legacy tables against what PROTEUS sends to a
    /// program that asked for nothing. Rows marked `Some(..)` are where
    /// PROTEUS differs on purpose; the value is what it sends instead.
    #[test]
    fn kitty_specification_legacy_tables() {
        const COMBOS: [u8; 7] = [NONE, CTRL, ALT, SHIFT, CTRL | SHIFT, ALT | SHIFT, CTRL | ALT];
        type Row = (&'static [u8], Option<&'static [u8]>);
        let (mut same, mut differ) = (0, 0);
        let mut run = |name: &str, key: Key, app_cursor: bool, row: Row| {
            let got = encode(&key, 0, app_cursor);
            match row.1 {
                None => {
                    assert_eq!(show(&got), show(row.0), "{name} mods {}", key.mods);
                    same += 1;
                }
                Some(ours) => {
                    assert_eq!(show(&got), show(ours), "{name} mods {} (deliberate difference)", key.mods);
                    assert_ne!(ours, row.0);
                    differ += 1;
                }
            }
        };

        // "Legacy functional keys", the C0 block: none, ctrl, alt, shift, ctrl+shift, alt+shift, ctrl+alt.
        let c0: &[(&str, Fk, [Row; 7])] = &[
            ("enter", Fk::Enter, [
                (b"\r", None), (b"\r", None), (b"\x1b\r", None),
                (b"\r", Some(b"\x1b\r")), (b"\r", Some(b"\x1b\r")), (b"\x1b\r", None), (b"\x1b\r", None),
            ]),
            ("escape", Fk::Escape, [
                (b"\x1b", None), (b"\x1b", None), (b"\x1b\x1b", None), (b"\x1b", None), (b"\x1b", None), (b"\x1b\x1b", None), (b"\x1b\x1b", None),
            ]),
            ("backspace", Fk::Backspace, [
                (b"\x7f", None), (b"\x08", None), (b"\x1b\x7f", None), (b"\x7f", None), (b"\x08", None), (b"\x1b\x7f", None), (b"\x1b\x08", None),
            ]),
            ("tab", Fk::Tab, [
                (b"\t", None), (b"\t", None), (b"\x1b\t", None), (b"\x1b[Z", None), (b"\x1b[Z", None), (b"\x1b\x1b[Z", None), (b"\x1b\t", None),
            ]),
        ];
        for (name, f, rows) in c0 {
            for (mods, row) in COMBOS.iter().zip(rows) {
                run(name, fk(*f, *mods, Press), false, *row);
            }
        }
        let space: [Row; 7] = [(b" ", None), (b"\0", None), (b"\x1b ", None), (b" ", None), (b"\0", None), (b"\x1b ", None), (b"\x1b\0", None)];
        for (mods, row) in COMBOS.iter().zip(space) {
            run("space", text_key(' ', ' ', *mods, Press), false, row);
        }

        // "Legacy text keys": the worked examples, in the table's column order:
        // none, shift, alt, ctrl, shift+alt, alt+ctrl, ctrl+shift.
        const COLUMNS: [u8; 7] = [NONE, SHIFT, ALT, CTRL, SHIFT | ALT, ALT | CTRL, CTRL | SHIFT];
        let text: &[(char, char, [Row; 7])] = &[
            ('i', 'I', [(b"i", None), (b"I", None), (b"\x1bi", None), (b"\t", None), (b"\x1bI", None), (b"\x1b\t", None), (b"\x1b[105;6u", Some(b"\t"))]),
            ('3', '#', [(b"3", None), (b"#", None), (b"\x1b3", None), (b"\x1b", None), (b"\x1b#", None), (b"\x1b\x1b", None), (b"\x1b[51;6u", Some(b"#"))]),
            (';', ':', [(b";", None), (b":", None), (b"\x1b;", None), (b";", None), (b"\x1b:", None), (b"\x1b;", None), (b"\x1b[59;6u", Some(b":"))]),
        ];
        for (c, shifted, rows) in text {
            for (mods, row) in COLUMNS.iter().zip(rows) {
                run(&c.to_string(), text_key(*c, *shifted, *mods, Press), false, *row);
            }
        }

        // The ctrl mapping table, every entry.
        let byte = |b: u8| -> &'static [u8] { Box::leak(Box::new([b])) };
        for c in 'a'..='z' {
            run("ctrl+letter", text_key(c, c.to_ascii_uppercase(), CTRL, Press), false, (byte(c as u8 - b'a' + 1), None));
        }
        let ctrl: &[(char, u8)] = &[
            (' ', 0), ('/', 31), ('0', 48), ('1', 49), ('2', 0), ('3', 27), ('4', 28), ('5', 29), ('6', 30), ('7', 31), ('8', 127), ('9', 57),
            ('?', 127), ('@', 0), ('[', 27), ('\\', 28), (']', 29), ('^', 30), ('_', 31), ('~', 30),
        ];
        for (c, want) in ctrl {
            run(&format!("ctrl+{c}"), text_key(*c, *c, CTRL, Press), false, (byte(*want), None));
        }

        // "Legacy functional keys": CSI number ~, CSI letter, SS3 letter.
        let functional: &[(Fk, bool, &[u8])] = &[
            (Fk::Insert, false, b"\x1b[2~"), (Fk::Delete, false, b"\x1b[3~"), (Fk::PageUp, false, b"\x1b[5~"), (Fk::PageDown, false, b"\x1b[6~"),
            (Fk::Up, false, b"\x1b[A"), (Fk::Down, false, b"\x1b[B"), (Fk::Right, false, b"\x1b[C"), (Fk::Left, false, b"\x1b[D"),
            (Fk::Home, false, b"\x1b[H"), (Fk::End, false, b"\x1b[F"),
            (Fk::Up, true, b"\x1bOA"), (Fk::Down, true, b"\x1bOB"), (Fk::Right, true, b"\x1bOC"), (Fk::Left, true, b"\x1bOD"),
            (Fk::Home, true, b"\x1bOH"), (Fk::End, true, b"\x1bOF"),
            (Fk::F(1), false, b"\x1bOP"), (Fk::F(2), false, b"\x1bOQ"), (Fk::F(3), false, b"\x1bOR"), (Fk::F(4), false, b"\x1bOS"),
            (Fk::F(5), false, b"\x1b[15~"), (Fk::F(6), false, b"\x1b[17~"), (Fk::F(7), false, b"\x1b[18~"), (Fk::F(8), false, b"\x1b[19~"),
            (Fk::F(9), false, b"\x1b[20~"), (Fk::F(10), false, b"\x1b[21~"), (Fk::F(11), false, b"\x1b[23~"), (Fk::F(12), false, b"\x1b[24~"),
            (Fk::Menu, false, b"\x1b[29~"),
        ];
        for (f, app_cursor, want) in functional {
            run(&format!("{f:?}"), fk(*f, NONE, Press), *app_cursor, (want, None));
        }
        // With modifiers: CSI 1;m letter and CSI number;m ~, never SS3.
        run("ctrl+f1", fk(Fk::F(1), CTRL, Press), false, (b"\x1b[1;5P", None));
        run("shift+up in cursor key mode", fk(Fk::Up, SHIFT, Press), true, (b"\x1b[1;2A", None));
        run("ctrl+delete", fk(Fk::Delete, CTRL, Press), false, (b"\x1b[3;5~", None));
        // The keypad is not a separate place in legacy mode.
        run("keypad enter", fk(Fk::KpEnter, NONE, Press), false, (b"\r", None));
        run("keypad left", fk(Fk::KpLeft, NONE, Press), false, (b"\x1b[D", None));

        println!("kitty specification, legacy tables: {same} of {} match; {differ} differ on purpose", same + differ);
        assert_eq!(differ, 5);
    }

    /// What existed before this encoder must still be sent, byte for byte.
    #[test]
    fn legacy_bytes_are_unchanged() {
        check(&[
            ("shift+enter is 'new line' to agents", fk(Fk::Enter, SHIFT, Press), 0, false, b"\x1b\r"),
            ("alt+enter", fk(Fk::Enter, ALT, Press), 0, false, b"\x1b\r"),
            ("ctrl+enter", fk(Fk::Enter, CTRL, Press), 0, false, b"\r"),
            ("shift+tab", fk(Fk::Tab, SHIFT, Press), 0, false, b"\x1b[Z"),
            ("ctrl+backspace", fk(Fk::Backspace, CTRL, Press), 0, false, b"\x08"),
            ("alt+backspace", fk(Fk::Backspace, ALT, Press), 0, false, b"\x1b\x7f"),
            ("ctrl+space", text_key(' ', ' ', CTRL, Press), 0, false, b"\0"),
            ("alt+space", text_key(' ', ' ', ALT, Press), 0, false, b"\x1b "),
            ("ctrl+c", text_key('c', 'C', CTRL, Press), 0, false, b"\x03"),
            ("ctrl+shift+c", text_key('c', 'C', CTRL | SHIFT, Press), 0, false, b"\x03"),
            ("ctrl+[", text_key('[', '{', CTRL, Press), 0, false, b"\x1b"),
            ("ctrl+-", text_key('-', '_', CTRL, Press), 0, false, b"\x1f"),
            ("ctrl+alt+c", text_key('c', 'C', CTRL | ALT, Press), 0, false, b"\x1b\x03"),
            ("alt+x", text_key('x', 'X', ALT, Press), 0, false, b"\x1bx"),
            ("alt+shift+x", text_key('x', 'X', ALT | SHIFT, Press), 0, false, b"\x1bX"),
            ("ctrl+right", fk(Fk::Right, CTRL, Press), 0, false, b"\x1b[1;5C"),
            ("ctrl+right, cursor key mode", fk(Fk::Right, CTRL, Press), 0, true, b"\x1b[1;5C"),
            ("home, cursor key mode", fk(Fk::Home, NONE, Press), 0, true, b"\x1bOH"),
            ("shift+f5", fk(Fk::F(5), SHIFT, Press), 0, false, b"\x1b[15;2~"),
            ("alt+f4 style", fk(Fk::F(4), ALT, Press), 0, false, b"\x1b[1;3S"),
            ("a held down repeats", text_key('a', 'A', NONE, Repeat), 0, false, b"a"),
            ("nothing on release", text_key('a', 'A', NONE, Release), 0, false, b""),
            ("modifier keys alone are silent", fk(Fk::LeftShift, SHIFT, Press), 0, false, b""),
        ]);
        // AltGr: Ctrl+Alt that typed a character of its own is that character.
        let at = Key { code: Code::Char('q'), shifted: None, base: None, text: "@", mods: CTRL | ALT, action: Press };
        assert_eq!(encode(&at, 0, false), b"@");
        assert_eq!(encode(&at, DISAMBIGUATE, false), b"@");
        // Text from an input method or a dead key has no key of its own.
        let composed = Key { code: Code::Char('\0'), shifted: None, base: None, text: "\u{e9}", mods: NONE, action: Press };
        assert_eq!(encode(&composed, 0, false), "\u{e9}".as_bytes());
        assert_eq!(encode(&composed, ALL_KEYS, false), "\u{e9}".as_bytes());
        assert_eq!(ctrl_code('C'), Some(0x03));
        assert_eq!(ctrl_code('\u{e9}'), None);
    }

    /// Windows hands over "the text without Ctrl" with a Ctrl chord, and
    /// Alt+a arrives carrying "a". The protocol attaches text only to key
    /// events that generate text: a chord must not arrive saying it typed a
    /// letter, or a program that inserts the text it is given types `a` for
    /// every Alt+a and Ctrl+a.
    #[test]
    fn a_chord_carries_no_text_even_when_the_window_system_reports_some() {
        let all_text = ALL_KEYS | ASSOCIATED_TEXT;
        let with = |c: char, shifted: Option<char>, text: &'static str, mods: u8| Key { code: Code::Char(c), shifted, base: None, text, mods, action: Press };
        check(&[
            ("ctrl+a reported with 'a'", with('a', None, "a", CTRL), all_text, false, b"\x1b[97;5u"),
            ("alt+a reported with 'a'", with('a', None, "a", ALT), all_text, false, b"\x1b[97;3u"),
            ("super+a reported with 'a'", with('a', None, "a", SUPER), all_text, false, b"\x1b[97;9u"),
            ("ctrl+shift+a reported with 'A'", with('a', Some('A'), "A", CTRL | SHIFT), 31, false, b"\x1b[97:65;6u"),
            ("alt+shift+a reported with 'A'", with('a', Some('A'), "A", ALT | SHIFT), all_text, false, b"\x1b[97;4u"),
            ("ctrl+alt+a reported with 'a' is a chord", with('a', None, "a", CTRL | ALT), all_text, false, b"\x1b[97;7u"),
            ("the same without 'all keys'", with('a', None, "a", CTRL), DISAMBIGUATE | ASSOCIATED_TEXT, false, b"\x1b[97;5u"),
            ("held: a repeat carries none either", Key { action: Repeat, ..with('a', None, "a", CTRL) }, all_text | EVENT_TYPES, false, b"\x1b[97;5:2u"),
            // Typing still carries its text: Shift alone, and AltGr (Ctrl+Alt that typed a character of its own).
            ("shift+a", with('a', Some('A'), "A", SHIFT), all_text, false, b"\x1b[97;2;65u"),
            ("altgr+q types @", with('q', None, "@", CTRL | ALT), all_text, false, b"\x1b[113;;64u"),
            ("caps lock is not a chord", with('a', None, "A", CAPS_LOCK), all_text, false, b"\x1b[97;65;65u"),
            // And none of this changes what a program that asked for nothing is sent.
            ("legacy ctrl+a", with('a', None, "a", CTRL), 0, false, b"\x01"),
            ("legacy alt+a", with('a', None, "a", ALT), 0, false, b"\x1ba"),
            ("disambiguate alt+a", with('a', None, "a", ALT), DISAMBIGUATE, false, b"\x1b[97;3u"),
        ]);
    }

    /// On a layout whose letters are not Latin, Ctrl acts on the key's
    /// position: the key that types Cyrillic `с` is where `c` is, and
    /// Ctrl with it must interrupt, not type the letter.
    #[test]
    fn ctrl_reaches_the_latin_key_under_a_non_latin_letter() {
        let on = |c: char, shifted: Option<char>, base: char, text: &'static str, mods: u8| Key { code: Code::Char(c), shifted, base: Some(base), text, mods, action: Press };
        check(&[
            ("ctrl+с (Cyrillic, where c is)", on('\u{441}', None, 'c', "\u{441}", CTRL), 0, false, b"\x03"),
            ("the same with no text reported", on('\u{441}', None, 'c', "", CTRL), 0, false, b"\x03"),
            ("ctrl+shift+С", on('\u{441}', Some('\u{421}'), 'c', "\u{421}", CTRL | SHIFT), 0, false, b"\x03"),
            ("ctrl+alt+с", on('\u{441}', None, 'c', "", CTRL | ALT), 0, false, b"\x1b\x03"),
            ("ctrl+α (Greek, where a is)", on('\u{3b1}', None, 'a', "\u{3b1}", CTRL), 0, false, b"\x01"),
            ("ctrl+х (Cyrillic, where [ is) is escape", on('\u{445}', None, '[', "\u{445}", CTRL), 0, false, b"\x1b"),
            ("ctrl+ж (where ; is): Ctrl has no code for ;", on('\u{436}', None, ';', "\u{436}", CTRL), 0, false, b";"),
            ("events only: still a control code", on('\u{441}', None, 'c', "\u{441}", CTRL), EVENT_TYPES, false, b"\x03"),
            // A Latin layout that moves letters about (Dvorak has o where s is)
            // goes by the letter, as before.
            ("ctrl+o on Dvorak", on('o', None, 's', "o", CTRL), 0, false, b"\x0f"),
            // Without Ctrl the letter is typed, and Alt prefixes it, as before.
            ("с alone", on('\u{441}', None, 'c', "\u{441}", NONE), 0, false, "\u{441}".as_bytes()),
            ("alt+с", on('\u{441}', None, 'c', "\u{441}", ALT), 0, false, "\x1b\u{441}".as_bytes()),
            // A program that asked for the protocol gets the key as it is, with its position on request.
            ("ctrl+с, disambiguate", on('\u{441}', None, 'c', "\u{441}", CTRL), DISAMBIGUATE, false, b"\x1b[1089;5u"),
            ("ctrl+с, alternates", on('\u{441}', None, 'c', "\u{441}", CTRL), DISAMBIGUATE | ALTERNATE_KEYS, false, b"\x1b[1089::99;5u"),
        ]);
        // A key the position table does not know stays as it was.
        let unknown = Key { code: Code::Char('\u{441}'), shifted: None, base: None, text: "", mods: CTRL, action: Press };
        assert_eq!(encode(&unknown, 0, false), "\u{441}".as_bytes());
    }

    /// What agent interfaces lean on once they have asked for the protocol.
    #[test]
    fn chords_agents_depend_on() {
        let d = DISAMBIGUATE;
        check(&[
            ("shift+enter", fk(Fk::Enter, SHIFT, Press), d, false, b"\x1b[13;2u"),
            ("ctrl+enter", fk(Fk::Enter, CTRL, Press), d, false, b"\x1b[13;5u"),
            ("alt+enter", fk(Fk::Enter, ALT, Press), d, false, b"\x1b[13;3u"),
            ("ctrl+shift+enter", fk(Fk::Enter, CTRL | SHIFT, Press), d, false, b"\x1b[13;6u"),
            ("shift+tab", fk(Fk::Tab, SHIFT, Press), d, false, b"\x1b[9;2u"),
            ("ctrl+backspace", fk(Fk::Backspace, CTRL, Press), d, false, b"\x1b[127;5u"),
            ("ctrl+i is not tab", text_key('i', 'I', CTRL, Press), d, false, b"\x1b[105;5u"),
            ("ctrl+m is not enter", text_key('m', 'M', CTRL, Press), d, false, b"\x1b[109;5u"),
            ("ctrl+[ is not escape", text_key('[', '{', CTRL, Press), d, false, b"\x1b[91;5u"),
            ("ctrl+shift+p", text_key('p', 'P', CTRL | SHIFT, Press), d, false, b"\x1b[112;6u"),
            ("alt+shift+f", text_key('f', 'F', ALT | SHIFT, Press), d, false, b"\x1b[102;4u"),
            ("ctrl+space", text_key(' ', ' ', CTRL, Press), d, false, b"\x1b[32;5u"),
            ("super+k", text_key('k', 'K', SUPER, Press), d, false, b"\x1b[107;9u"),
            ("ctrl+up", fk(Fk::Up, CTRL, Press), d, false, b"\x1b[1;5A"),
            ("up ignores cursor key mode", fk(Fk::Up, NONE, Press), d, true, b"\x1b[A"),
            ("shift+f5", fk(Fk::F(5), SHIFT, Press), d, false, b"\x1b[15;2~"),
            ("keypad enter is its own key", fk(Fk::KpEnter, NONE, Press), d, false, b"\x1b[57414u"),
            ("a held, events on", text_key('a', 'A', NONE, Repeat), d | EVENT_TYPES, false, b"a"),
            ("shift+enter released: nothing", fk(Fk::Enter, SHIFT, Release), d | EVENT_TYPES, false, b""),
            ("events only: ctrl+a is still a control code", text_key('a', 'A', CTRL, Press), EVENT_TYPES, false, b"\x01"),
            ("events only: its release is reported", text_key('a', 'A', CTRL, Release), EVENT_TYPES, false, b"\x1b[97;5:3u"),
            ("events only: escape is still escape", fk(Fk::Escape, NONE, Press), EVENT_TYPES, false, b"\x1b"),
            ("all keys with text", text_key('a', 'A', NONE, Press), ALL_KEYS | ASSOCIATED_TEXT, false, b"\x1b[97;;97u"),
            ("all keys, alternates and text", text_key('a', 'A', SHIFT, Press), 31, false, b"\x1b[97:65;2;65u"),
            // The specification is silent here; PROTEUS keeps Enter usable
            // with Num Lock on, for the same reason Enter stays legacy at all.
            ("num lock does not change enter", fk(Fk::Enter, NUM_LOCK, Press), d, false, b"\r"),
        ]);
        // A keypad digit types its digit until every key is an escape code.
        let kp1 = Key { code: Code::Fn(Fk::Kp(1)), shifted: None, base: None, text: "1", mods: NONE, action: Press };
        assert_eq!(encode(&kp1, 0, false), b"1");
        assert_eq!(encode(&kp1, d, false), b"1");
        assert_eq!(encode(&kp1, ALL_KEYS, false), b"\x1b[57400u");
        assert_eq!(encode(&kp1, ALL_KEYS | ASSOCIATED_TEXT, false), b"\x1b[57400;;49u");
        let ctrl_kp1 = Key { text: "", mods: CTRL, ..kp1 };
        assert_eq!(encode(&ctrl_kp1, d, false), b"\x1b[57400;5u");
        assert_eq!(encode(&ctrl_kp1, 0, false), b"1");
    }
}
