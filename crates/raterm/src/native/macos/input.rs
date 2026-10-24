//! tao's key events, as the view's keys.

use tao::event::KeyEvent;
use tao::keyboard::{Key as TaoKey, KeyLocation, ModifiersState};

use crate::native::keys::{Key, Mods, Named};

pub(super) fn mods(state: ModifiersState) -> Mods {
    Mods {
        shift: state.shift_key(),
        ctrl: state.control_key(),
        alt: state.alt_key(),
        logo: state.super_key(),
    }
}

/// The key of an event, `None` for a lone modifier and the keys raterm
/// does not know.
pub(super) fn key(event: &KeyEvent, mods: Mods) -> Option<Key> {
    let named = match &event.logical_key {
        TaoKey::Character(c) => {
            let mut chars = c.chars();
            return match (chars.next(), chars.next()) {
                (Some(c), None) if !c.is_control() => Some(Key::Char(c)),
                _ => None,
            };
        }
        TaoKey::Space => return Some(Key::Char(' ')),
        TaoKey::ArrowUp => Named::Up,
        TaoKey::ArrowDown => Named::Down,
        TaoKey::ArrowRight => Named::Right,
        TaoKey::ArrowLeft => Named::Left,
        TaoKey::Home => Named::Home,
        TaoKey::End => Named::End,
        TaoKey::Insert => Named::Insert,
        TaoKey::Delete => Named::Delete,
        TaoKey::PageUp => Named::PageUp,
        TaoKey::PageDown => Named::PageDown,
        TaoKey::F1 => Named::F(1),
        TaoKey::F2 => Named::F(2),
        TaoKey::F3 => Named::F(3),
        TaoKey::F4 => Named::F(4),
        TaoKey::F5 => Named::F(5),
        TaoKey::F6 => Named::F(6),
        TaoKey::F7 => Named::F(7),
        TaoKey::F8 => Named::F(8),
        TaoKey::F9 => Named::F(9),
        TaoKey::F10 => Named::F(10),
        TaoKey::F11 => Named::F(11),
        TaoKey::F12 => Named::F(12),
        TaoKey::Enter if event.location == KeyLocation::Numpad => Named::KpEnter,
        TaoKey::Enter => Named::Return,
        TaoKey::Backspace => Named::BackSpace,
        TaoKey::Tab if mods.shift => Named::BackTab,
        TaoKey::Tab => Named::Tab,
        TaoKey::Escape => Named::Escape,
        // Other keys only make shortcuts: F13, ContextMenu… lowercased.
        TaoKey::Unidentified(_) | TaoKey::Dead(_) => return None,
        other => return Some(Key::Other(format!("{other:?}").to_lowercase())),
    };
    Some(Key::Named(named))
}
