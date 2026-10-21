//! Keys: shortcuts, and the sequences other keys send, as in app.js.

use std::collections::BTreeMap;

use gtk::gdk;
use raterm::config::{Binding, Config};
use raterm::term::Modes;

const MODIFIERS: [&str; 4] = ["ctrl", "alt", "shift", "super"];

/// The default shortcuts, as `DEFAULT_KEYS` in app.js.
const DEFAULT_KEYS: [(&str, &str); 10] = [
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
    let mut parts: Vec<&str> = spec.split('+').collect();
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

/// The shortcut a key event is, in the form of `normalize`.
pub(super) fn name(ev: &gdk::EventKey) -> String {
    let state = ev.state();
    let mods = [
        state.contains(gdk::ModifierType::CONTROL_MASK),
        state.contains(gdk::ModifierType::MOD1_MASK),
        state.contains(gdk::ModifierType::SHIFT_MASK),
        state.contains(gdk::ModifierType::SUPER_MASK),
    ];
    let key = match ev.keyval().to_unicode() {
        Some(' ') => "space".to_owned(),
        Some(c) if !c.is_control() => c.to_lowercase().to_string(),
        // Names as the page's key names: Page_Up is "pageup".
        _ => ev
            .keyval()
            .name()
            .map(|n| n.to_lowercase().replace('_', ""))
            .unwrap_or_default(),
    };
    let mut out: Vec<&str> = MODIFIERS
        .iter()
        .zip(mods)
        .filter(|&(_, on)| on)
        .map(|(m, _)| *m)
        .collect();
    out.push(&key);
    normalize(&out.join("+"))
}

/// xterm's encoding of a mouse event, as `sendMouse` in app.js: `button` is
/// 0-2 for the buttons, 64-65 for the wheel, plus 32 for motion; `sgr` is
/// mode 1006, else the legacy encoding.
pub(super) fn mouse(
    button: u8,
    state: gdk::ModifierType,
    (x, y): (usize, usize),
    release: bool,
    sgr: bool,
) -> Option<Vec<u8>> {
    let code = button
        + 4 * state.contains(gdk::ModifierType::SHIFT_MASK) as u8
        + 8 * state.contains(gdk::ModifierType::MOD1_MASK) as u8
        + 16 * state.contains(gdk::ModifierType::CONTROL_MASK) as u8;
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

/// The sequence a key sends, the same as `encode` in app.js; `None` for text,
/// which goes through the input method.
pub(super) fn encode(ev: &gdk::EventKey, modes: &Modes) -> Option<String> {
    use gdk::keys::constants as k;
    let state = ev.state();
    let (shift, alt, ctrl) = (
        state.contains(gdk::ModifierType::SHIFT_MASK),
        state.contains(gdk::ModifierType::MOD1_MASK),
        state.contains(gdk::ModifierType::CONTROL_MASK),
    );
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
    let key = ev.keyval();
    Some(match key {
        k::Up => csi("A"),
        k::Down => csi("B"),
        k::Right => csi("C"),
        k::Left => csi("D"),
        k::Home => csi("H"),
        k::End => csi("F"),
        k::Insert => tilde(2),
        k::Delete => tilde(3),
        k::Page_Up => tilde(5),
        k::Page_Down => tilde(6),
        k::F1 => ss3("P"),
        k::F2 => ss3("Q"),
        k::F3 => ss3("R"),
        k::F4 => ss3("S"),
        k::F5 => tilde(15),
        k::F6 => tilde(17),
        k::F7 => tilde(18),
        k::F8 => tilde(19),
        k::F9 => tilde(20),
        k::F10 => tilde(21),
        k::F11 => tilde(23),
        k::F12 => tilde(24),
        k::Return | k::KP_Enter => format!("{esc}\r"),
        k::BackSpace => format!("{esc}{}", if ctrl { "\x08" } else { "\x7f" }),
        k::Tab => format!("{esc}\t"),
        k::ISO_Left_Tab => "\x1b[Z".into(),
        k::Escape => "\x1b".into(),
        _ => {
            let c = key.to_unicode()?;
            if ctrl {
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
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_events_in_both_encodings() {
        let none = gdk::ModifierType::empty();
        assert_eq!(
            mouse(0, none, (4, 9), false, true).unwrap(),
            b"\x1b[<0;5;10M"
        );
        assert_eq!(
            mouse(2, none, (4, 9), true, true).unwrap(),
            b"\x1b[<2;5;10m"
        );
        let ctrl = gdk::ModifierType::CONTROL_MASK;
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
    }
}
