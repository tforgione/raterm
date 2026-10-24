//! Drawing the grid with Cairo and Pango: the `Canvas` of Linux, the
//! background, and the images of the rows kept from frame to frame.

use std::collections::HashMap;

use gtk::prelude::*;
use gtk::{cairo, pango};
use raterm::config;
use raterm::graphics::{Image, Images, Pixels};
use raterm::term::{Attrs, Line, Terminal};

use super::View;
use crate::native::boxes::{Segment, Shape};
use crate::native::flow::{PAD_Y, Visual};
use crate::native::paint::{self, Canvas, Marks, Slice, Span, Style};
use crate::native::theme::theme;

pub(super) use crate::native::paint::INDICATOR_SHOWN;

/// `[background] image`, if set and readable.
pub(super) fn load_image(background: &config::Background) -> Option<gtk::gdk_pixbuf::Pixbuf> {
    let path = background.image_path()?;
    gtk::gdk_pixbuf::Pixbuf::from_file(&path)
        .inspect_err(|e| eprintln!("raterm: background image {}: {e}", path.display()))
        .ok()
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

impl View {
    pub(super) fn draw(&self, cr: &cairo::Context) {
        // Replaces what was there: painted over, a translucent background
        // would build up.
        let opacity = self.flow.config.window.opacity();
        cr.set_operator(cairo::Operator::Source);
        match self.backdrop() {
            Some(backdrop) => {
                let _ = cr.set_source_surface(&backdrop, 0.0, 0.0);
                let _ = cr.paint_with_alpha(opacity);
            }
            None => {
                set_color(cr, theme().bg, opacity);
                let _ = cr.paint();
            }
        }
        cr.set_operator(cairo::Operator::Over);
        self.session.read(|term| {
            (self.images.borrow_mut()).retain(|&id, _| term.images.get(id).is_some());
            // Rows kept may show images since removed, which their cells
            // still mark.
            if self.removed.replace(term.images.removed) != term.images.removed {
                self.rows.borrow_mut().clear();
            }
            let visuals = self.flow.visuals(term);
            self.drawn.set(visuals.len());
            for (i, visual) in visuals.iter().enumerate() {
                let y = PAD_Y + i as f64 * self.flow.cell.1;
                if let Visual::Text { line, start, at } = *visual {
                    let marks = self.flow.marks(term, line, start, at);
                    self.draw_line(cr, i, line, start, y, &marks, &term.images);
                }
            }
            let (w, h) = (
                self.area.allocated_width() as f64,
                self.area.allocated_height() as f64,
            );
            if let Some((x, y, w, h, alpha)) = self.flow.indicator(term, w, h) {
                self.canvas(cr).fill_rounded(x, y, w, h, theme().fg, alpha);
            }
        });
    }

    /// The background image fitted to the view, under the background color
    /// at `tint`: composed again only when the size of the view changes.
    fn backdrop(&self) -> Option<cairo::ImageSurface> {
        let image = self.image.as_ref()?;
        let (w, h) = (self.area.allocated_width(), self.area.allocated_height());
        let mut kept = self.backdrop.borrow_mut();
        if let Some(backdrop) = kept.as_ref()
            && (backdrop.width(), backdrop.height()) == (w, h)
        {
            return Some(backdrop.clone());
        }
        let backdrop = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
        let cr = cairo::Context::new(&backdrop).ok()?;
        set_color(&cr, theme().bg, 1.0);
        let _ = cr.paint();
        let (iw, ih) = (image.width() as f64, image.height() as f64);
        let background = &self.flow.config.background;
        match background.fit {
            config::Fit::Tile => {
                cr.set_source_pixbuf(image, 0.0, 0.0);
                cr.source().set_extend(cairo::Extend::Repeat);
            }
            fit => {
                let (sx, sy) = (w as f64 / iw, h as f64 / ih);
                let scale = if fit == config::Fit::Cover {
                    sx.max(sy)
                } else {
                    sx.min(sy)
                };
                cr.translate((w as f64 - iw * scale) / 2.0, (h as f64 - ih * scale) / 2.0);
                cr.scale(scale, scale);
                cr.set_source_pixbuf(image, 0.0, 0.0);
            }
        }
        let _ = cr.paint();
        cr.identity_matrix();
        set_color(&cr, theme().bg, background.tint());
        let _ = cr.paint();
        drop(cr);
        *kept = Some(backdrop.clone());
        Some(backdrop)
    }

    /// The rows of the view that would not be drawn as they were, or `None`
    /// when the whole view has to be (blocks, scroll indicator, another
    /// number of rows).
    pub(super) fn changed_rows(&self, term: &Terminal) -> Option<Vec<usize>> {
        let visuals = self.flow.visuals(term);
        if visuals.len() != self.drawn.get()
            || self.flow.scrolled_at.get().is_some()
            || self.removed.get() != term.images.removed
        {
            return None;
        }
        let rows = self.rows.borrow();
        let width = self.area.allocated_width();
        let mut changed = Vec::new();
        for (i, visual) in visuals.iter().enumerate() {
            let Visual::Text { line, start, at } = *visual else {
                return None;
            };
            let marks = self.flow.marks(term, line, start, at);
            if !rows
                .get(i)
                .and_then(Option::as_ref)
                .is_some_and(|r| r.is(line, start, &marks, self.flow.cell.0, width))
            {
                changed.push(i);
            }
        }
        Some(changed)
    }

    /// Draws cells `start..start + cols` of a line at height `y`, row `row`
    /// of the view, with the cells of `marks` in their colors. The image of
    /// the row is kept, and drawn again as it is while the row stays the
    /// same: drawing a screen of code took 2.3 ms even with its text shaped.
    #[allow(clippy::too_many_arguments)]
    fn draw_line(
        &self,
        cr: &cairo::Context,
        row: usize,
        line: &Line,
        start: usize,
        y: f64,
        marks: &Marks,
        images: &Images,
    ) {
        let (cw, ch) = self.flow.cell;
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
        // over the next rows as when drawn directly. ASCII stays in its
        // cell: without margins, its images take a third of the memory.
        if !kept {
            let ascii = line.runs.iter().all(|r| r.text.is_ascii());
            let margin = if ascii { 0.0 } else { ch };
            let height = (ch + 2.0 * margin).ceil() as i32;
            let Ok(image) = cr
                .target()
                .create_similar(cairo::Content::ColorAlpha, width, height)
            else {
                return;
            };
            let Ok(rc) = cairo::Context::new(&image) else {
                return;
            };
            let own = |c: char| {
                self.coverage
                    .as_ref()
                    .is_some_and(|cov| cov.get(c as i32) == pango::CoverageLevel::Exact)
            };
            let ops = self.flow.plan_row(line, start, margin, marks, images, own);
            paint::paint(&mut self.canvas(&rc), &ops);
            raterm::trace!("rendered {row}");
            // Output that came after the rows to draw were picked changed
            // this one too: kept as drawn but clipped, it would stay stale.
            if !cr.in_clip(1.0, y + ch / 2.0).unwrap_or(true) {
                self.queue_row(row);
            }
            rows[row] = Some(Row {
                line: line.clone(),
                start,
                marks: marks.clone(),
                cell: cw,
                width,
                margin,
                image,
            });
        }
        let Row { image, margin, .. } = rows[row].as_ref().unwrap();
        let _ = cr.set_source_surface(image, 0.0, y - margin);
        cr.rectangle(0.0, y - margin, width as f64, ch + 2.0 * margin);
        let _ = cr.fill();
    }

    fn canvas<'a>(&'a self, cr: &'a cairo::Context) -> CairoCanvas<'a> {
        let layout = self.area.create_pango_layout(None);
        layout.set_font_description(Some(&self.font));
        CairoCanvas {
            cr,
            view: self,
            layout,
            style: self.area.style_context(),
        }
    }

    /// The surface of an image, made when it first shows; PNG is decoded
    /// by gdk-pixbuf's PNG loader only, so that no other loader reads what a
    /// program sent.
    fn surface(&self, id: u32, image: &Image) -> Option<cairo::ImageSurface> {
        let mut images = self.images.borrow_mut();
        if let Some(surface) = images.get(&id) {
            return surface.clone();
        }
        let data = &image.data;
        let (w, h) = (data.width as i32, data.height as i32);
        let made = (|| {
            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).ok()?;
            match &data.pixels {
                Pixels::Argb(pixels) => {
                    let stride = surface.stride() as usize;
                    let mut bytes = surface.data().ok()?;
                    for (y, row) in pixels.chunks(data.width).enumerate() {
                        let row = row.iter().flat_map(|p| p.to_ne_bytes());
                        for (dst, b) in bytes[y * stride..].iter_mut().zip(row) {
                            *dst = b;
                        }
                    }
                }
                Pixels::Png(png) => {
                    let loader = gtk::gdk_pixbuf::PixbufLoader::with_type("png").ok()?;
                    let decoded = loader.write(png).and_then(|_| loader.close());
                    let pixbuf = decoded.ok().and_then(|_| loader.pixbuf())?;
                    let cr = cairo::Context::new(&surface).ok()?;
                    cr.set_source_pixbuf(&pixbuf, 0.0, 0.0);
                    cr.paint().ok()?;
                }
            }
            Some(surface)
        })();
        if made.is_none() {
            eprintln!("raterm: image {id} could not be decoded");
        }
        images.insert(id, made.clone());
        made
    }
}

/// Painting with Cairo, text with Pango in the configured font.
struct CairoCanvas<'a> {
    cr: &'a cairo::Context,
    view: &'a View,
    layout: pango::Layout,
    style: gtk::StyleContext,
}

impl Canvas for CairoCanvas<'_> {
    fn fill(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: u32, alpha: f64) {
        set_color(self.cr, rgb, alpha);
        self.cr.rectangle(x, y, w, h);
        let _ = self.cr.fill();
    }

    fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: u32, width: f64) {
        set_color(self.cr, rgb, 1.0);
        self.cr.set_line_width(width);
        self.cr.rectangle(x, y, w, h);
        let _ = self.cr.stroke();
    }

    fn fill_rounded(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: u32, alpha: f64) {
        use std::f64::consts::PI;
        let (cr, r) = (self.cr, w / 2.0);
        set_color(cr, rgb, alpha);
        cr.new_sub_path();
        cr.arc(x + r, y + r, r, PI, 1.5 * PI);
        cr.arc(x + w - r, y + r, r, 1.5 * PI, 0.0);
        cr.arc(x + w - r, y + h - r, r, 0.0, 0.5 * PI);
        cr.arc(x + r, y + h - r, r, 0.5 * PI, PI);
        cr.close_path();
        let _ = cr.fill();
    }

    fn text(&mut self, x: f64, y: f64, text: &str, spans: &[Span]) {
        let attrs = pango::AttrList::new();
        for span in spans {
            text_attrs(&attrs, span.style, span.start as u32..span.end as u32);
        }
        self.layout.set_text(text);
        self.layout.set_attributes(Some(&attrs));
        gtk::render_layout(&self.style, self.cr, x, y, &self.layout);
    }

    fn glyph(&mut self, x: f64, y: f64, text: &str, style: Style) {
        let attrs = pango::AttrList::new();
        text_attrs(&attrs, style, 0..u32::MAX);
        self.layout.set_attributes(Some(&attrs));
        self.layout.set_text(text);
        let shift = self.view.baseline - self.layout.baseline() as f64 / pango::SCALE as f64;
        gtk::render_layout(&self.style, self.cr, x, y + shift, &self.layout);
    }

    fn shape(&mut self, shape: &Shape, rgb: u32) {
        let cr = self.cr;
        set_color(cr, rgb, 1.0);
        match shape {
            Shape::Fill(rects) => {
                for &(x, y, w, h) in rects {
                    cr.rectangle(x, y, w, h);
                }
                let _ = cr.fill();
            }
            &Shape::Shade((x, y, w, h), alpha) => {
                let _ = cr.save();
                cr.rectangle(x, y, w, h);
                cr.clip();
                let _ = cr.paint_with_alpha(alpha);
                let _ = cr.restore();
            }
            Shape::Stroke(width, path) => {
                cr.set_line_width(*width);
                for segment in path {
                    match *segment {
                        Segment::Move(x, y) => cr.move_to(x, y),
                        Segment::Line(x, y) => cr.line_to(x, y),
                        Segment::Curve(x1, y1, x2, y2, x3, y3) => {
                            cr.curve_to(x1, y1, x2, y2, x3, y3)
                        }
                    }
                }
                let _ = cr.stroke();
            }
        }
    }

    fn image(&mut self, slice: &Slice) {
        let Some(surface) = self.view.surface(slice.id, slice.image) else {
            return;
        };
        let cr = self.cr;
        let (left, top, sw, sh) = slice.image.source;
        cr.save().ok();
        cr.rectangle(slice.x, slice.y, slice.w, slice.h);
        cr.clip();
        cr.translate(slice.x, slice.y);
        cr.scale(slice.scale.0, slice.scale.1);
        cr.translate(-slice.from.0 - left as f64, -slice.from.1 - top as f64);
        cr.rectangle(left as f64, top as f64, sw as f64, sh as f64);
        cr.clip();
        let _ = cr.set_source_surface(&surface, 0.0, 0.0);
        // Smooth when enlarged, as kitty draws them.
        cr.source().set_filter(cairo::Filter::Bilinear);
        let _ = cr.paint();
        cr.restore().ok();
    }
}

/// Surfaces of the images in view, by number; `None` for those that could
/// not be decoded, not to try again.
pub(super) type Surfaces = HashMap<u32, Option<cairo::ImageSurface>>;

/// The image of a row of the view, and what it was drawn from.
pub(super) struct Row {
    line: Line,
    start: usize,
    marks: Marks,
    /// The width of a cell (the zoom changes the font), and of the view.
    cell: f64,
    width: i32,
    /// The transparent margin above and below the row in its image.
    margin: f64,
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

/// Adds the attributes of a style to bytes `range` of a layout's text.
fn text_attrs(list: &pango::AttrList, style: Style, range: std::ops::Range<u32>) {
    let (fg, attrs) = (style.fg, style.attrs);
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

pub(super) fn set_color(cr: &cairo::Context, rgb: u32, alpha: f64) {
    let c = |shift: u32| ((rgb >> shift) & 0xff) as f64 / 255.0;
    cr.set_source_rgba(c(16), c(8), c(0), alpha);
}
