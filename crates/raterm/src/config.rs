//! User configuration, read from `$XDG_CONFIG_HOME/raterm/config.toml`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Program started in the terminal; defaults to `$SHELL`.
    pub shell: Option<String>,
    /// Lines and blocks kept in the scrollback; unbounded by default.
    pub scrollback: Option<usize>,
    /// Bold text in one of the 8 normal colors uses its bright variant.
    pub bold_is_bright: bool,
    /// The window title follows the title set by programs (OSC 0/2).
    pub dynamic_title: bool,
    pub font: Font,
    pub colors: Colors,
    pub cursor: Cursor,
    pub window: Window,
    pub background: Background,
    pub images: Images,
    pub markdown: Markdown,
    pub blocks: Blocks,
    /// Shortcuts, as `"ctrl+shift+c" = "copy"` or `"f13" = { send = "..." }`,
    /// merged over the defaults; `"none"` removes a default.
    pub keys: BTreeMap<String, Binding>,
}

/// The palette, as in alacritty; each color unset keeps the default theme's.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub selection: Option<Rgb>,
    pub selection_text: Option<Rgb>,
    /// Colors 0 to 7, then 8 to 15.
    pub normal: Vec<Rgb>,
    pub bright: Vec<Rgb>,
}

/// A color written `#rrggbb`: nothing else can reach the CSS of blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb(pub u32);

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        text.strip_prefix('#')
            .filter(|hex| hex.len() == 6)
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .map(Rgb)
            .ok_or_else(|| serde::de::Error::custom(format!("not a #rrggbb color: {text:?}")))
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Font {
    pub family: String,
    /// In points, like other terminals.
    pub size: f32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cursor {
    pub style: CursorStyle,
    pub blink: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyle {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Window {
    pub decorations: bool,
    /// Opacity of the default background, from 0 to 1.
    pub opacity: f64,
}

impl Window {
    pub fn opacity(&self) -> f64 {
        self.opacity.clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Images {
    /// Maximum height of image and SVG blocks, in lines; unlimited if unset.
    pub max_height: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Markdown {
    /// Width of the centered column of text, in columns; 0 for the whole
    /// terminal width. Tables always get the whole width.
    pub max_width: u16,
}

impl Default for Markdown {
    fn default() -> Self {
        Markdown { max_width: 120 }
    }
}

/// An image under the grid, darkened by the background color.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Background {
    /// Absolute, `~/...`, or relative to the configuration's directory.
    pub image: Option<String>,
    pub fit: Fit,
    /// Opacity of the background color over the image.
    pub tint: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fit {
    /// Fills the window, cropped.
    Cover,
    /// Whole, centered on the background color.
    Contain,
    Tile,
}

impl Default for Background {
    fn default() -> Self {
        Background {
            image: None,
            fit: Fit::Cover,
            tint: 0.7,
        }
    }
}

impl Background {
    pub fn image_path(&self) -> Option<PathBuf> {
        let image = self.image.as_deref()?;
        Some(match image.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
            None => dir().join(image),
        })
    }

    pub fn tint(&self) -> f64 {
        self.tint.clamp(0.0, 1.0)
    }
}

/// Text of Markdown and HTML blocks; their code uses the terminal font.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Blocks {
    pub font_family: String,
    /// In points, like the terminal font.
    pub font_size: f32,
    /// The terminal's background when unset.
    pub background: Option<Rgb>,
    /// Columns on each side of the text of Markdown blocks, which are then
    /// only as wide as it; the whole width when unset.
    pub padding: Option<u16>,
}

impl Default for Blocks {
    fn default() -> Self {
        Blocks {
            font_family: "sans-serif".into(),
            font_size: 12.0,
            background: None,
            padding: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
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
            scrollback: None,
            bold_is_bright: false,
            dynamic_title: true,
            font: Font::default(),
            colors: Colors::default(),
            cursor: Cursor::default(),
            window: Window::default(),
            background: Background::default(),
            images: Images::default(),
            markdown: Markdown::default(),
            blocks: Blocks::default(),
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
        Window {
            decorations: true,
            opacity: 1.0,
        }
    }
}

impl Config {
    /// Loads the configuration, falling back to the defaults (with a warning)
    /// when the file is missing or invalid.
    pub fn load() -> Config {
        let path = dir().join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).unwrap_or_else(|e| {
                eprintln!("raterm: {}: {e}", path.display());
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

/// `$XDG_CONFIG_HOME/raterm`, or `~/.config/raterm`.
fn dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_default()
        .join("raterm")
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
        assert_eq!(config.scrollback, None);
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
        assert_eq!(config.window.opacity(), 1.0);
    }

    #[test]
    fn blocks_background_defaults_to_the_terminal() {
        let blocks = |text| Config::parse(text).unwrap().blocks;
        assert_eq!(blocks("").background, None);
        assert_eq!(blocks("").padding, None);
        assert_eq!(blocks("[blocks]\npadding = 2").padding, Some(2));
        let set = blocks("[blocks]\nbackground = \"#2a2d33\"");
        assert_eq!(set.background, Some(Rgb(0x2a2d33)));
        assert!(Config::parse("[blocks]\nbackground = \"red\"").is_err());
    }

    #[test]
    fn background_image_paths() {
        let background = |text| Config::parse(text).unwrap().background;
        assert_eq!(background("").image_path(), None);
        let set = background("[background]\nimage = \"/w/a.png\"\nfit = \"tile\"\ntint = 3");
        assert_eq!(set.image_path(), Some(PathBuf::from("/w/a.png")));
        assert_eq!((set.fit, set.tint()), (Fit::Tile, 1.0));
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let set = background("[background]\nimage = \"~/w/a.png\"");
        assert_eq!(set.image_path(), Some(home.join("w/a.png")));
        assert_eq!((set.fit, set.tint()), (Fit::Cover, 0.7));
        let set = background("[background]\nimage = \"a.png\"");
        assert_eq!(set.image_path(), Some(dir().join("a.png")));
    }

    #[test]
    fn opacity_stays_between_0_and_1() {
        let window = |text| Config::parse(text).unwrap().window;
        assert_eq!(window("[window]\nopacity = 0.85").opacity(), 0.85);
        assert_eq!(window("[window]\nopacity = 2.0").opacity(), 1.0);
        assert_eq!(window("[window]\ndecorations = false").opacity(), 1.0);
    }

    #[test]
    fn colors_are_rrggbb() {
        let config = Config::parse(
            "[colors]\nforeground = \"#eaeaea\"\nnormal = [\"#000000\", \"#cd0000\"]\n",
        )
        .unwrap();
        assert_eq!(config.colors.foreground, Some(Rgb(0xeaeaea)));
        assert_eq!(config.colors.normal, [Rgb(0), Rgb(0xcd0000)]);
        assert!(Config::parse("[colors]\nbackground = \"red\"\n").is_err());
        assert!(Config::parse("[colors]\nbackground = \"#fff;}\"\n").is_err());
    }

    #[test]
    fn typos_are_reported() {
        assert!(Config::parse("scrolback = 10").is_err());
    }
}
