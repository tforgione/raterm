//! GDK's key events, as the view's keys.

use gtk::gdk;

use crate::native::keys::{Key, Mods, Named};

pub(super) fn mods(state: gdk::ModifierType) -> Mods {
    Mods {
        shift: state.contains(gdk::ModifierType::SHIFT_MASK),
        ctrl: state.contains(gdk::ModifierType::CONTROL_MASK),
        alt: state.contains(gdk::ModifierType::MOD1_MASK),
        logo: state.contains(gdk::ModifierType::SUPER_MASK),
    }
}

pub(super) fn key(ev: &gdk::EventKey) -> Key {
    use gdk::keys::constants as k;
    let keyval = ev.keyval();
    let named = match keyval {
        k::Up => Named::Up,
        k::Down => Named::Down,
        k::Right => Named::Right,
        k::Left => Named::Left,
        k::Home => Named::Home,
        k::End => Named::End,
        k::Insert => Named::Insert,
        k::Delete => Named::Delete,
        k::Page_Up => Named::PageUp,
        k::Page_Down => Named::PageDown,
        k::F1 => Named::F(1),
        k::F2 => Named::F(2),
        k::F3 => Named::F(3),
        k::F4 => Named::F(4),
        k::F5 => Named::F(5),
        k::F6 => Named::F(6),
        k::F7 => Named::F(7),
        k::F8 => Named::F(8),
        k::F9 => Named::F(9),
        k::F10 => Named::F(10),
        k::F11 => Named::F(11),
        k::F12 => Named::F(12),
        k::Return => Named::Return,
        k::KP_Enter => Named::KpEnter,
        k::BackSpace => Named::BackSpace,
        k::Tab => Named::Tab,
        k::ISO_Left_Tab => Named::BackTab,
        k::Escape => Named::Escape,
        _ => {
            return match keyval.to_unicode() {
                Some(c) if !c.is_control() => Key::Char(c),
                _ => Key::Other(
                    keyval
                        .name()
                        .map(|n| n.to_lowercase().replace('_', ""))
                        .unwrap_or_default(),
                ),
            };
        }
    };
    Key::Named(named)
}
