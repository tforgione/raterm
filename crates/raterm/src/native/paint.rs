//! Drawing the grid, whatever paints it: which cells take which colors, the
//! text of a row and how it is shaped, box drawing, the cursor, images and
//! the scroll indicator. `plan_row` decides, a backend's `Canvas` paints.

use std::time::Duration;

use raterm::graphics::{Image, Images};
use raterm::term::{Attrs, CursorShape, Line, Run, Terminal};

use super::boxes::{self, Shape};
use super::flow::{Flow, PAD_X};
use super::runs::{cells, regional, units, wide};
use super::theme::{rgb, theme};

/// What a backend paints with (see Spec-macOS): Cairo and Pango on Linux,
/// Core Graphics and Core Text on macOS. Coordinates are in pixels from the
/// top left of what is painted; colors are RGB.
pub(super) trait Canvas {
    /// Fills a rectangle.
    fn fill(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: u32, alpha: f64);
    /// Strokes the outline of a rectangle, `width` wide.
    fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: u32, width: f64);
    /// Fills a rectangle with rounded ends, of a radius of half its width
    /// (the scroll indicator).
    fn fill_rounded(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: u32, alpha: f64);
    /// Shapes `text` as one, in the configured font, and draws it with the
    /// top of its line at `y`; `spans` give the styles of its byte ranges.
    fn text(&mut self, x: f64, y: f64, text: &str, spans: &[Span]);
    /// Draws a glyph alone (a fallback font's, or a flag), its cell at `x`,
    /// `y`, on the baseline of the configured font.
    fn glyph(&mut self, x: f64, y: f64, text: &str, style: Style);
    /// Paints a box drawing char.
    fn shape(&mut self, shape: &Shape, rgb: u32);
    /// Paints part of an image of the graphics protocols (see `Slice`).
    fn image(&mut self, slice: &Slice);
}

/// How a text is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Style {
    pub(super) fg: u32,
    /// Bold, italic, underline, strikeout and dim; the others are not drawn
    /// by the text itself.
    pub(super) attrs: Attrs,
}

/// The style of bytes `start..end` of a text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Span {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) style: Style,
}

/// Part of an image of the graphics protocols, in the cells it covers.
pub(super) struct Slice<'a> {
    pub(super) id: u32,
    pub(super) image: &'a Image,
    /// Where it goes, clipped to these cells.
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) w: f64,
    pub(super) h: f64,
    /// The scale from the image's pixels to the view's.
    pub(super) scale: (f64, f64),
    /// The image's pixel at `x`, `y`.
    pub(super) from: (f64, f64),
}

/// A step of drawing a row, in order: what comes later paints over.
pub(super) enum Op<'a> {
    Fill {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        rgb: u32,
    },
    StrokeRect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        rgb: u32,
    },
    Text {
        x: f64,
        y: f64,
        text: String,
        spans: Vec<Span>,
    },
    Glyph {
        x: f64,
        y: f64,
        text: String,
        style: Style,
    },
    Shape {
        shape: Shape,
        rgb: u32,
    },
    Image(Slice<'a>),
}

/// Paints the steps of a row.
pub(super) fn paint(canvas: &mut impl Canvas, ops: &[Op]) {
    for op in ops {
        match op {
            &Op::Fill { x, y, w, h, rgb } => canvas.fill(x, y, w, h, rgb, 1.0),
            &Op::StrokeRect { x, y, w, h, rgb } => canvas.stroke_rect(x, y, w, h, rgb, 1.0),
            Op::Text { x, y, text, spans } => canvas.text(*x, *y, text, spans),
            Op::Glyph { x, y, text, style } => canvas.glyph(*x, *y, text, *style),
            Op::Shape { shape, rgb } => canvas.shape(shape, *rgb),
            Op::Image(slice) => canvas.image(slice),
        }
    }
}

/// The cells of a line drawn in other colors than their own.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Marks {
    selected: Option<(usize, usize)>,
    /// The selection is a block, which covers the blanks past the end of the
    /// line too.
    block: bool,
    found: Vec<(usize, usize)>,
    current: Option<(usize, usize)>,
    /// How the cursor looks, on its line only.
    cursor: Option<Look>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
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

/// How long the scroll indicator stays, then how long until it starts fading.
pub(super) const INDICATOR_SHOWN: Duration = Duration::from_millis(1000);
const INDICATOR_SOLID: Duration = Duration::from_millis(600);

impl Flow {
    /// The cells of a line in the colors of the selection and the search,
    /// and the cursor.
    pub(super) fn marks(&self, term: &Terminal, line: &Line, start: usize, at: u64) -> Marks {
        let (found, current) = match &self.search {
            Some(search) => search.cells(line, at),
            None => (Vec::new(), None),
        };
        let has_cursor = line.runs.iter().any(|r| r.style.attrs.has(Attrs::CURSOR));
        Marks {
            selected: self.selection.and_then(|s| s.cells_of(term, at, start)),
            block: self.selection.is_some_and(|s| s.block.is_some()),
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

    /// Draws cells `start..start + cols` of a line at height `y`, with the
    /// cells of `marks` in their colors; `own` tells the chars the
    /// configured font has glyphs for. Images under the text come first,
    /// those over it last.
    pub(super) fn plan_row<'a>(
        &self,
        line: &Line,
        start: usize,
        y: f64,
        marks: &Marks,
        images: &'a Images,
        own: impl Fn(char) -> bool,
    ) -> Vec<Op<'a>> {
        let mut ops = self.slices(line, start, y, images, true);
        self.plan_text(&mut ops, line, start, y, marks, own);
        ops.extend(self.slices(line, start, y, images, false));
        ops
    }

    fn plan_text(
        &self,
        ops: &mut Vec<Op>,
        line: &Line,
        start: usize,
        y: f64,
        marks: &Marks,
        own: impl Fn(char) -> bool,
    ) {
        let (cw, ch) = self.cell;
        let cols = self.size.0 as usize;
        // The line ends at its last char: the rest of a block is drawn here.
        if let Some((s, e)) = marks.selected.filter(|_| marks.block) {
            let end: usize = line.runs.iter().map(cells).sum();
            let (s, e) = (s.max(end), e.min(start + cols));
            if s < e {
                let x = PAD_X + (s - start) as f64 * cw;
                let (w, rgb) = ((e - s) as f64 * cw, theme().selection);
                ops.push(Op::Fill {
                    x,
                    y,
                    w,
                    h: ch,
                    rgb,
                });
            }
        }
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
        // its styles as spans: shaping each part on its own took 10 ms for a
        // screen of code, and each char on its own 26 ms for one of French.
        // Glyphs of fallback fonts are drawn cell by cell.
        let whole = parts.iter().all(|(run, chars, _)| {
            run.cluster == 0
                && wide(run) == 1
                && chars
                    .iter()
                    .all(|&(_, c)| c.chars().all(|c| c.is_ascii() || own(c)))
        });
        let mut text = String::new();
        let mut spans = Vec::new();
        // Box drawing chars, drawn by raterm after the text.
        let mut drawn = Vec::new();
        for (run, chars, mark) in &parts {
            let (fg, bg) = colors(run, *mark, marks.cursor);
            let x = PAD_X + (chars[0].0 - start) as f64 * cw;
            if let Some(rgb) = bg {
                let w = (chars.len() * wide(run)) as f64 * cw;
                ops.push(Op::Fill {
                    x,
                    y,
                    w,
                    h: ch,
                    rgb,
                });
            }
            let hidden = run.style.attrs.has(Attrs::HIDDEN);
            let style = Style {
                fg,
                attrs: run.style.attrs,
            };
            if whole {
                let from = text.len();
                for &(cell, c) in chars {
                    if !hidden && boxes::drawn(c) {
                        drawn.push((cell, c, fg));
                    }
                    text.push_str(if hidden || boxes::drawn(c) { " " } else { c });
                }
                let end = text.len();
                spans.push(Span {
                    start: from,
                    end,
                    style,
                });
                continue;
            }
            // Blanks have nothing more to draw: full-screen programs paint
            // with them.
            let lined = run.style.attrs.has(Attrs::UNDERLINE | Attrs::STRIKE);
            if hidden || !lined && chars.iter().all(|&(_, c)| c == " ") {
                continue;
            }
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
                if boxes::drawn(first) {
                    drawn.push((cell, first, fg));
                    continue;
                }
                if regional(&text) && i < chars.len() && regional(chars[i].1) {
                    text += chars[i].1;
                    i += 1;
                }
                let x = PAD_X + (cell - start) as f64 * cw;
                ops.push(Op::Glyph { x, y, text, style });
            }
        }
        if whole {
            let x = PAD_X + (first - start) as f64 * cw;
            ops.push(Op::Text { x, y, text, spans });
        }
        for (cell, c, rgb) in drawn {
            let x = PAD_X + (cell - start) as f64 * cw;
            if let Some(shape) = boxes::shape(c, x, y, cw, ch) {
                ops.push(Op::Shape { shape, rgb });
            }
        }
        // A block cursor is drawn by its colors; the others over the text.
        let cursor = parts
            .iter()
            .find(|(run, ..)| run.style.attrs.has(Attrs::CURSOR));
        if let (Some((run, chars, _)), Some(look)) = (cursor, marks.cursor) {
            let x = PAD_X + (chars[0].0 - start) as f64 * cw;
            let w = (chars.len() * wide(run)) as f64 * cw;
            let rgb = theme().cursor;
            ops.push(match look {
                Look::Bar => Op::Fill {
                    x,
                    y,
                    w: 2.0,
                    h: ch,
                    rgb,
                },
                Look::Underline => Op::Fill {
                    x,
                    y: y + ch - 2.0,
                    w,
                    h: 2.0,
                    rgb,
                },
                Look::Hollow => Op::StrokeRect {
                    x: x + 0.5,
                    y: y + 0.5,
                    w: w - 1.0,
                    h: ch - 1.0,
                    rgb,
                },
                Look::Block | Look::Hidden => return,
            });
        }
    }

    /// The slices of images cells of a line show, those under the text or
    /// over it, at height `y`: an image keeps its size in cells, whatever the
    /// zoom.
    fn slices<'a>(
        &self,
        line: &Line,
        start: usize,
        y: f64,
        images: &'a Images,
        under: bool,
    ) -> Vec<Op<'a>> {
        let (cw, ch) = self.cell;
        let end = start + self.size.0 as usize;
        let mut ops = Vec::new();
        for shown in &line.images {
            let slice = shown.slice;
            let (from, to) = (shown.at.max(start), (shown.at + shown.len).min(end));
            if from < to
                && let Some(image) = images.get(slice.image)
                && image.under == under
            {
                let (iw, ih) = image.cell;
                // Where the cell `from` starts in the image's pixels.
                let col = (slice.col as usize + from - shown.at) as f64;
                ops.push(Op::Image(Slice {
                    id: slice.image,
                    image,
                    x: PAD_X + (from - start) as f64 * cw,
                    y,
                    w: (to - from) as f64 * cw,
                    h: ch,
                    scale: (cw / iw, ch / ih),
                    from: (
                        col * iw - image.offset.0,
                        slice.row as f64 * ih - image.offset.1,
                    ),
                }));
            }
        }
        ops
    }

    /// The scroll indicator, a thin rounded bar at the right edge of a view
    /// `width × height` showing where it is in the flow: fully shown while
    /// scrolling, then fading. The flow counts a row per entry of the
    /// history, and blocks for the rows they were measured to take: exact
    /// for most lines, cheap with a long history, as blocks are few.
    /// `(x, y, width, height, alpha)`.
    pub(super) fn indicator(
        &self,
        term: &Terminal,
        width: f64,
        height: f64,
    ) -> Option<(f64, f64, f64, f64, f64)> {
        let age = self.scrolled_at.get()?.elapsed();
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
            return None;
        }
        let track = height - 4.0;
        let bar = (rows / total * track).max(24.0);
        let top = (total - rows - self.scroll as f64).max(0.0) / (total - rows) * (track - bar);
        Some((width - 5.0, 2.0 + top, 3.0, bar, alpha))
    }
}

impl Flow {
    /// Draws the whole view, `width × height`, every row anew: the
    /// background, the rows of text and the scroll indicator (blocks are
    /// web views over it). The Linux view keeps the image of each row
    /// instead (`gtk::draw`), which macOS can do too if drawing it all
    /// proves slow.
    #[cfg(target_os = "macos")]
    pub(super) fn draw_frame(
        &self,
        canvas: &mut impl Canvas,
        width: f64,
        height: f64,
        own: impl Fn(char) -> bool,
    ) {
        let opacity = self.config.window.opacity();
        canvas.fill(0.0, 0.0, width, height, theme().bg, opacity);
        self.session.read(|term| {
            for (i, visual) in self.visuals(term).iter().enumerate() {
                let y = super::flow::PAD_Y + i as f64 * self.cell.1;
                if let super::flow::Visual::Text { line, start, at } = *visual {
                    let marks = self.marks(term, line, start, at);
                    paint(
                        canvas,
                        &self.plan_row(line, start, y, &marks, &term.images, &own),
                    );
                }
            }
            if let Some((x, y, w, h, alpha)) = self.indicator(term, width, height) {
                canvas.fill_rounded(x, y, w, h, theme().fg, alpha);
            }
        });
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
