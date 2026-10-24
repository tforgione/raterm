//! Keys: shortcuts, and the sequences other keys send. Backends translate
//! their toolkit's key events to `Key` and `Mods`.

use std::collections::BTreeMap;

use raterm::config::{Binding, Config};
use raterm::term::Modes;

/// The modifiers held with a key, a click or the wheel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Mods {
    pub(super) shift: bool,
    pub(super) ctrl: bool,
    pub(super) alt: bool,
    /// Super on Linux, Cmd on macOS.
    pub(super) logo: bool,
}

/// A key pressed.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Key {
    /// A key that types a char (not a control char), as typed: Shift+C is
    /// `C`.
    Char(char),
    Named(Named),
    /// Any other key, by its name lowercased without underscores, for
    /// shortcuts only.
    Other(String),
}

/// The keys that send a sequence of their own.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Named {
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    /// F1 to F12.
    F(u8),
    Return,
    KpEnter,
    BackSpace,
    Tab,
    /// Shift+Tab.
    BackTab,
    Escape,
}

impl Named {
    /// The name of the key in shortcuts, as the browser's key names
    /// lowercased: Page_Up is "pageup".
    fn name(self) -> String {
        match self {
            Named::Up => "up".into(),
            Named::Down => "down".into(),
            Named::Right => "right".into(),
            Named::Left => "left".into(),
            Named::Home => "home".into(),
            Named::End => "end".into(),
            Named::Insert => "insert".into(),
            Named::Delete => "delete".into(),
            Named::PageUp => "pageup".into(),
            Named::PageDown => "pagedown".into(),
            Named::F(n) => format!("f{n}"),
            Named::Return => "return".into(),
            Named::KpEnter => "kpenter".into(),
            Named::BackSpace => "backspace".into(),
            Named::Tab => "tab".into(),
            Named::BackTab => "isolefttab".into(),
            Named::Escape => "escape".into(),
        }
    }
}

/// Modifiers in the order of shortcut names; `cmd` is read as `super`.
const MODIFIERS: [&str; 4] = ["ctrl", "alt", "shift", "super"];

/// The default shortcuts.
#[cfg(not(target_os = "macos"))]
const DEFAULT_KEYS: &[(&str, &str)] = &[
    ("ctrl+shift+c", "copy"),
    ("ctrl+shift+v", "paste"),
    ("shift+insert", "paste_selection"),
    ("ctrl+shift+f", "search"),
    ("ctrl+=", "zoom_in"),
    ("ctrl++", "zoom_in"),
    ("ctrl+-", "zoom_out"),
    ("ctrl+0", "zoom_reset"),
    ("shift+pageup", "scroll_page_up"),
    ("shift+pagedown", "scroll_page_down"),
];

/// The default shortcuts on macOS: Cmd, as its applications (see
/// Spec-macOS); Ctrl goes to the program.
#[cfg(target_os = "macos")]
const DEFAULT_KEYS: &[(&str, &str)] = &[
    ("cmd+c", "copy"),
    ("cmd+v", "paste"),
    ("cmd+f", "search"),
    ("cmd+=", "zoom_in"),
    ("cmd++", "zoom_in"),
    ("cmd+-", "zoom_out"),
    ("cmd+0", "zoom_reset"),
    ("cmd+n", "new_window"),
    ("shift+pageup", "scroll_page_up"),
    ("shift+pagedown", "scroll_page_down"),
];

/// The shortcuts: the defaults, with those of the configuration over them.
pub(super) fn bindings(config: &Config) -> BTreeMap<String, Binding> {
    DEFAULT_KEYS
        .iter()
        .map(|&(k, a)| (k.to_owned(), Binding::Action(a.into())))
        .chain(config.keys.clone())
        .map(|(k, b)| (normalize(&k), b))
        .collect()
}

/// Canonical form of a shortcut such as "Shift+Ctrl+C": "ctrl+shift+c".
fn normalize(spec: &str) -> String {
    let spec = spec.to_lowercase();
    let mut parts: Vec<&str> = spec
        .split('+')
        .map(|p| if p == "cmd" { "super" } else { p })
        .collect();
    let mut key = parts.pop().unwrap_or("").to_owned();
    if key.is_empty() {
        parts.pop();
        key = "+".into();
    }
    let mut out: Vec<String> = MODIFIERS
        .iter()
        .filter(|m| parts.contains(m))
        .map(|m| m.to_string())
        .collect();
    out.push(key);
    out.join("+")
}

/// The shortcut a key is, in the form of `normalize`.
pub(super) fn name(key: &Key, mods: Mods) -> String {
    let key = match key {
        Key::Char(' ') => "space".to_owned(),
        Key::Char(c) => c.to_lowercase().to_string(),
        Key::Named(named) => named.name(),
        Key::Other(name) => name.clone(),
    };
    let mods = [mods.ctrl, mods.alt, mods.shift, mods.logo];
    let mut out: Vec<&str> = MODIFIERS
        .iter()
        .zip(mods)
        .filter(|&(_, on)| on)
        .map(|(m, _)| *m)
        .collect();
    out.push(&key);
    normalize(&out.join("+"))
}

/// xterm's encoding of a mouse event: `button` is
/// 0-2 for the buttons, 64-65 for the wheel, plus 32 for motion; `sgr` is
/// mode 1006, else the legacy encoding.
pub(super) fn mouse(
    button: u8,
    mods: Mods,
    (x, y): (usize, usize),
    release: bool,
    sgr: bool,
) -> Option<Vec<u8>> {
    let code = button + 4 * mods.shift as u8 + 8 * mods.alt as u8 + 16 * mods.ctrl as u8;
    if sgr {
        let end = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{};{}{end}", x + 1, y + 1).into_bytes());
    }
    // The legacy encoding has no release per button, and stops at 223.
    if x >= 223 || y >= 223 {
        return None;
    }
    let b = if release { (code & !3) | 3 } else { code };
    Some(vec![0x1b, b'[', b'M', 32 + b, 33 + x as u8, 33 + y as u8])
}

/// The sequence a key sends; `None` for text,
/// which goes through the input method.
pub(super) fn encode(key: &Key, mods: Mods, modes: &Modes) -> Option<String> {
    let Mods {
        shift, alt, ctrl, ..
    } = mods;
    let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let csi = |fin: &str| {
        if m > 1 {
            format!("\x1b[1;{m}{fin}")
        } else if modes.app_cursor {
            format!("\x1bO{fin}")
        } else {
            format!("\x1b[{fin}")
        }
    };
    let tilde = |n: u8| {
        if m > 1 {
            format!("\x1b[{n};{m}~")
        } else {
            format!("\x1b[{n}~")
        }
    };
    let ss3 = |fin: &str| {
        if m > 1 {
            format!("\x1b[1;{m}{fin}")
        } else {
            format!("\x1bO{fin}")
        }
    };
    let esc = if alt { "\x1b" } else { "" };
    let named = match key {
        Key::Named(named) => *named,
        Key::Char(c) => {
            let c = *c;
            return Some(if ctrl {
                let code = match c {
                    ' ' => 0,
                    '/' => 0x1f,
                    c if ('@'..='_').contains(&c.to_ascii_uppercase()) => {
                        c.to_ascii_uppercase() as u8 - 0x40
                    }
                    _ => return None,
                };
                format!("{esc}{}", code as char)
            } else if alt {
                format!("\x1b{c}")
            } else {
                return None;
            });
        }
        Key::Other(_) => return None,
    };
    Some(match named {
        Named::Up => csi("A"),
        Named::Down => csi("B"),
        Named::Right => csi("C"),
        Named::Left => csi("D"),
        Named::Home => csi("H"),
        Named::End => csi("F"),
        Named::Insert => tilde(2),
        Named::Delete => tilde(3),
        Named::PageUp => tilde(5),
        Named::PageDown => tilde(6),
        Named::F(1) => ss3("P"),
        Named::F(2) => ss3("Q"),
        Named::F(3) => ss3("R"),
        Named::F(4) => ss3("S"),
        Named::F(5) => tilde(15),
        Named::F(6) => tilde(17),
        Named::F(7) => tilde(18),
        Named::F(8) => tilde(19),
        Named::F(9) => tilde(20),
        Named::F(10) => tilde(21),
        Named::F(11) => tilde(23),
        Named::F(12) => tilde(24),
        Named::F(_) => return None,
        Named::Return | Named::KpEnter => format!("{esc}\r"),
        Named::BackSpace => format!("{esc}{}", if ctrl { "\x08" } else { "\x7f" }),
        Named::Tab => format!("{esc}\t"),
        Named::BackTab => "\x1b[Z".into(),
        Named::Escape => "\x1b".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_events_in_both_encodings() {
        let none = Mods::default();
        assert_eq!(
            mouse(0, none, (4, 9), false, true).unwrap(),
            b"\x1b[<0;5;10M"
        );
        assert_eq!(
            mouse(2, none, (4, 9), true, true).unwrap(),
            b"\x1b[<2;5;10m"
        );
        let ctrl = Mods { ctrl: true, ..none };
        assert_eq!(
            mouse(64, ctrl, (0, 0), false, true).unwrap(),
            b"\x1b[<80;1;1M"
        );
        assert_eq!(mouse(0, none, (4, 9), false, false).unwrap(), b"\x1b[M %*");
        assert_eq!(mouse(1, none, (4, 9), true, false).unwrap(), b"\x1b[M#%*");
        assert_eq!(mouse(0, none, (300, 0), false, false), None);
    }

    #[test]
    fn shortcut_names_are_canonical() {
        assert_eq!(normalize("Shift+Ctrl+C"), "ctrl+shift+c");
        assert_eq!(normalize("ctrl++"), "ctrl++");
        assert_eq!(normalize("shift+PageUp"), "shift+pageup");
        assert_eq!(normalize("Cmd+C"), "super+c");
    }

    #[test]
    fn keys_are_named_as_shortcuts() {
        let ctrl_shift = Mods {
            ctrl: true,
            shift: true,
            ..Mods::default()
        };
        assert_eq!(name(&Key::Char('C'), ctrl_shift), "ctrl+shift+c");
        assert_eq!(name(&Key::Char(' '), Mods::default()), "space");
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        assert_eq!(name(&Key::Named(Named::PageUp), shift), "shift+pageup");
    }

    #[test]
    fn keys_send_xterm_sequences() {
        let modes = Modes::default();
        let none = Mods::default();
        let ctrl = Mods { ctrl: true, ..none };
        let alt = Mods { alt: true, ..none };
        assert_eq!(
            encode(&Key::Named(Named::Up), none, &modes).unwrap(),
            "\x1b[A"
        );
        assert_eq!(
            encode(&Key::Named(Named::Up), ctrl, &modes).unwrap(),
            "\x1b[1;5A"
        );
        assert_eq!(
            encode(&Key::Named(Named::F(5)), none, &modes).unwrap(),
            "\x1b[15~"
        );
        assert_eq!(encode(&Key::Char('c'), ctrl, &modes).unwrap(), "\x03");
        assert_eq!(encode(&Key::Char('x'), alt, &modes).unwrap(), "\x1bx");
        assert_eq!(encode(&Key::Char('x'), none, &modes), None);
    }
}
