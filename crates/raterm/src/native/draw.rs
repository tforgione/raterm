//! Drawing the grid with Pango, in the terminal's colors.

use std::sync::OnceLock;

use gtk::prelude::*;
use gtk::{cairo, pango};
use raterm::config;
use raterm::term::{Line, Run, Terminal};

use super::{PAD_X, PAD_Y, View, Visual};

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
    match run.cluster {
        0 => run.text.chars().count() * wide(run),
        n => n as usize,
    }
}

/// Cells each char of a run takes.
fn wide(run: &Run) -> usize {
    if run.style.class.contains("wide") {
        2
    } else {
        1
    }
}

/// What each cell of a run holds, with the cell it starts at in the run: a
/// char, or the whole run when it is a unit (see `Run::cluster`).
pub(super) fn units(run: &Run) -> Vec<(usize, &str)> {
    if run.cluster > 0 {
        return vec![(0, run.text.as_str())];
    }
    let wide = wide(run);
    run.text
        .char_indices()
        .enumerate()
        .map(|(i, (at, c))| (i * wide, &run.text[at..at + c.len_utf8()]))
        .collect()
}

/// A regional indicator: two of them are a flag.
fn regional(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|c| ('\u{1f1e6}'..='\u{1f1ff}').contains(&c))
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
                    let (found, current) = match &self.search {
                        Some(search) => search.cells(line, at),
                        None => (Vec::new(), None),
                    };
                    let marks = Marks {
                        selected: self.selection.and_then(|s| s.cells_of(term, at)),
                        found,
                        current,
                    };
                    self.draw_line(cr, &style, line, start, y, &marks);
                }
            }
            self.draw_indicator(cr, term);
        });
    }

    /// A thin rounded bar at the right edge, where the view is in the flow:
    /// fully shown while scrolling, then fading. The flow counts a row per
    /// entry of the history, and blocks for the rows
    /// they were measured to take: exact for most lines, cheap with a long
    /// history, as blocks are few.
    fn draw_indicator(&self, cr: &cairo::Context, term: &Terminal) {
        let Some(at) = self.scrolled_at.get() else {
            return;
        };
        let age = at.elapsed();
        let alpha = if age < INDICATOR_SOLID {
            0.5
        } else {
            0.5 * (1.0
                - (age - INDICATOR_SOLID).as_secs_f64()
                    / (INDICATOR_SHOWN - INDICATOR_SOLID).as_secs_f64())
        };
        let rows = self.size.1 as f64;
        let used = term
            .screen()
            .iter()
            .rposition(|l| !l.runs.is_empty())
            .map_or(1, |y| y + 1);
        let first = term.history.first();
        let blocks: usize = self
            .heights
            .borrow()
            .keys()
            .filter(|&&n| n >= first)
            .map(|&n| self.block_rows(n) - 1)
            .sum();
        let total = (term.history.next() - first) as f64 + (used + blocks) as f64;
        if alpha <= 0.0 || term.alt_screen() || total <= rows {
            return;
        }
        let (w, h) = (
            self.area.allocated_width() as f64,
            self.area.allocated_height() as f64,
        );
        let track = h - 4.0;
        let height = (rows / total * track).max(24.0);
        let top = (total - rows - self.scroll as f64).max(0.0) / (total - rows) * (track - height);
        let (x, y, width, r) = (w - 5.0, 2.0 + top, 3.0, 1.5);
        set_color(cr, theme().fg, alpha);
        cr.new_sub_path();
        cr.arc(
            x + r,
            y + r,
            r,
            std::f64::consts::PI,
            1.5 * std::f64::consts::PI,
        );
        cr.arc(x + width - r, y + r, r, 1.5 * std::f64::consts::PI, 0.0);
        cr.arc(
            x + width - r,
            y + height - r,
            r,
            0.0,
            0.5 * std::f64::consts::PI,
        );
        cr.arc(
            x + r,
            y + height - r,
            r,
            0.5 * std::f64::consts::PI,
            std::f64::consts::PI,
        );
        cr.close_path();
        let _ = cr.fill();
    }

    /// Draws cells `start..start + cols` of a line at height `y`, with the
    /// cells of `marks` in their colors.
    fn draw_line(
        &self,
        cr: &cairo::Context,
        style: &gtk::StyleContext,
        line: &Line,
        start: usize,
        y: f64,
        marks: &Marks,
    ) {
        let cols = self.size.0 as usize;
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
            let wide = wide(run);
            // The cells of the run in this row, with what each holds.
            let chars: Vec<(usize, &str)> = units(run)
                .into_iter()
                .map(|(x, text)| (col + x, text))
                .filter(|&(x, _)| x >= start && x < start + cols)
                .collect();
            col += n;
            // Runs of chars with the same mark, drawn in its colors.
            for part in chars.chunk_by(|a, b| marks.at(a.0) == marks.at(b.0)) {
                self.draw_part(cr, style, run, part, start, wide, y, marks.at(part[0].0));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_part(
        &self,
        cr: &cairo::Context,
        style: &gtk::StyleContext,
        run: &Run,
        chars: &[(usize, &str)],
        start: usize,
        wide: usize,
        y: f64,
        mark: Mark,
    ) {
        let (cw, ch) = self.cell;
        let class = &run.style.class;
        let mut fg = color(run.style.fg.as_deref()).unwrap_or(theme().fg);
        let mut bg = color(run.style.bg.as_deref());
        if class.contains("cursor") {
            (fg, bg) = (theme().bg, Some(theme().cursor));
        }
        match mark {
            Mark::None => {}
            Mark::Found => (fg, bg) = (theme().bg, Some(theme().palette[3])),
            Mark::Current => (fg, bg) = (theme().bg, Some(theme().palette[9])),
            Mark::Selected => (fg, bg) = (theme().selection_text, Some(theme().selection)),
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
            return;
        }
        // Glyphs of fallback fonts have other widths: each cell is drawn at its
        // place, so that the rest of the line stays on the grid. Two regional
        // indicators make a flag: drawn as one glyph from the first cell.
        let mut i = 0;
        while i < chars.len() {
            let (cell, first) = chars[i];
            let mut text = first.to_owned();
            i += 1;
            if regional(&text) && i < chars.len() && regional(chars[i].1) {
                text += chars[i].1;
                i += 1;
            }
            layout.set_text(&text);
            gtk::render_layout(style, cr, PAD_X + (cell - start) as f64 * cw, y, &layout);
        }
    }
}

/// How long the scroll indicator stays, then how long until it starts fading.
pub(super) const INDICATOR_SHOWN: std::time::Duration = std::time::Duration::from_millis(1000);
const INDICATOR_SOLID: std::time::Duration = std::time::Duration::from_millis(600);

/// The cells of a line drawn in other colors than their own.
struct Marks {
    selected: Option<(usize, usize)>,
    found: Vec<(usize, usize)>,
    current: Option<(usize, usize)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    None,
    Found,
    Current,
    Selected,
}

impl Marks {
    fn at(&self, x: usize) -> Mark {
        let has = |r: &(usize, usize)| x >= r.0 && x < r.1;
        if self.selected.as_ref().is_some_and(has) {
            Mark::Selected
        } else if self.current.as_ref().is_some_and(has) {
            Mark::Current
        } else if self.found.iter().any(has) {
            Mark::Found
        } else {
            Mark::None
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
