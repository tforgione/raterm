//! Drawing the grid with Core Graphics and Core Text: the `Canvas` of macOS,
//! the font, and the `NSView` the grid is drawn in.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSBitmapImageRep, NSGraphicsContext, NSView};
use objc2_core_foundation::{
    CFAttributedString, CFRetained, CFString, CGAffineTransform, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapInfo, CGColor, CGColorRenderingIntent, CGColorSpace, CGContext, CGDataProvider,
    CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality, CGPath,
};
use objc2_core_text::{
    CTFont, CTFontOrientation, CTFontSymbolicTraits, CTLine, kCTFontAttributeName,
    kCTForegroundColorAttributeName,
};
use objc2_foundation::{NSData, NSMutableAttributedString, NSPoint, NSRange, NSRect, NSString};
use raterm::graphics::{Data, Pixels};
use raterm::term::Attrs;
use tao::platform::macos::WindowExtMacOS;
use tao::window::Window;

use crate::native::boxes::{Segment, Shape};
use crate::native::paint::{Canvas, Slice, Span, Style};

/// What the grid's `drawRect:` calls: paint on this surface a view of this
/// width and height, in points.
pub(super) type Draw = Box<dyn Fn(Surface<'_>, f64, f64)>;

/// The configured font, as Core Text knows it, in its four styles.
#[derive(Clone)]
pub(super) struct Font {
    regular: CFRetained<CTFont>,
    bold: CFRetained<CTFont>,
    italic: CFRetained<CTFont>,
    bold_italic: CFRetained<CTFont>,
}

impl Font {
    /// The font of `family` (a Core Text name, Menlo by default) at `size`
    /// points.
    pub(super) fn new(family: &str, size: f64) -> Font {
        let family = if family == "monospace" {
            "Menlo"
        } else {
            family
        };
        let font =
            unsafe { CTFont::with_name(&CFString::from_str(family), size, std::ptr::null()) };
        // Core Text gives a font for any name, Helvetica if it knows none.
        let found = unsafe { font.family_name() }.to_string();
        let ps = unsafe { font.post_script_name() }.to_string();
        let font = if found.eq_ignore_ascii_case(family) || ps.eq_ignore_ascii_case(family) {
            font
        } else {
            eprintln!("raterm: font {family:?} not found, using Menlo");
            unsafe { CTFont::with_name(&CFString::from_str("Menlo"), size, std::ptr::null()) }
        };
        Font::styled(font)
    }

    fn styled(regular: CFRetained<CTFont>) -> Font {
        let with = |traits: CTFontSymbolicTraits| unsafe {
            regular
                .copy_with_symbolic_traits(0.0, std::ptr::null(), traits, traits)
                .unwrap_or_else(|| regular.clone())
        };
        let bold = with(CTFontSymbolicTraits::BoldTrait);
        let italic = with(CTFontSymbolicTraits::ItalicTrait);
        let bold_italic = with(CTFontSymbolicTraits::BoldTrait | CTFontSymbolicTraits::ItalicTrait);
        Font {
            regular,
            bold,
            italic,
            bold_italic,
        }
    }

    /// Width and height of a cell, in points: the advance of `X`, and the
    /// ascent, descent and leading rounded up to whole points.
    pub(super) fn cell(&self) -> (f64, f64) {
        let font = &self.regular;
        let width = self
            .glyph_of('X')
            .map_or(font_size(font) * 0.6, |mut glyph| unsafe {
                font.advances_for_glyphs(
                    CTFontOrientation::Horizontal,
                    NonNull::from(&mut glyph),
                    std::ptr::null_mut(),
                    1,
                )
            });
        let height = unsafe { font.ascent() + font.descent() + font.leading() };
        (width, height.ceil())
    }

    /// The baseline from the top of a cell, which every text is drawn on.
    pub(super) fn baseline(&self) -> f64 {
        let font = &self.regular;
        // The leading, rounded up with the cell, splits above and below.
        let (_, h) = self.cell();
        let natural = unsafe { font.ascent() + font.descent() };
        (unsafe { font.ascent() } + (h - natural) / 2.0).round()
    }

    fn glyph_of(&self, c: char) -> Option<u16> {
        let mut utf16 = [0u16; 2];
        let utf16 = c.encode_utf16(&mut utf16);
        let mut glyphs = [0u16; 2];
        let found = unsafe {
            self.regular.glyphs_for_characters(
                NonNull::new(utf16.as_mut_ptr()).unwrap(),
                NonNull::new(glyphs.as_mut_ptr()).unwrap(),
                utf16.len() as isize,
            )
        };
        found.then_some(glyphs[0])
    }

    /// Whether the font has its own glyph for `c`: rows of such chars are
    /// shaped as one text (`Canvas::text`), the others cell by cell.
    pub(super) fn has_glyph(&self, c: char) -> bool {
        self.glyph_of(c).is_some()
    }

    /// The font at another size, for the zoom.
    pub(super) fn sized(&self, size: f64) -> Font {
        let font = unsafe {
            self.regular
                .copy_with_attributes(size, std::ptr::null(), None)
        };
        Font::styled(font)
    }

    fn styled_for(&self, attrs: Attrs) -> &CTFont {
        match (attrs.has(Attrs::BOLD), attrs.has(Attrs::ITALIC)) {
            (false, false) => &self.regular,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (true, true) => &self.bold_italic,
        }
    }
}

fn font_size(font: &CTFont) -> f64 {
    unsafe { font.size() }
}

/// Images of the graphics protocols, decoded once, by number; `None` for
/// those that could not be. Those not drawn in a frame are dropped.
#[derive(Default)]
pub(super) struct Images {
    decoded: HashMap<u32, (*const Data, Option<CFRetained<CGImage>>)>,
    drawn: HashSet<u32>,
}

/// What the grid draws on, in its `drawRect:`.
pub(super) struct Surface<'a> {
    cg: &'a CGContext,
    images: &'a RefCell<Images>,
}

impl<'a> Surface<'a> {
    /// A canvas that draws text in `font`.
    pub(super) fn canvas<'b>(&'b self, font: &'b Font) -> CgCanvas<'b> {
        CgCanvas {
            cg: self.cg,
            images: self.images,
            baseline: font.baseline(),
            font,
        }
    }
}

pub(super) struct GridIvars {
    draw: Draw,
    images: RefCell<Images>,
}

define_class!(
    // The view the grid is drawn in, flipped so that its origin is at the top
    // left as the flow's coordinates; the web views of blocks are its
    // subviews. Mouse and keys go through it to tao's view.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RatermGrid"]
    #[ivars = GridIvars]
    pub(super) struct GridView;

    impl GridView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            false
        }

        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            let hit: *mut NSView = unsafe { msg_send![super(self), hitTest: point] };
            // Clicks on the grid itself go to tao's view, under it.
            if std::ptr::eq(hit, self as *const GridView as *const NSView) {
                std::ptr::null_mut()
            } else {
                hit
            }
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let Some(context) = NSGraphicsContext::currentContext() else {
                return;
            };
            let cg = context.CGContext();
            let bounds = self.bounds();
            let ivars = self.ivars();
            ivars.images.borrow_mut().drawn.clear();
            let surface = Surface {
                cg: &cg,
                images: &ivars.images,
            };
            (ivars.draw)(surface, bounds.size.width, bounds.size.height);
            let mut images = ivars.images.borrow_mut();
            let drawn = std::mem::take(&mut images.drawn);
            images.decoded.retain(|id, _| drawn.contains(id));
        }
    }
);

/// The grid's view, filling the window.
pub(super) struct Grid {
    view: Retained<GridView>,
}

impl Grid {
    /// Makes the view over the window's content view, filling it and
    /// resizing with it. Its `drawRect:` calls `draw`.
    pub(super) fn new(window: &Window, draw: Draw) -> Grid {
        let mtm = MainThreadMarker::new().expect("the view runs on the main thread");
        // SAFETY: tao's view lives as long as the window.
        let parent: &NSView = unsafe { &*(window.ns_view() as *const NSView) };
        let this = mtm.alloc::<GridView>().set_ivars(GridIvars {
            draw,
            images: RefCell::default(),
        });
        let view: Retained<GridView> =
            unsafe { msg_send![super(this), initWithFrame: parent.bounds()] };
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        parent.addSubview(&view);
        Grid { view }
    }

    /// Asks AppKit to draw the view again.
    pub(super) fn invalidate(&self) {
        self.view.setNeedsDisplay(true);
    }

    /// The view where the web views of blocks go, above the grid: the grid
    /// itself, whose coordinates are the flow's.
    pub(super) fn superview(&self) -> *mut c_void {
        Retained::as_ptr(&self.view) as *mut c_void
    }
}

/// Painting with Core Graphics, text with Core Text in the configured font.
pub(super) struct CgCanvas<'a> {
    cg: &'a CGContext,
    images: &'a RefCell<Images>,
    font: &'a Font,
    baseline: f64,
}

fn rgba(rgb: u32, alpha: f64) -> (f64, f64, f64, f64) {
    let c = |shift: u32| ((rgb >> shift) & 0xff) as f64 / 255.0;
    (c(16), c(8), c(0), alpha)
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

/// A Core Foundation object as an Objective-C one (they are toll-free
/// bridged).
fn as_object<T>(cf: &T) -> &AnyObject {
    unsafe { &*(cf as *const T as *const AnyObject) }
}

impl CgCanvas<'_> {
    fn set_fill(&self, rgb: u32, alpha: f64) {
        let (r, g, b, a) = rgba(rgb, alpha);
        CGContext::set_rgb_fill_color(Some(self.cg), r, g, b, a);
    }

    /// Draws `text` with its top at `y`, in the styles of `spans` (byte
    /// ranges), on the baseline of the configured font.
    fn line(&self, x: f64, y: f64, text: &str, spans: &[Span]) {
        let string = NSString::from_str(text);
        let attributed =
            NSMutableAttributedString::initWithString(NSMutableAttributedString::alloc(), &string);
        // Spans are in bytes, attributed strings in UTF-16 units.
        let utf16 = |byte: usize| text[..byte.min(text.len())].encode_utf16().count();
        let font_key: &NSString = unsafe { &*(kCTFontAttributeName as *const CFString).cast() };
        let color_key: &NSString =
            unsafe { &*(kCTForegroundColorAttributeName as *const CFString).cast() };
        let mut decorations = Vec::new();
        for span in spans {
            let (start, end) = (utf16(span.start), utf16(span.end));
            if start >= end {
                continue;
            }
            let range = NSRange::new(start, end - start);
            let attrs = span.style.attrs;
            let alpha = if attrs.has(Attrs::DIM) { 0.6 } else { 1.0 };
            let (r, g, b, a) = rgba(span.style.fg, alpha);
            let color = CGColor::new_srgb(r, g, b, a);
            unsafe {
                attributed.addAttribute_value_range(
                    font_key,
                    as_object(self.font.styled_for(attrs)),
                    range,
                );
                attributed.addAttribute_value_range(color_key, as_object(&*color), range);
            }
            if attrs.has(Attrs::UNDERLINE) || attrs.has(Attrs::STRIKE) {
                decorations.push((start, end, span.style));
            }
        }
        let cf: &CFAttributedString = unsafe { &*(Retained::as_ptr(&attributed) as *const _) };
        let line = unsafe { CTLine::with_attributed_string(cf) };
        let baseline = y + self.baseline;
        let cg = Some(self.cg);
        CGContext::save_g_state(cg);
        // The view is flipped: glyphs would be upside down.
        CGContext::set_text_matrix(
            cg,
            CGAffineTransform {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: -1.0,
                tx: 0.0,
                ty: 0.0,
            },
        );
        CGContext::set_text_position(cg, x, baseline);
        unsafe { line.draw(self.cg) };
        CGContext::restore_g_state(cg);
        // Core Text draws no strikeout: both lines are drawn here.
        let font = &self.font.regular;
        for (start, end, style) in decorations {
            let at = |i: usize| unsafe {
                line.offset_for_string_index(i as isize, std::ptr::null_mut())
            };
            let (x0, x1) = (x + at(start), x + at(end));
            let thickness = unsafe { font.underline_thickness() }.max(1.0);
            let alpha = if style.attrs.has(Attrs::DIM) {
                0.6
            } else {
                1.0
            };
            self.set_fill(style.fg, alpha);
            if style.attrs.has(Attrs::UNDERLINE) {
                let under = baseline - unsafe { font.underline_position() };
                CGContext::fill_rect(cg, rect(x0, under, x1 - x0, thickness));
            }
            if style.attrs.has(Attrs::STRIKE) {
                let strike = baseline - unsafe { font.x_height() } / 2.0;
                CGContext::fill_rect(cg, rect(x0, strike - thickness / 2.0, x1 - x0, thickness));
            }
        }
    }

    /// The image of a slice, decoded the first time it shows.
    fn decoded(&self, id: u32, data: &Arc<Data>) -> Option<CFRetained<CGImage>> {
        let mut images = self.images.borrow_mut();
        images.drawn.insert(id);
        let key = Arc::as_ptr(data);
        if let Some((at, image)) = images.decoded.get(&id)
            && *at == key
        {
            return image.clone();
        }
        let image = decode(data);
        if image.is_none() {
            eprintln!("raterm: image {id} could not be decoded");
        }
        images.decoded.insert(id, (key, image.clone()));
        image
    }
}

/// An image's pixels as a `CGImage`. PNG goes through `NSBitmapImageRep`
/// only once its signature is checked, so that no other decoder reads what
/// a program sent.
fn decode(data: &Data) -> Option<CFRetained<CGImage>> {
    match &data.pixels {
        Pixels::Argb(pixels) => {
            let bytes: Vec<u8> = pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
            let cf = objc2_core_foundation::CFData::from_bytes(&bytes);
            let provider = CGDataProvider::with_cf_data(Some(&cf))?;
            let space = CGColorSpace::new_device_rgb()?;
            // Premultiplied ARGB in native (little) endian: BGRA in memory.
            let info = CGBitmapInfo(
                CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0,
            );
            unsafe {
                CGImage::new(
                    data.width,
                    data.height,
                    8,
                    32,
                    data.width * 4,
                    Some(&space),
                    info,
                    Some(&provider),
                    std::ptr::null(),
                    true,
                    CGColorRenderingIntent::RenderingIntentDefault,
                )
            }
        }
        Pixels::Png(png) => {
            if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
                return None;
            }
            let rep = NSBitmapImageRep::initWithData(
                NSBitmapImageRep::alloc(),
                &NSData::with_bytes(png),
            )?;
            let image = rep.CGImage()?;
            // SAFETY: the same object, retained as Core Foundation does.
            Some(unsafe { CFRetained::retain(NonNull::from(&*image).cast::<CGImage>()) })
        }
    }
}

impl Canvas for CgCanvas<'_> {
    fn fill(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: u32, alpha: f64) {
        self.set_fill(rgb, alpha);
        CGContext::fill_rect(Some(self.cg), rect(x, y, w, h));
    }

    fn stroke_rect(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: u32, width: f64) {
        let (r, g, b, a) = rgba(rgb, 1.0);
        let cg = Some(self.cg);
        CGContext::set_rgb_stroke_color(cg, r, g, b, a);
        CGContext::stroke_rect_with_width(cg, rect(x, y, w, h), width);
    }

    fn fill_rounded(&mut self, x: f64, y: f64, w: f64, h: f64, rgb: u32, alpha: f64) {
        let r = (w / 2.0).min(h / 2.0);
        let path = unsafe { CGPath::with_rounded_rect(rect(x, y, w, h), r, r, std::ptr::null()) };
        self.set_fill(rgb, alpha);
        let cg = Some(self.cg);
        CGContext::add_path(cg, Some(&path));
        CGContext::fill_path(cg);
    }

    fn text(&mut self, x: f64, y: f64, text: &str, spans: &[Span]) {
        self.line(x, y, text, spans);
    }

    fn glyph(&mut self, x: f64, y: f64, text: &str, style: Style) {
        // Core Text falls back to another font for chars this one lacks.
        let span = Span {
            start: 0,
            end: text.len(),
            style,
        };
        self.line(x, y, text, &[span]);
    }

    fn shape(&mut self, shape: &Shape, rgb: u32) {
        let cg = Some(self.cg);
        self.set_fill(rgb, 1.0);
        match shape {
            Shape::Fill(rects) => {
                CGContext::begin_path(cg);
                for &(x, y, w, h) in rects {
                    CGContext::add_rect(cg, rect(x, y, w, h));
                }
                CGContext::fill_path(cg);
            }
            &Shape::Shade((x, y, w, h), alpha) => {
                self.set_fill(rgb, alpha);
                CGContext::fill_rect(cg, rect(x, y, w, h));
            }
            Shape::Stroke(width, path) => {
                let (r, g, b, a) = rgba(rgb, 1.0);
                CGContext::set_rgb_stroke_color(cg, r, g, b, a);
                CGContext::set_line_width(cg, *width);
                CGContext::begin_path(cg);
                for segment in path {
                    match *segment {
                        Segment::Move(x, y) => CGContext::move_to_point(cg, x, y),
                        Segment::Line(x, y) => CGContext::add_line_to_point(cg, x, y),
                        Segment::Curve(x1, y1, x2, y2, x3, y3) => {
                            CGContext::add_curve_to_point(cg, x1, y1, x2, y2, x3, y3)
                        }
                    }
                }
                CGContext::stroke_path(cg);
            }
        }
    }

    fn image(&mut self, slice: &Slice) {
        let Some(image) = self.decoded(slice.id, &slice.image.data) else {
            return;
        };
        let cg = Some(self.cg);
        let (left, top, sw, sh) = slice.image.source;
        let (iw, ih) = (
            slice.image.data.width as f64,
            slice.image.data.height as f64,
        );
        CGContext::save_g_state(cg);
        CGContext::clip_to_rect(cg, rect(slice.x, slice.y, slice.w, slice.h));
        CGContext::translate_ctm(cg, slice.x, slice.y);
        CGContext::scale_ctm(cg, slice.scale.0, slice.scale.1);
        CGContext::translate_ctm(cg, -slice.from.0 - left as f64, -slice.from.1 - top as f64);
        CGContext::clip_to_rect(cg, rect(left as f64, top as f64, sw as f64, sh as f64));
        // Smooth when enlarged, as kitty draws them.
        CGContext::set_interpolation_quality(cg, CGInterpolationQuality::High);
        // Images are drawn bottom up: flipped back for the flipped view.
        CGContext::translate_ctm(cg, 0.0, ih);
        CGContext::scale_ctm(cg, 1.0, -1.0);
        CGContext::draw_image(cg, rect(0.0, 0.0, iw, ih), Some(&image));
        CGContext::restore_g_state(cg);
    }
}
