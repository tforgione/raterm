//! Keys, encoded as terminal sequences like `encode` in app.js.

use gtk::gdk;
use raterm::term::Modes;

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
