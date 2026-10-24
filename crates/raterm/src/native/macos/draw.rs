//! Drawing the grid with Core Graphics and Core Text: the `Canvas` of macOS,
//! the font, and the `NSView` the grid is drawn in.
//!
//! To write on a Mac (see Spec-macOS): add the `objc2`, `objc2-foundation`,
//! `objc2-app-kit`, `objc2-core-graphics`, `objc2-core-text` and
//! `objc2-core-foundation` crates to the macOS dependencies of
//! `Cargo.toml`, the versions `tao` already pulls (`Cargo.lock`).

use tao::window::Window;

use crate::native::boxes::Shape;
use crate::native::paint::{Canvas, Slice, Span, Style};

/// What the grid's `drawRect:` calls: paint on this canvas a view of this
/// width and height, in points.
pub(super) type Draw = Box<dyn Fn(&mut CgCanvas, f64, f64)>;

/// The configured font, as Core Text knows it (`CTFont`).
pub(super) struct Font {}

impl Font {
    /// The font of `family` (a Core Text name, Menlo by default) at `size`
    /// points.
    pub(super) fn new(_family: &str, _size: f64) -> Font {
        todo!("macOS: CTFontCreateWithName, falling back to Menlo")
    }

    /// Width and height of a cell, in points: the advance of `X` (as the
    /// Linux view measures 100 of them), and the ascent, descent and leading
    /// rounded up to whole points.
    pub(super) fn cell(&self) -> (f64, f64) {
        todo!("macOS: CTFontGetAdvancesForGlyphs on X, CTFontGetAscent + Descent + Leading")
    }

    /// The baseline from the top of a cell, which glyphs of fallback fonts
    /// are drawn on.
    #[expect(dead_code, reason = "for CgCanvas, once written")]
    pub(super) fn baseline(&self) -> f64 {
        todo!("macOS: CTFontGetAscent")
    }

    /// Whether the font has its own glyph for `c`: rows of such chars are
    /// shaped as one text (`Canvas::text`), the others cell by cell.
    pub(super) fn has_glyph(&self, _c: char) -> bool {
        todo!("macOS: CTFontGetGlyphsForCharacters on the UTF-16 of c")
    }

    /// The font at another size, for the zoom.
    pub(super) fn sized(&self, _size: f64) -> Font {
        todo!("macOS: CTFontCreateCopyWithAttributes with the new size")
    }
}

/// The `NSView` the grid is drawn in: over the window's content view
/// (`tao::platform::macos::WindowExtMacOS::ns_view`), under the web views
/// of blocks, flipped (`isFlipped` is true) so that its origin is at the
/// top left, as the flow's coordinates.
pub(super) struct Grid {}

impl Grid {
    /// Makes the view, filling the window and resizing with it. Its
    /// `drawRect:` makes a `CgCanvas` on the current `CGContext` and calls
    /// `draw` with it and the view's size, in points.
    pub(super) fn new(_window: &Window, _draw: Draw) -> Grid {
        todo!(
            "macOS: an NSView subclass (objc2's define_class!) with isFlipped and drawRect:, \
             added to window.ns_view() with autoresizing; transparent when [window] opacity < 1"
        )
    }

    /// Asks AppKit to draw the view again.
    pub(super) fn invalidate(&self) {
        todo!("macOS: setNeedsDisplay(true)")
    }

    /// The view where the web views of blocks go, above the grid.
    pub(super) fn superview(&self) -> *mut std::ffi::c_void {
        todo!("macOS: the window's content view, which holds the grid and the blocks")
    }
}

/// Painting with Core Graphics, text with Core Text in the configured font.
pub(super) struct CgCanvas {}

impl Canvas for CgCanvas {
    fn fill(&mut self, _x: f64, _y: f64, _w: f64, _h: f64, _rgb: u32, _alpha: f64) {
        todo!("macOS: CGContextSetRGBFillColor, CGContextFillRect")
    }

    fn stroke_rect(&mut self, _x: f64, _y: f64, _w: f64, _h: f64, _rgb: u32, _width: f64) {
        todo!("macOS: CGContextSetRGBStrokeColor, CGContextSetLineWidth, CGContextStrokeRect")
    }

    fn fill_rounded(&mut self, _x: f64, _y: f64, _w: f64, _h: f64, _rgb: u32, _alpha: f64) {
        todo!("macOS: CGPathCreateWithRoundedRect, radius w / 2, then CGContextFillPath")
    }

    fn text(&mut self, _x: f64, _y: f64, _text: &str, _spans: &[Span]) {
        todo!(
            "macOS: a CFAttributedString of text, each span's color, bold (a bold copy of the \
             font), italic, underline, strikeout and dim (alpha 0.6), drawn as one CTLine at \
             its baseline: y + Font::baseline (the view is flipped: set the text matrix)"
        )
    }

    fn glyph(&mut self, _x: f64, _y: f64, _text: &str, _style: Style) {
        todo!(
            "macOS: as text, one glyph whose font Core Text falls back to, on the baseline of \
             the configured font: y + Font::baseline"
        )
    }

    fn shape(&mut self, _shape: &Shape, _rgb: u32) {
        todo!(
            "macOS: Fill: CGContextAddRect for each rectangle then CGContextFillPath; Shade: \
             the color at alpha in the rectangle; Stroke: the segments as \
             CGContextMoveToPoint, AddLineToPoint, AddCurveToPoint, then CGContextStrokePath"
        )
    }

    fn image(&mut self, slice: &Slice) {
        // What it takes: the image and its pixels, where it goes and how.
        let _ = (slice.id, slice.image, slice.x, slice.y, slice.w, slice.h);
        let _ = (slice.scale, slice.from);
        todo!(
            "macOS: decode slice.image.data once (Argb pixels: a CGImage from the bytes; Png: \
             ImageIO, PNG only), then clip to x, y, w, h, scale by slice.scale, and draw its \
             source rectangle so that pixel slice.from lands at x, y, interpolated"
        )
    }
}
