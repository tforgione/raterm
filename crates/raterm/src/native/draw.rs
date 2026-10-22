//! Drawing the grid with Pango, in the terminal's colors.

use std::sync::OnceLock;

use gtk::prelude::*;
use gtk::{cairo, pango};
use raterm::config;
use raterm::term::{Attrs, CursorShape, Line, Run, Terminal, Tint};

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

/// The baseline of the configured font from the top of a cell, in pixels.
pub(super) fn baseline(area: &gtk::DrawingArea, font: &pango::FontDescription) -> f64 {
    let layout = area.create_pango_layout(Some("X"));
    layout.set_font_description(Some(font));
    layout.baseline() as f64 / pango::SCALE as f64
}

/// The chars the configured font has glyphs for.
pub(super) fn coverage(
    area: &gtk::DrawingArea,
    font: &pango::FontDescription,
) -> Option<pango::Coverage> {
    let font = area.pango_context().load_font(font)?;
    Some(font.coverage(&pango::Language::default()))
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
    if run.style.attrs.has(Attrs::WIDE) {
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
            let visuals = self.visuals(term);
            self.drawn.set(visuals.len());
            for (i, visual) in visuals.iter().enumerate() {
                let y = PAD_Y + i as f64 * self.cell.1;
                if let Visual::Text { line, start, at } = *visual {
                    let marks = self.marks(term, line, at);
                    self.draw_line(cr, &style, i, line, start, y, &marks);
                }
            }
            self.draw_indicator(cr, term);
        });
    }

    fn marks(&self, term: &Terminal, line: &Line, at: u64) -> Marks {
        let (found, current) = match &self.search {
            Some(search) => search.cells(line, at),
            None => (Vec::new(), None),
        };
        let has_cursor = line.runs.iter().any(|r| r.style.attrs.has(Attrs::CURSOR));
        Marks {
            selected: self.selection.and_then(|s| s.cells_of(term, at)),
            found,
            current,
            cursor: has_cursor.then(|| self.cursor_look(term)),
        }
    }

    /// How the cursor looks now: hollow without the focus, as in other
    /// terminals, and away half of the time when it blinks.
    fn cursor_look(&self, term: &Terminal) -> Look {
        let (shape, blinks) = term.cursor();
        if shape == CursorShape::Hidden {
            Look::Hidden
        } else if !self.focused.get() {
            Look::Hollow
        } else if blinks && !self.blink_on.get() {
            Look::Hidden
        } else {
            match shape {
                CursorShape::Block => Look::Block,
                CursorShape::Beam => Look::Bar,
                CursorShape::Underline => Look::Underline,
                CursorShape::HollowBlock => Look::Hollow,
                CursorShape::Hidden => Look::Hidden,
            }
        }
    }

    /// The rows of the view that would not be drawn as they were, or `None`
    /// when the whole view has to be (blocks, scroll indicator, another
    /// number of rows).
    pub(super) fn changed_rows(&self, term: &Terminal) -> Option<Vec<usize>> {
        let visuals = self.visuals(term);
        if visuals.len() != self.drawn.get() || self.scrolled_at.get().is_some() {
            return None;
        }
        let rows = self.rows.borrow();
        let width = self.area.allocated_width();
        let mut changed = Vec::new();
        for (i, visual) in visuals.iter().enumerate() {
            let Visual::Text { line, start, at } = *visual else {
                return None;
            };
            let marks = self.marks(term, line, at);
            if !rows
                .get(i)
                .and_then(Option::as_ref)
                .is_some_and(|r| r.is(line, start, &marks, self.cell.0, width))
            {
                changed.push(i);
            }
        }
        Some(changed)
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

    /// Draws cells `start..start + cols` of a line at height `y`, row `row`
    /// of the view, with the cells of `marks` in their colors. The image of
    /// the row is kept, and drawn again as it is while the row stays the
    /// same: drawing a screen of code took 2.3 ms even with its text shaped.
    #[allow(clippy::too_many_arguments)]
    fn draw_line(
        &self,
        cr: &cairo::Context,
        style: &gtk::StyleContext,
        row: usize,
        line: &Line,
        start: usize,
        y: f64,
        marks: &Marks,
    ) {
        let (cw, ch) = self.cell;
        let width = self.area.allocated_width();
        let mut rows = self.rows.borrow_mut();
        if rows.len() <= row {
            rows.resize_with(row + 1, || None);
        }
        let kept = rows[row]
            .as_ref()
            .is_some_and(|r| r.is(line, start, marks, cw, width));
        // Glyphs of fallback fonts can be taller than a cell: the image has
        // a transparent margin of a cell above and below, where they spill
        // over the next rows as when drawn directly.
        if !kept {
            let height = (3.0 * ch).ceil() as i32;
            let Ok(image) = cr
                .target()
                .create_similar(cairo::Content::ColorAlpha, width, height)
            else {
                return;
            };
            let Ok(rc) = cairo::Context::new(&image) else {
                return;
            };
            self.render_line(&rc, style, line, start, ch, marks);
            rows[row] = Some(Row {
                line: line.clone(),
                start,
                marks: marks.clone(),
                cell: cw,
                width,
                image,
            });
        }
        let image = &rows[row].as_ref().unwrap().image;
        let _ = cr.set_source_surface(image, 0.0, y - ch);
        cr.rectangle(0.0, y - ch, width as f64, 3.0 * ch);
        let _ = cr.fill();
    }

    /// Draws cells `start..start + cols` of a line at height `y` of `cr`.
    fn render_line(
        &self,
        cr: &cairo::Context,
        style: &gtk::StyleContext,
        line: &Line,
        start: usize,
        y: f64,
        marks: &Marks,
    ) {
        let (cw, ch) = self.cell;
        let cols = self.size.0 as usize;
        // Parts of the row: cells of the same run with the same mark.
        let mut parts = Vec::new();
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
            let chars: Vec<(usize, &str)> = units(run)
                .into_iter()
                .map(|(x, text)| (col + x, text))
                .filter(|&(x, _)| x >= start && x < start + cols)
                .collect();
            col += n;
            for part in chars.chunk_by(|a, b| marks.at(a.0) == marks.at(b.0)) {
                parts.push((run, part.to_vec(), marks.at(part[0].0)));
            }
        }
        let Some(first) = parts.first().map(|(_, chars, _)| chars[0].0) else {
            return;
        };
        // A row of the font's own glyphs, a cell each, is shaped as one text,
        // its styles as attributes: shaping each part on its own took 10 ms
        // for a screen of code, and each char on its own 26 ms for one of
        // French. Glyphs of fallback fonts are drawn cell by cell.
        let own = |c: char| {
            c.is_ascii()
                || self
                    .coverage
                    .as_ref()
                    .is_some_and(|cov| cov.get(c as i32) == pango::CoverageLevel::Exact)
        };
        let whole = parts.iter().all(|(run, chars, _)| {
            run.cluster == 0 && wide(run) == 1 && chars.iter().all(|&(_, c)| c.chars().all(own))
        });
        let layout = self.area.create_pango_layout(None);
        layout.set_font_description(Some(&self.font));
        let attrs = pango::AttrList::new();
        let mut text = String::new();
        for (run, chars, mark) in &parts {
            let (fg, bg) = colors(run, *mark, marks.cursor);
            let x = PAD_X + (chars[0].0 - start) as f64 * cw;
            if let Some(bg) = bg {
                set_color(cr, bg, 1.0);
                cr.rectangle(x, y, (chars.len() * wide(run)) as f64 * cw, ch);
                let _ = cr.fill();
            }
            let hidden = run.style.attrs.has(Attrs::HIDDEN);
            if whole {
                let from = text.len() as u32;
                for &(_, c) in chars {
                    text.push_str(if hidden { " " } else { c });
                }
                text_attrs(&attrs, fg, run.style.attrs, from..text.len() as u32);
                continue;
            }
            // Blanks have nothing more to draw: full-screen programs paint
            // with them.
            let lined = run.style.attrs.has(Attrs::UNDERLINE | Attrs::STRIKE);
            if hidden || !lined && chars.iter().all(|&(_, c)| c == " ") {
                continue;
            }
            let attrs = pango::AttrList::new();
            text_attrs(&attrs, fg, run.style.attrs, 0..u32::MAX);
            layout.set_attributes(Some(&attrs));
            // Glyphs of fallback fonts have other widths: each cell is drawn
            // at its place, so that the rest of the line stays on the grid,
            // and on the baseline of the configured font, as their fonts
            // have other heights. Two regional indicators make a flag: drawn
            // as one glyph from the first cell.
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
                let shift = self.baseline - layout.baseline() as f64 / pango::SCALE as f64;
                let x = PAD_X + (cell - start) as f64 * cw;
                gtk::render_layout(style, cr, x, y + shift, &layout);
            }
        }
        if whole {
            layout.set_text(&text);
            layout.set_attributes(Some(&attrs));
            gtk::render_layout(style, cr, PAD_X + (first - start) as f64 * cw, y, &layout);
        }
        // A block cursor is drawn by its colors; the others over the text.
        let cursor = parts
            .iter()
            .find(|(run, ..)| run.style.attrs.has(Attrs::CURSOR));
        if let (Some((run, chars, _)), Some(look)) = (cursor, marks.cursor) {
            let x = PAD_X + (chars[0].0 - start) as f64 * cw;
            let w = (chars.len() * wide(run)) as f64 * cw;
            set_color(cr, theme().cursor, 1.0);
            match look {
                Look::Bar => cr.rectangle(x, y, 2.0, ch),
                Look::Underline => cr.rectangle(x, y + ch - 2.0, w, 2.0),
                Look::Hollow => {
                    cr.set_line_width(1.0);
                    cr.rectangle(x + 0.5, y + 0.5, w - 1.0, ch - 1.0);
                    let _ = cr.stroke();
                }
                Look::Block | Look::Hidden => {}
            }
            let _ = cr.fill();
        }
    }
}

/// The image of a row of the view, and what it was drawn from.
pub(super) struct Row {
    line: Line,
    start: usize,
    marks: Marks,
    /// The width of a cell (the zoom changes the font), and of the view.
    cell: f64,
    width: i32,
    image: cairo::Surface,
}

impl Row {
    fn is(&self, line: &Line, start: usize, marks: &Marks, cell: f64, width: i32) -> bool {
        self.start == start
            && self.cell == cell
            && self.width == width
            && self.marks == *marks
            && self.line == *line
    }
}

/// The text and background colors of a run, with a mark and the look of the
/// cursor.
fn colors(run: &Run, mark: Mark, cursor: Option<Look>) -> (u32, Option<u32>) {
    let mut fg = run.style.fg.map_or(theme().fg, rgb);
    let mut bg = run.style.bg.map(rgb);
    if run.style.attrs.has(Attrs::CURSOR) && cursor == Some(Look::Block) {
        (fg, bg) = (theme().bg, Some(theme().cursor));
    }
    match mark {
        Mark::None => {}
        Mark::Found => (fg, bg) = (theme().bg, Some(theme().palette[3])),
        Mark::Current => (fg, bg) = (theme().bg, Some(theme().palette[9])),
        Mark::Selected => (fg, bg) = (theme().selection_text, Some(theme().selection)),
    }
    (fg, bg)
}

/// Adds the attributes of a run's style to bytes `range` of a layout's text.
fn text_attrs(list: &pango::AttrList, fg: u32, attrs: Attrs, range: std::ops::Range<u32>) {
    let add = |mut attr: pango::Attribute| {
        attr.set_start_index(range.start);
        attr.set_end_index(range.end);
        list.insert(attr);
    };
    add(pango::AttrColor::new_foreground(
        ((fg >> 16) as u16) * 257,
        ((fg >> 8 & 0xff) as u16) * 257,
        ((fg & 0xff) as u16) * 257,
    )
    .into());
    if attrs.has(Attrs::BOLD) {
        add(pango::AttrInt::new_weight(pango::Weight::Bold).into());
    }
    if attrs.has(Attrs::ITALIC) {
        add(pango::AttrInt::new_style(pango::Style::Italic).into());
    }
    if attrs.has(Attrs::UNDERLINE) {
        add(pango::AttrInt::new_underline(pango::Underline::Single).into());
    }
    if attrs.has(Attrs::STRIKE) {
        add(pango::AttrInt::new_strikethrough(true).into());
    }
    if attrs.has(Attrs::DIM) {
        add(pango::AttrInt::new_foreground_alpha(0x9999).into());
    }
}

/// How long the scroll indicator stays, then how long until it starts fading.
pub(super) const INDICATOR_SHOWN: std::time::Duration = std::time::Duration::from_millis(1000);
const INDICATOR_SOLID: std::time::Duration = std::time::Duration::from_millis(600);

/// The cells of a line drawn in other colors than their own.
#[derive(Clone, PartialEq)]
struct Marks {
    selected: Option<(usize, usize)>,
    found: Vec<(usize, usize)>,
    current: Option<(usize, usize)>,
    /// How the cursor looks, on its line only.
    cursor: Option<Look>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Look {
    Block,
    Bar,
    Underline,
    Hollow,
    Hidden,
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

/// A color of the core, as RGB in the theme.
fn rgb(tint: Tint) -> u32 {
    match tint {
        Tint::Fg => theme().fg,
        Tint::Bg => theme().bg,
        Tint::Palette(i) => theme().palette[i as usize % 16],
        Tint::Rgb(v) => v,
    }
}
