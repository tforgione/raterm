//! Drawing the grid with Pango, in the terminal's colors.

use std::sync::OnceLock;

use gtk::prelude::*;
use gtk::{cairo, pango};
use raterm::config;
use raterm::term::{Line, Run};

use super::{PAD_X, PAD_Y, View, Visual};

pub(super) struct Theme {
    pub(super) fg: u32,
    pub(super) bg: u32,
    pub(super) cursor: u32,
    pub(super) palette: [u32; 16],
}

/// The default theme of style.css, with the colors of `user.css` over it:
/// until the palette has a place in config.toml (Spec-Native-grid), the
/// native view reads the same variables as the page.
pub(super) fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let mut theme = Theme {
            fg: 0xd8dee9,
            bg: 0x151618,
            cursor: 0xd8dee9,
            palette: [
                0x2e3440, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xd8dee9,
                0x5c6370, 0xf08088, 0xb5e890, 0xf5d58e, 0x82c4ff, 0xda9cf0, 0x7fd4de, 0xffffff,
            ],
        };
        let css = std::fs::read_to_string(config::dir().join("user.css")).unwrap_or_default();
        for decl in css.split([';', '{', '}']) {
            let Some((name, value)) = decl.split_once(':') else {
                continue;
            };
            let Some(rgb) = value
                .trim()
                .strip_prefix('#')
                .and_then(|h| u32::from_str_radix(h, 16).ok())
            else {
                continue;
            };
            match name.trim().strip_prefix("--") {
                Some("fg") => theme.fg = rgb,
                Some("bg") => theme.bg = rgb,
                Some("cursor") => theme.cursor = rgb,
                Some(c) => {
                    if let Some(i) = c.strip_prefix('c').and_then(|i| i.parse::<usize>().ok())
                        && i < 16
                    {
                        theme.palette[i] = rgb;
                    }
                }
                None => {}
            }
        }
        theme
    })
}

/// Width and height of a cell of the font, in pixels.
pub(super) fn measure(area: &gtk::DrawingArea, font: &pango::FontDescription) -> (f64, f64) {
    let layout = area.create_pango_layout(Some(&"X".repeat(100)));
    layout.set_font_description(Some(font));
    let (w, h) = layout.size();
    let scale = pango::SCALE as f64;
    (w as f64 / scale / 100.0, (h as f64 / scale).ceil())
}

/// Cells a run takes.
pub(super) fn cells(run: &Run) -> usize {
    let wide = if run.style.class.contains("wide") {
        2
    } else {
        1
    };
    run.text.chars().count() * wide
}

impl View {
    pub(super) fn draw(&self, cr: &cairo::Context) {
        set_color(cr, theme().bg, 1.0);
        let _ = cr.paint();
        let style = self.area.style_context();
        self.session.read(|term| {
            for (i, visual) in self.visuals(term).iter().enumerate() {
                let y = PAD_Y + i as f64 * self.cell.1;
                if let Visual::Text { line, start, at } = *visual {
                    let selected = self.selection.and_then(|s| s.cells_of(term, at));
                    self.draw_line(cr, &style, line, start, y, selected);
                }
            }
        });
    }

    /// Draws cells `start..start + cols` of a line at height `y`, with the
    /// `selected` cells inverted.
    fn draw_line(
        &self,
        cr: &cairo::Context,
        style: &gtk::StyleContext,
        line: &Line,
        start: usize,
        y: f64,
        selected: Option<(usize, usize)>,
    ) {
        let cols = self.size.0 as usize;
        let is_selected = |x: usize| selected.is_some_and(|(s, e)| x >= s && x < e);
        let mut col = 0;
        for run in &line.runs {
            let n = cells(run);
            if col + n <= start {
                col += n;
                continue;
            }
            if col >= start + cols {
                break;
            }
            let wide = n / run.text.chars().count().max(1);
            // The chars of the run in this row, with their cell.
            let chars: Vec<(usize, char)> = run
                .text
                .chars()
                .enumerate()
                .map(|(i, c)| (col + i * wide, c))
                .filter(|&(x, _)| x >= start && x < start + cols)
                .collect();
            col += n;
            // Runs of chars all selected or not, drawn in their own colors.
            for part in chars.chunk_by(|a, b| is_selected(a.0) == is_selected(b.0)) {
                self.draw_part(cr, style, run, part, start, wide, y, is_selected(part[0].0));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_part(
        &self,
        cr: &cairo::Context,
        style: &gtk::StyleContext,
        run: &Run,
        chars: &[(usize, char)],
        start: usize,
        wide: usize,
        y: f64,
        selected: bool,
    ) {
        let (cw, ch) = self.cell;
        let class = &run.style.class;
        let mut fg = color(run.style.fg.as_deref()).unwrap_or(theme().fg);
        let mut bg = color(run.style.bg.as_deref());
        if class.contains("cursor") {
            (fg, bg) = (theme().bg, Some(theme().cursor));
        }
        // Selected text is shown inverted, as in the page.
        if selected {
            (fg, bg) = (theme().bg, Some(theme().fg));
        }
        let x = PAD_X + (chars[0].0 - start) as f64 * cw;
        if let Some(bg) = bg {
            set_color(cr, bg, 1.0);
            cr.rectangle(x, y, (chars.len() * wide) as f64 * cw, ch);
            let _ = cr.fill();
        }
        if class.split(' ').any(|c| c == "h") {
            return;
        }
        let attrs = pango::AttrList::new();
        attrs.insert(pango::AttrColor::new_foreground(
            ((fg >> 16) as u16) * 257,
            ((fg >> 8 & 0xff) as u16) * 257,
            ((fg & 0xff) as u16) * 257,
        ));
        for c in class.split(' ') {
            match c {
                "b" => attrs.insert(pango::AttrInt::new_weight(pango::Weight::Bold)),
                "i" => attrs.insert(pango::AttrInt::new_style(pango::Style::Italic)),
                "u" => attrs.insert(pango::AttrInt::new_underline(pango::Underline::Single)),
                "s" => attrs.insert(pango::AttrInt::new_strikethrough(true)),
                "d" => attrs.insert(pango::AttrInt::new_foreground_alpha(0x9999)),
                _ => {}
            }
        }
        let layout = self.area.create_pango_layout(None);
        layout.set_font_description(Some(&self.font));
        layout.set_attributes(Some(&attrs));
        if chars.iter().all(|&(_, c)| c.is_ascii()) {
            let text: String = chars.iter().map(|&(_, c)| c).collect();
            layout.set_text(&text);
            gtk::render_layout(style, cr, x, y, &layout);
        } else {
            // Glyphs of fallback fonts have other widths: each goes to its own
            // cell, so that the rest of the line stays on the grid.
            for &(cell, c) in chars {
                layout.set_text(c.encode_utf8(&mut [0; 4]));
                gtk::render_layout(style, cr, PAD_X + (cell - start) as f64 * cw, y, &layout);
            }
        }
    }
}

pub(super) fn set_color(cr: &cairo::Context, rgb: u32, alpha: f64) {
    let c = |shift: u32| ((rgb >> shift) & 0xff) as f64 / 255.0;
    cr.set_source_rgba(c(16), c(8), c(0), alpha);
}

/// A color of the core (`var(--c1)`, `#rrggbb`, `var(--fg)`), as RGB.
pub(super) fn color(spec: Option<&str>) -> Option<u32> {
    let spec = spec?;
    if let Some(hex) = spec.strip_prefix('#') {
        return u32::from_str_radix(hex, 16).ok();
    }
    match spec.strip_prefix("var(--")?.strip_suffix(')')? {
        "fg" => Some(theme().fg),
        "bg" => Some(theme().bg),
        c => theme()
            .palette
            .get(c.strip_prefix('c')?.parse::<usize>().ok()?)
            .copied(),
    }
}
