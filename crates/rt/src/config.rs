//! User configuration, read from `$XDG_CONFIG_HOME/rt/config.toml`.
//!
//! Colors are not configured here: they are CSS variables, overridden in
//! `user.css` next to this file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Program started in the terminal; defaults to `$SHELL`.
    pub shell: Option<String>,
    /// Lines and blocks kept in the scrollback.
    pub scrollback: usize,
    /// Bold text in one of the 8 normal colors uses its bright variant.
    pub bold_is_bright: bool,
    /// The window title follows the title set by programs (OSC 0/2).
    pub dynamic_title: bool,
    pub font: Font,
    pub cursor: Cursor,
    pub window: Window,
    pub images: Images,
    pub videos: Videos,
    /// Shortcuts, as `"ctrl+shift+c" = "copy"` or `"f13" = { send = "..." }`,
    /// merged over the defaults; `"none"` removes a default.
    pub keys: BTreeMap<String, Binding>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Font {
    pub family: String,
    /// In points, like other terminals.
    pub size: f32,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cursor {
    pub style: CursorStyle,
    pub blink: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Window {
    pub decorations: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Images {
    /// Maximum height of image, SVG and video blocks, in lines; unlimited if
    /// unset.
    pub max_height: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Videos {
    /// Show videos sent whole in the stream (from stdin or over ssh), not
    /// only local files sent by path; off as they can be heavy.
    pub embedded: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Binding {
    /// copy, paste, paste_selection, search, zoom_in, zoom_out, zoom_reset,
    /// scroll_page_up, scroll_page_down, new_window or none.
    Action(String),
    /// Text sent to the program as if typed.
    Send { send: String },
}

impl Default for Config {
    fn default() -> Self {
        Config {
            shell: None,
            scrollback: 10_000,
            bold_is_bright: false,
            dynamic_title: true,
            font: Font::default(),
            cursor: Cursor::default(),
            window: Window::default(),
            images: Images::default(),
            videos: Videos::default(),
            keys: BTreeMap::new(),
        }
    }
}

impl Default for Font {
    fn default() -> Self {
        Font {
            family: "monospace".into(),
            size: 10.5,
        }
    }
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor {
            style: CursorStyle::Block,
            blink: false,
        }
    }
}

impl Default for Window {
    fn default() -> Self {
        Window { decorations: true }
    }
}

impl Config {
    /// Loads the configuration, falling back to the defaults (with a warning)
    /// when the file is missing or invalid.
    pub fn load() -> Config {
        let path = dir().join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).unwrap_or_else(|e| {
                eprintln!("rt: {}: {e}", path.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn parse(text: &str) -> Result<Config, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn shell(&self) -> String {
        self.shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".into())
    }
}

/// `$XDG_CONFIG_HOME/rt`, or `~/.config/rt`.
pub fn dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_default()
        .join("rt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_defaults() {
        let config = Config::parse(
            r#"
            bold_is_bright = true
            [font]
            family = "Ubuntu Mono"
            [keys]
            "ctrl+shift+e" = "new_window"
            "shift+pageup" = { send = "\u001b[5;2~" }
            "#,
        )
        .unwrap();
        assert!(config.bold_is_bright);
        assert_eq!(config.font.family, "Ubuntu Mono");
        assert_eq!(config.font.size, Font::default().size);
        assert_eq!(config.scrollback, 10_000);
        assert_eq!(
            config.keys["ctrl+shift+e"],
            Binding::Action("new_window".into())
        );
        assert_eq!(
            config.keys["shift+pageup"],
            Binding::Send {
                send: "\x1b[5;2~".into()
            }
        );
    }

    #[test]
    fn typos_are_reported() {
        assert!(Config::parse("scrolback = 10").is_err());
    }
}
