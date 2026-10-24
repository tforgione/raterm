//! The terminal's colors, from the `[colors]` of the configuration.

use std::sync::OnceLock;

use raterm::config;
use raterm::term::Tint;

pub(super) struct Theme {
    pub(super) fg: u32,
    pub(super) bg: u32,
    pub(super) cursor: u32,
    pub(super) selection: u32,
    pub(super) selection_text: u32,
    pub(super) palette: [u32; 16],
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Sets the colors, once, before the view draws anything.
pub(super) fn init_theme(colors: &config::Colors) {
    let _ = THEME.set(load(colors));
}

pub(super) fn theme() -> &'static Theme {
    THEME.get_or_init(|| load(&config::Colors::default()))
}

/// The default theme, under the `[colors]` of config.toml.
fn load(colors: &config::Colors) -> Theme {
    let mut theme = Theme {
        fg: 0xd8dee9,
        bg: 0x151618,
        cursor: 0xd8dee9,
        selection: 0xd8dee9,
        selection_text: 0x151618,
        palette: [
            0x2e3440, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xd8dee9,
            0x5c6370, 0xf08088, 0xb5e890, 0xf5d58e, 0x82c4ff, 0xda9cf0, 0x7fd4de, 0xffffff,
        ],
    };
    let rgb = |c: Option<config::Rgb>| c.map(|c| c.0);
    theme.fg = rgb(colors.foreground).unwrap_or(theme.fg);
    theme.bg = rgb(colors.background).unwrap_or(theme.bg);
    theme.cursor = rgb(colors.cursor).unwrap_or(theme.cursor);
    for (i, c) in colors
        .normal
        .iter()
        .chain(&colors.bright)
        .take(16)
        .enumerate()
    {
        theme.palette[i] = c.0;
    }
    // Selected text is shown inverted, unless set.
    theme.selection = rgb(colors.selection).unwrap_or(theme.fg);
    theme.selection_text = rgb(colors.selection_text).unwrap_or(theme.bg);
    theme
}

/// A color of the core, as RGB in the theme.
pub(super) fn rgb(tint: Tint) -> u32 {
    match tint {
        Tint::Fg => theme().fg,
        Tint::Bg => theme().bg,
        Tint::Palette(i) => theme().palette[i as usize % 16],
        Tint::Rgb(v) => v,
    }
}
