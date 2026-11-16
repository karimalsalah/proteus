//! Turning key, paste and mouse events into the bytes a terminal program expects.
//!
//! The rules for keys live in `keys.rs`, which knows nothing about windows;
//! this file only describes a window-system key event in those terms.
use alacritty_terminal::term::TermMode;
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, KeyCode, KeyLocation, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

use crate::keys::{self, Action, Code, Fk};

/// The kitty keyboard protocol flags the program has switched on.
pub fn kitty_flags(mode: TermMode) -> u8 {
    let bit = |m: TermMode, f: u8| if mode.contains(m) { f } else { 0 };
    bit(TermMode::DISAMBIGUATE_ESC_CODES, keys::DISAMBIGUATE)
        | bit(TermMode::REPORT_EVENT_TYPES, keys::EVENT_TYPES)
        | bit(TermMode::REPORT_ALTERNATE_KEYS, keys::ALTERNATE_KEYS)
        | bit(TermMode::REPORT_ALL_KEYS_AS_ESC, keys::ALL_KEYS)
        | bit(TermMode::REPORT_ASSOCIATED_TEXT, keys::ASSOCIATED_TEXT)
}

/// Fed to the terminal's own parser (not sent to the program) to drop every
/// keyboard-protocol flag on the current screen: the way out when a program
/// died with the protocol on and left every chord arriving as an escape code.
pub const DROP_KEYBOARD_FLAGS: &[u8] = b"\x1b[<4096u";

/// Shift, Ctrl, Alt, the Windows key and the lock keys: keys that are held
/// or toggled rather than typed.
pub fn is_modifier(ev: &KeyEvent) -> bool {
    matches!(named(&ev.logical_key, ev.location), Some(Code::Fn(f)) if f.is_modifier())
}

const F_KEYS: [NamedKey; 35] = [
    NamedKey::F1,
    NamedKey::F2,
    NamedKey::F3,
    NamedKey::F4,
    NamedKey::F5,
    NamedKey::F6,
    NamedKey::F7,
    NamedKey::F8,
    NamedKey::F9,
    NamedKey::F10,
    NamedKey::F11,
    NamedKey::F12,
    NamedKey::F13,
    NamedKey::F14,
    NamedKey::F15,
    NamedKey::F16,
    NamedKey::F17,
    NamedKey::F18,
    NamedKey::F19,
    NamedKey::F20,
    NamedKey::F21,
    NamedKey::F22,
    NamedKey::F23,
    NamedKey::F24,
    NamedKey::F25,
    NamedKey::F26,
    NamedKey::F27,
    NamedKey::F28,
    NamedKey::F29,
    NamedKey::F30,
    NamedKey::F31,
    NamedKey::F32,
    NamedKey::F33,
    NamedKey::F34,
    NamedKey::F35,
];

fn named(key: &Key, loc: KeyLocation) -> Option<Code> {
    let Key::Named(n) = key else { return None };
    let pad = loc == KeyLocation::Numpad;
    let side = |l: Fk, r: Fk| if loc == KeyLocation::Right { r } else { l };
    let on_pad = |plain: Fk, keypad: Fk| if pad { keypad } else { plain };
    Some(Code::Fn(match n {
        NamedKey::Space => return Some(Code::Char(' ')),
        NamedKey::Enter => on_pad(Fk::Enter, Fk::KpEnter),
        NamedKey::Tab => Fk::Tab,
        NamedKey::Backspace => Fk::Backspace,
        NamedKey::Escape => Fk::Escape,
        NamedKey::Insert => on_pad(Fk::Insert, Fk::KpInsert),
        NamedKey::Delete => on_pad(Fk::Delete, Fk::KpDelete),
        NamedKey::ArrowLeft => on_pad(Fk::Left, Fk::KpLeft),
        NamedKey::ArrowRight => on_pad(Fk::Right, Fk::KpRight),
        NamedKey::ArrowUp => on_pad(Fk::Up, Fk::KpUp),
        NamedKey::ArrowDown => on_pad(Fk::Down, Fk::KpDown),
        NamedKey::PageUp => on_pad(Fk::PageUp, Fk::KpPageUp),
        NamedKey::PageDown => on_pad(Fk::PageDown, Fk::KpPageDown),
        NamedKey::Home => on_pad(Fk::Home, Fk::KpHome),
        NamedKey::End => on_pad(Fk::End, Fk::KpEnd),
        NamedKey::Clear if pad => Fk::KpBegin,
        NamedKey::CapsLock => Fk::CapsLock,
        NamedKey::ScrollLock => Fk::ScrollLock,
        NamedKey::NumLock => Fk::NumLock,
        NamedKey::PrintScreen => Fk::PrintScreen,
        NamedKey::Pause => Fk::Pause,
        NamedKey::ContextMenu => Fk::Menu,
        NamedKey::Shift => side(Fk::LeftShift, Fk::RightShift),
        NamedKey::Control => side(Fk::LeftControl, Fk::RightControl),
        NamedKey::Alt => side(Fk::LeftAlt, Fk::RightAlt),
        NamedKey::Super => side(Fk::LeftSuper, Fk::RightSuper),
        NamedKey::Hyper => side(Fk::LeftHyper, Fk::RightHyper),
        NamedKey::Meta => side(Fk::LeftMeta, Fk::RightMeta),
        NamedKey::AltGraph => Fk::IsoLevel3Shift,
        NamedKey::MediaPlay => Fk::MediaPlay,
        NamedKey::MediaPause => Fk::MediaPause,
        NamedKey::MediaPlayPause => Fk::MediaPlayPause,
        NamedKey::MediaStop => Fk::MediaStop,
        NamedKey::MediaFastForward => Fk::MediaFastForward,
        NamedKey::MediaRewind => Fk::MediaRewind,
        NamedKey::MediaTrackNext => Fk::MediaTrackNext,
        NamedKey::MediaTrackPrevious => Fk::MediaTrackPrevious,
        NamedKey::MediaRecord => Fk::MediaRecord,
        NamedKey::AudioVolumeDown => Fk::LowerVolume,
        NamedKey::AudioVolumeUp => Fk::RaiseVolume,
        NamedKey::AudioVolumeMute => Fk::MuteVolume,
        other => Fk::F(F_KEYS.iter().position(|f| f == other)? as u8 + 1),
    }))
}

/// What the physical key types on a US keyboard, whatever layout is active.
fn us_layout(key: PhysicalKey) -> Option<char> {
    let PhysicalKey::Code(code) = key else { return None };
    Some(match code {
        KeyCode::KeyA => 'a',
        KeyCode::KeyB => 'b',
        KeyCode::KeyC => 'c',
        KeyCode::KeyD => 'd',
        KeyCode::KeyE => 'e',
        KeyCode::KeyF => 'f',
        KeyCode::KeyG => 'g',
        KeyCode::KeyH => 'h',
        KeyCode::KeyI => 'i',
        KeyCode::KeyJ => 'j',
        KeyCode::KeyK => 'k',
        KeyCode::KeyL => 'l',
        KeyCode::KeyM => 'm',
        KeyCode::KeyN => 'n',
        KeyCode::KeyO => 'o',
        KeyCode::KeyP => 'p',
        KeyCode::KeyQ => 'q',
        KeyCode::KeyR => 'r',
        KeyCode::KeyS => 's',
        KeyCode::KeyT => 't',
        KeyCode::KeyU => 'u',
        KeyCode::KeyV => 'v',
        KeyCode::KeyW => 'w',
        KeyCode::KeyX => 'x',
        KeyCode::KeyY => 'y',
        KeyCode::KeyZ => 'z',
        KeyCode::Digit0 => '0',
        KeyCode::Digit1 => '1',
        KeyCode::Digit2 => '2',
        KeyCode::Digit3 => '3',
        KeyCode::Digit4 => '4',
        KeyCode::Digit5 => '5',
        KeyCode::Digit6 => '6',
        KeyCode::Digit7 => '7',
        KeyCode::Digit8 => '8',
        KeyCode::Digit9 => '9',
        KeyCode::Minus => '-',
        KeyCode::Equal => '=',
        KeyCode::BracketLeft => '[',
        KeyCode::BracketRight => ']',
        KeyCode::Backslash => '\\',
        KeyCode::Semicolon => ';',
        KeyCode::Quote => '\'',
        KeyCode::Backquote => '`',
        KeyCode::Comma => ',',
        KeyCode::Period => '.',
        KeyCode::Slash => '/',
        _ => return None,
    })
}

/// The bytes for one key event, or `None` when the program is sent nothing
/// (a key coming up when it did not ask to hear about releases, a modifier
/// pressed by itself, a dead key waiting for its letter).
///
/// Caps Lock and Num Lock are never reported as modifiers: a program that
/// compares the modifier field exactly would stop recognising Shift+Enter
/// whenever Num Lock is on.
pub fn encode_key(ev: &KeyEvent, mods: ModifiersState, mode: TermMode) -> Option<Vec<u8>> {
    let action = match (ev.state, ev.repeat) {
        (ElementState::Released, _) => Action::Release,
        (ElementState::Pressed, true) => Action::Repeat,
        (ElementState::Pressed, false) => Action::Press,
    };
    let parts = Parts {
        logical: &ev.logical_key,
        bare: &ev.key_without_modifiers(),
        location: ev.location,
        physical: ev.physical_key,
        text: ev.text.as_ref().map(|t| t.as_str()),
        action,
    };
    encode_parts(&parts, mods, mode)
}

/// What a window key event says, field by field. Separate from the event
/// itself only because the event type cannot be built outside the window
/// library, and this needs to be tested.
struct Parts<'a> {
    /// The key with Shift, Caps Lock and AltGr applied (but not Ctrl).
    logical: &'a Key,
    /// The key with nothing held.
    bare: &'a Key,
    location: KeyLocation,
    physical: PhysicalKey,
    text: Option<&'a str>,
    action: Action,
}

fn encode_parts(ev: &Parts, mods: ModifiersState, mode: TermMode) -> Option<Vec<u8>> {
    let shift = mods.shift_key();
    let m = if shift { keys::SHIFT } else { 0 }
        | if mods.alt_key() { keys::ALT } else { 0 }
        | if mods.control_key() { keys::CTRL } else { 0 }
        | if mods.super_key() { keys::SUPER } else { 0 };
    let action = ev.action;
    let text = ev.text.filter(|t| !t.chars().any(|c| c.is_control())).unwrap_or("");
    let (code, shifted) = match ev.logical {
        Key::Character(s) => {
            // `logical` is what the key types with Shift, Caps Lock and AltGr
            // applied; the key's own name is what it types with nothing held.
            let typed = s.chars().next()?;
            let bare = match ev.bare {
                Key::Character(u) => u.chars().next().unwrap_or(typed),
                _ => typed,
            };
            let bare = bare.to_lowercase().next().unwrap_or(bare);
            let code = match (ev.location, typed) {
                (KeyLocation::Numpad, '0'..='9') => Code::Fn(Fk::Kp(typed as u8 - b'0')),
                (KeyLocation::Numpad, '.' | ',') => Code::Fn(Fk::KpDecimal),
                (KeyLocation::Numpad, '/') => Code::Fn(Fk::KpDivide),
                (KeyLocation::Numpad, '*') => Code::Fn(Fk::KpMultiply),
                (KeyLocation::Numpad, '-') => Code::Fn(Fk::KpSubtract),
                (KeyLocation::Numpad, '+') => Code::Fn(Fk::KpAdd),
                (KeyLocation::Numpad, '=') => Code::Fn(Fk::KpEqual),
                _ => Code::Char(bare),
            };
            (code, (shift && typed != bare).then_some(typed))
        }
        named_key @ Key::Named(_) => (named(named_key, ev.location)?, None),
        // A dead key pressed twice (the usual way to type a bare `^` or `~`
        // on many layouts) arrives as the dead key itself, carrying the text
        // that finally came out. That text belongs to no single key.
        Key::Dead(_) if !text.is_empty() => (Code::Char('\0'), None),
        // Ctrl+Alt with a key the layout gives no third character to (every
        // letter on a US keyboard): Windows has no character for the chord,
        // so the window library cannot name the key. The key with nothing
        // held is still the key.
        Key::Unidentified(_) if mods.control_key() && mods.alt_key() => match ev.bare {
            Key::Character(u) => {
                let bare = u.chars().next()?;
                (Code::Char(bare.to_lowercase().next().unwrap_or(bare)), None)
            }
            _ => return None,
        },
        _ => return None,
    };
    // Windows repeats a held modifier key like any other; no other system
    // does, and a program that asked for every key does not expect a stream
    // of Shift presses for as long as Shift is down.
    if action == Action::Repeat && matches!(code, Code::Fn(f) if f.is_modifier()) {
        return None;
    }
    let key = keys::Key { code, shifted, base: us_layout(ev.physical), text, mods: m, action };
    let bytes = keys::encode(&key, kitty_flags(mode), mode.contains(TermMode::APP_CURSOR));
    (!bytes.is_empty()).then_some(bytes)
}

/// Text that came from no single key (an input method's commit).
pub fn encode_text(text: &str, mode: TermMode) -> Vec<u8> {
    let key = keys::Key { code: Code::Char('\0'), shifted: None, base: None, text, mods: 0, action: Action::Press };
    keys::encode(&key, kitty_flags(mode), false)
}

/// Wrap pasted text. With bracketed paste the program can tell paste from
/// typing; the end marker is stripped from the payload so pasted text can
/// never break out of the bracket.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let clean = text.replace("\x1b[201~", "").replace('\x1b', "");
        let mut v = b"\x1b[200~".to_vec();
        v.extend_from_slice(clean.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Mouse report. `button`: 0 left, 1 middle, 2 right, 64/65 wheel up/down;
/// add 32 for motion. Coordinates are zero-based cells.
pub fn encode_mouse(button: u8, col: usize, row: usize, pressed: bool, sgr: bool) -> Option<Vec<u8>> {
    if sgr {
        let fin = if pressed { 'M' } else { 'm' };
        Some(format!("\x1b[<{};{};{}{}", button, col + 1, row + 1, fin).into_bytes())
    } else {
        if col >= 223 || row >= 223 {
            return None;
        }
        let b = if pressed { button } else { 3 };
        Some(vec![0x1b, b'[', b'M', 32 + b, 32 + 1 + col as u8, 32 + 1 + row as u8])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl_code(s: &str) -> Option<u8> {
        keys::ctrl_code(s.chars().next()?)
    }

    #[test]
    fn ctrl_codes() {
        assert_eq!(ctrl_code("c"), Some(0x03));
        assert_eq!(ctrl_code("C"), Some(0x03));
        assert_eq!(ctrl_code("["), Some(0x1b));
        assert_eq!(ctrl_code("é"), None);
    }

    #[test]
    fn kitty_flags_follow_the_terminal_mode() {
        assert_eq!(kitty_flags(TermMode::NONE), 0);
        assert_eq!(kitty_flags(TermMode::DISAMBIGUATE_ESC_CODES | TermMode::APP_CURSOR), 1);
        assert_eq!(kitty_flags(TermMode::KITTY_KEYBOARD_PROTOCOL), 31);
        // An input method's text is plain text unless the program asked for it as a code.
        assert_eq!(encode_text("\u{e9}", TermMode::NONE), "\u{e9}".as_bytes());
        assert_eq!(encode_text("\u{e9}", TermMode::REPORT_ALL_KEYS_AS_ESC | TermMode::REPORT_ASSOCIATED_TEXT), b"\x1b[0;;233u");
    }

    /// Window key events as the window library reports them on Windows,
    /// through to bytes. (The event type itself cannot be built in a test;
    /// these are its fields.)
    #[test]
    fn window_key_events_become_the_right_bytes() {
        use winit::keyboard::SmolStr;
        let ch = |s: &str| Key::Character(SmolStr::new(s));
        let none = ModifiersState::empty();
        let (shift, ctrl, alt) = (ModifiersState::SHIFT, ModifiersState::CONTROL, ModifiersState::ALT);
        let legacy = TermMode::NONE;
        let kitty = TermMode::DISAMBIGUATE_ESC_CODES;
        let everything = TermMode::KITTY_KEYBOARD_PROTOCOL;
        struct Ev(Key, Key, KeyLocation, KeyCode, Option<&'static str>, Action);
        let run = |e: &Ev, mods: ModifiersState, mode: TermMode| -> String {
            let parts = Parts { logical: &e.0, bare: &e.1, location: e.2, physical: PhysicalKey::Code(e.3), text: e.4, action: e.5 };
            crate::probe::show(&encode_parts(&parts, mods, mode).unwrap_or_default())
        };
        let std = KeyLocation::Standard;
        let pad = KeyLocation::Numpad;
        let fnkey = |n: NamedKey, loc, code, text| Ev(Key::Named(n), Key::Named(n), loc, code, text, Action::Press);

        // Letters: plain, shifted, under Caps Lock, with Ctrl, with Alt.
        let a = Ev(ch("a"), ch("a"), std, KeyCode::KeyA, Some("a"), Action::Press);
        let big_a = Ev(ch("A"), ch("a"), std, KeyCode::KeyA, Some("A"), Action::Press);
        let ctrl_a = Ev(ch("a"), ch("a"), std, KeyCode::KeyA, Some("\u{1}"), Action::Press);
        let ctrl_shift_p = Ev(ch("P"), ch("p"), std, KeyCode::KeyP, Some("\u{10}"), Action::Press);
        assert_eq!(run(&a, none, legacy), "a");
        assert_eq!(run(&big_a, shift, legacy), "A");
        assert_eq!(run(&big_a, none, legacy), "A", "caps lock");
        assert_eq!(run(&ctrl_a, ctrl, legacy), "\\x01");
        assert_eq!(run(&a, alt, legacy), "\\ea");
        assert_eq!(run(&ctrl_shift_p, ctrl | shift, legacy), "\\x10");
        assert_eq!(run(&a, none, kitty), "a");
        assert_eq!(run(&big_a, shift, kitty), "A");
        assert_eq!(run(&ctrl_a, ctrl, kitty), "\\e[97;5u");
        assert_eq!(run(&a, alt, kitty), "\\e[97;3u");
        assert_eq!(run(&ctrl_shift_p, ctrl | shift, kitty), "\\e[112;6u");
        assert_eq!(run(&big_a, shift, everything), "\\e[97:65;2;65u");
        // A shifted symbol: the key is `3`, what it types is `#`.
        let hash = Ev(ch("#"), ch("3"), std, KeyCode::Digit3, Some("#"), Action::Press);
        assert_eq!(run(&hash, shift, legacy), "#");
        assert_eq!(run(&hash, ctrl | shift, kitty), "\\e[51;6u");

        // Enter and its chords: the reason this file exists.
        let enter = fnkey(NamedKey::Enter, std, KeyCode::Enter, Some("\r"));
        assert_eq!(run(&enter, none, legacy), "\\x0d");
        assert_eq!(run(&enter, shift, legacy), "\\e\\x0d");
        assert_eq!(run(&enter, alt, legacy), "\\e\\x0d");
        assert_eq!(run(&enter, ctrl, legacy), "\\x0d");
        assert_eq!(run(&enter, none, kitty), "\\x0d");
        assert_eq!(run(&enter, shift, kitty), "\\e[13;2u");
        assert_eq!(run(&enter, ctrl, kitty), "\\e[13;5u");
        assert_eq!(run(&enter, ctrl | shift, kitty), "\\e[13;6u");
        let pad_enter = fnkey(NamedKey::Enter, pad, KeyCode::NumpadEnter, Some("\r"));
        assert_eq!(run(&pad_enter, none, legacy), "\\x0d");
        assert_eq!(run(&pad_enter, none, kitty), "\\e[57414u");

        // Other keys that are not text.
        let space = fnkey(NamedKey::Space, std, KeyCode::Space, Some(" "));
        let tab = fnkey(NamedKey::Tab, std, KeyCode::Tab, Some("\t"));
        let back = fnkey(NamedKey::Backspace, std, KeyCode::Backspace, Some("\u{8}"));
        let esc = fnkey(NamedKey::Escape, std, KeyCode::Escape, Some("\u{1b}"));
        let up = fnkey(NamedKey::ArrowUp, std, KeyCode::ArrowUp, None);
        let f5 = fnkey(NamedKey::F5, std, KeyCode::F5, None);
        assert_eq!(run(&space, none, legacy), " ");
        assert_eq!(run(&space, ctrl, legacy), "\\x00");
        assert_eq!(run(&space, ctrl, kitty), "\\e[32;5u");
        assert_eq!(run(&tab, none, legacy), "\\x09");
        assert_eq!(run(&tab, shift, legacy), "\\e[Z");
        assert_eq!(run(&tab, shift, kitty), "\\e[9;2u");
        assert_eq!(run(&back, none, legacy), "\\x7f");
        assert_eq!(run(&back, ctrl, legacy), "\\x08");
        assert_eq!(run(&back, none, kitty), "\\x7f");
        assert_eq!(run(&esc, none, legacy), "\\e");
        assert_eq!(run(&esc, none, kitty), "\\e[27u");
        assert_eq!(run(&up, none, legacy), "\\e[A");
        assert_eq!(run(&up, none, TermMode::APP_CURSOR), "\\eOA");
        assert_eq!(run(&up, ctrl, legacy), "\\e[1;5A");
        assert_eq!(run(&f5, shift, legacy), "\\e[15;2~");

        // The keypad: digits type digits; with Num Lock off they are cursor keys.
        let pad1 = Ev(ch("1"), ch("1"), pad, KeyCode::Numpad1, Some("1"), Action::Press);
        let pad_end = fnkey(NamedKey::End, pad, KeyCode::Numpad1, None);
        assert_eq!(run(&pad1, none, legacy), "1");
        assert_eq!(run(&pad1, none, kitty), "1");
        assert_eq!(run(&pad1, none, everything), "\\e[57400;;49u");
        assert_eq!(run(&pad_end, none, legacy), "\\e[F");
        assert_eq!(run(&pad_end, none, kitty), "\\e[57424u");

        // Layouts: a dead key gives nothing, then the composed letter; AltGr
        // types a character; a Cyrillic key reports its Latin position.
        let dead = Ev(Key::Dead(Some('\u{b4}')), Key::Dead(Some('\u{b4}')), std, KeyCode::Quote, None, Action::Press);
        let e_acute = Ev(ch("\u{e9}"), ch("e"), std, KeyCode::KeyE, Some("\u{e9}"), Action::Press);
        let at = Ev(ch("@"), ch("q"), std, KeyCode::KeyQ, Some("@"), Action::Press);
        let cyrillic = Ev(ch("\u{441}"), ch("\u{441}"), std, KeyCode::KeyC, None, Action::Press);
        assert_eq!(run(&dead, none, legacy), "");
        assert_eq!(run(&e_acute, none, legacy), "\\xc3\\xa9");
        assert_eq!(run(&e_acute, none, kitty), "\\xc3\\xa9");
        assert_eq!(run(&at, none, legacy), "@");
        assert_eq!(run(&at, ctrl | alt, legacy), "@", "AltGr reported as Ctrl+Alt");
        assert_eq!(run(&at, ctrl | alt, kitty), "@");
        assert_eq!(run(&cyrillic, ctrl, TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALTERNATE_KEYS), "\\e[1089::99;5u");

        // Releases, repeats and modifiers by themselves.
        let a_up = Ev(ch("a"), ch("a"), std, KeyCode::KeyA, None, Action::Release);
        let a_again = Ev(ch("a"), ch("a"), std, KeyCode::KeyA, Some("a"), Action::Repeat);
        let lshift = Ev(Key::Named(NamedKey::Shift), Key::Named(NamedKey::Shift), KeyLocation::Left, KeyCode::ShiftLeft, None, Action::Press);
        let rctrl_up = Ev(Key::Named(NamedKey::Control), Key::Named(NamedKey::Control), KeyLocation::Right, KeyCode::ControlRight, None, Action::Release);
        let events = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_EVENT_TYPES;
        assert_eq!(run(&a_up, none, legacy), "");
        assert_eq!(run(&a_up, none, kitty), "");
        assert_eq!(run(&a_up, none, events), "\\e[97;1:3u");
        assert_eq!(run(&a_again, none, legacy), "a");
        assert_eq!(run(&a_again, none, events), "a");
        assert_eq!(run(&lshift, shift, legacy), "");
        assert_eq!(run(&lshift, shift, kitty), "");
        assert_eq!(run(&lshift, shift, everything), "\\e[57441;2u");
        assert_eq!(run(&rctrl_up, none, everything), "\\e[57448;1:3u");
        assert!(named(&Key::Named(NamedKey::Shift), KeyLocation::Left).is_some_and(|c| matches!(c, Code::Fn(f) if f.is_modifier())));
        assert_eq!(us_layout(PhysicalKey::Code(KeyCode::Semicolon)), Some(';'));
    }

    /// Events in the shapes the window library really produces on Windows
    /// (winit 0.30, `platform_impl/windows/keyboard.rs`), which are not the
    /// shapes one would guess:
    ///  - with Ctrl held, `text` is "the text without Ctrl" (`a`, not U+0001);
    ///  - Ctrl+Alt+letter on a layout without AltGr has no character at all
    ///    (`ToUnicodeEx` returns nothing), so the key arrives unidentified;
    ///  - a dead key pressed twice arrives as the dead key carrying `^^`;
    ///  - a held modifier key repeats.
    #[test]
    fn window_events_in_the_shapes_windows_gives_them() {
        use winit::keyboard::{NativeKey, SmolStr};
        let ch = |s: &str| Key::Character(SmolStr::new(s));
        let (none, shift, ctrl, alt) = (ModifiersState::empty(), ModifiersState::SHIFT, ModifiersState::CONTROL, ModifiersState::ALT);
        let legacy = TermMode::NONE;
        let kitty = TermMode::DISAMBIGUATE_ESC_CODES;
        let everything = TermMode::KITTY_KEYBOARD_PROTOCOL;
        let run = |logical: &Key, bare: &Key, code: KeyCode, text: Option<&str>, action: Action, mods: ModifiersState, mode: TermMode| -> String {
            let parts = Parts { logical, bare, location: KeyLocation::Standard, physical: PhysicalKey::Code(code), text, action };
            crate::probe::show(&encode_parts(&parts, mods, mode).unwrap_or_default())
        };
        let press = Action::Press;

        // Ctrl+A: the event says it typed "a". It is a chord, with no text.
        let a = ch("a");
        assert_eq!(run(&a, &a, KeyCode::KeyA, Some("a"), press, ctrl, legacy), "\\x01");
        assert_eq!(run(&a, &a, KeyCode::KeyA, Some("a"), press, ctrl, kitty), "\\e[97;5u");
        assert_eq!(run(&a, &a, KeyCode::KeyA, Some("a"), press, ctrl, everything), "\\e[97;5u");
        assert_eq!(run(&a, &a, KeyCode::KeyA, Some("a"), press, alt, everything), "\\e[97;3u");
        assert_eq!(run(&ch("P"), &ch("p"), KeyCode::KeyP, Some("P"), press, ctrl | shift, everything), "\\e[112:80;6u");
        assert_eq!(run(&ch("A"), &a, KeyCode::KeyA, Some("A"), press, shift, everything), "\\e[97:65;2;65u", "typing keeps its text");

        // Ctrl+Alt+A on a US keyboard: no character, no name, only the bare key.
        let nameless = Key::Unidentified(NativeKey::Windows(0x41));
        assert_eq!(run(&nameless, &a, KeyCode::KeyA, None, press, ctrl | alt, legacy), "\\e\\x01");
        assert_eq!(run(&nameless, &a, KeyCode::KeyA, None, press, ctrl | alt, kitty), "\\e[97;7u");
        assert_eq!(run(&nameless, &a, KeyCode::KeyA, None, press, ctrl | alt | shift, kitty), "\\e[97;8u");
        assert_eq!(run(&nameless, &a, KeyCode::KeyA, None, Action::Release, ctrl | alt, kitty | TermMode::REPORT_EVENT_TYPES), "\\e[97;7:3u");
        // A key with no name and no chord stays silent (AltGr with a key that has no third character).
        assert_eq!(run(&nameless, &a, KeyCode::KeyA, None, press, none, legacy), "");
        assert_eq!(run(&nameless, &a, KeyCode::KeyA, None, press, alt, kitty), "");

        // Ctrl+C on a Russian layout: the key types Cyrillic `с` and sits where `c` is.
        let es = ch("\u{441}");
        assert_eq!(run(&es, &es, KeyCode::KeyC, Some("\u{441}"), press, ctrl, legacy), "\\x03");
        assert_eq!(run(&es, &es, KeyCode::KeyC, Some("\u{441}"), press, none, legacy), "\\xd1\\x81");

        // A dead key: nothing the first time, `^^` the second, `^` with Space.
        let dead = Key::Dead(Some('^'));
        let caret = ch("^");
        assert_eq!(run(&dead, &caret, KeyCode::BracketLeft, None, press, none, legacy), "");
        assert_eq!(run(&dead, &caret, KeyCode::BracketLeft, Some("^^"), press, none, legacy), "^^");
        assert_eq!(run(&dead, &caret, KeyCode::BracketLeft, Some("^^"), press, none, kitty), "^^");
        assert_eq!(run(&dead, &caret, KeyCode::BracketLeft, Some("^^"), press, shift, kitty), "^^", "a dead key reached with Shift");
        assert_eq!(run(&dead, &caret, KeyCode::BracketLeft, Some("^^"), press, none, everything), "\\e[0;;94:94u");
        assert_eq!(run(&dead, &caret, KeyCode::BracketLeft, None, Action::Release, none, everything), "");
        let space = Key::Named(NamedKey::Space);
        assert_eq!(run(&space, &space, KeyCode::Space, Some("^"), press, none, legacy), "^");
        // One it cannot combine with: both characters arrive with the letter's key.
        assert_eq!(run(&ch("x"), &ch("x"), KeyCode::KeyX, Some("^x"), press, none, legacy), "^x");

        // A held Shift repeats on Windows; the program hears it go down once.
        let lshift = Key::Named(NamedKey::Shift);
        let held = |action| {
            let parts = Parts { logical: &lshift, bare: &lshift, location: KeyLocation::Left, physical: PhysicalKey::Code(KeyCode::ShiftLeft), text: None, action };
            crate::probe::show(&encode_parts(&parts, shift, everything).unwrap_or_default())
        };
        assert_eq!(held(press), "\\e[57441;2u");
        assert_eq!(held(Action::Repeat), "");
        // Other keys still repeat.
        assert_eq!(run(&a, &a, KeyCode::KeyA, Some("a"), Action::Repeat, none, everything), "\\e[97;1:2;97u");
    }

    #[test]
    fn paste_cannot_escape_bracket() {
        let v = encode_paste("a\x1b[201~rm -rf /", true);
        let s = String::from_utf8(v).unwrap();
        assert!(s.starts_with("\x1b[200~") && s.ends_with("\x1b[201~"));
        assert_eq!(s.matches("\x1b[201~").count(), 1);
        assert_eq!(encode_paste("a\r\nb\nc", false), b"a\rb\rc");
    }

    #[test]
    fn mouse_reports() {
        assert_eq!(encode_mouse(0, 4, 2, true, true).unwrap(), b"\x1b[<0;5;3M");
        assert_eq!(encode_mouse(0, 4, 2, false, true).unwrap(), b"\x1b[<0;5;3m");
        assert_eq!(encode_mouse(64, 0, 0, true, false).unwrap(), vec![0x1b, b'[', b'M', 96, 33, 33]);
        assert!(encode_mouse(0, 300, 0, true, false).is_none());
    }
}
